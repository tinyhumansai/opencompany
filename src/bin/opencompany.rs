use std::path::PathBuf;

use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use opencompany::company::Schedule;
use opencompany::runtime::lifecycle_scheduler::load_or_create_cutoff_millis;
use opencompany::runtime::{
    CompanyScheduler, LifecycleScheduler, MaintenanceTicker, SystemClock, WorkflowScheduler,
};
use opencompany::{
    AppConfig, AppState, CompanyId, CompanyManifest, Result,
    app::config::{ConfigFile, ProcessEnv, resolve},
    app::doctor,
    openhuman::{LaunchMode, OpenHumanLaunch},
    runtime::RuntimeBuilder,
};
use tokio::sync::Notify;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the Axum HTTP server.
    Serve {
        /// Address to bind. Falls back to `OPENCOMPANY_BIND`, then the
        /// `bind` key of `config.toml`, then `127.0.0.1:8080`.
        #[arg(long)]
        bind: Option<String>,
        /// Optional OpenHuman checkout path to report in `/spec`.
        #[arg(long)]
        openhuman_root: Option<PathBuf>,
        /// A company to load and register at boot (a manifest file or a
        /// directory containing one). Repeatable for multi-company hosting.
        #[arg(long = "company", value_name = "DIR")]
        companies: Vec<PathBuf>,
        /// OpenCompany home holding company bundles. Defaults to
        /// `$HOME/.opencompany`, with bundles under `companies/<slug>`.
        #[arg(long)]
        home: Option<PathBuf>,
        /// Opt every loaded company into going public on tiny.place, regardless
        /// of each manifest's `[place].discoverable`. Requires the `tinyplace`
        /// feature to actually reach the network; without it the flag only marks
        /// companies discoverable for the local A2A routes.
        #[arg(long)]
        discoverable: bool,
    },
    /// Print a JSON runtime specification.
    Spec {
        /// Optional OpenHuman checkout path to report.
        #[arg(long)]
        openhuman_root: Option<PathBuf>,
    },
    /// Validate a company manifest and print its effective configuration.
    Check {
        /// Manifest file or a directory containing `company.toml`/`agents.toml`.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Print the system prompt each of a company's agents would be built with.
    ///
    /// A brief (`agents/prompts/*.md`, an inline `prompt`, a routed `context`
    /// entry) is the most editable thing in a bundle and used to be the least
    /// inspectable: seeing it meant running the company and reading a provider
    /// trace. This renders the same composition from the manifest alone, names
    /// every section's origin, and says plainly which sections need a live
    /// runtime instead of guessing at them.
    ///
    /// Build with `--features openhuman` to include the harness's own tool
    /// briefs (workspace, ledgers, deliverables, delegation); the default build
    /// renders the persona and the checked-in briefs and reports the rest as
    /// deferred. `scripts/dump-prompt.sh` is the wrapper that gets the feature
    /// flag right.
    Prompt {
        /// Company bundle directory, or a manifest file.
        #[arg(long = "company", value_name = "DIR", default_value = ".")]
        company: PathBuf,
        /// Only this agent id. Repeat for several.
        #[arg(long = "agent", value_name = "ID")]
        agents: Vec<String>,
        /// Print the prompt body verbatim, with no report around it — the bytes
        /// to diff against a provider trace. Requires exactly one agent.
        #[arg(long)]
        raw: bool,
        /// Print the report as JSON instead of Markdown.
        #[arg(long)]
        json: bool,
        /// Write one `<agent-id>.prompt.md` per agent into this directory
        /// instead of printing.
        #[arg(long = "out", value_name = "DIR")]
        out: Option<PathBuf>,
    },
    /// Report the effective runtime configuration, which layer set each value,
    /// and what is missing per optional capability.
    Doctor {
        /// Optional company manifest whose `[brain].mode` participates in
        /// resolution. Defaults to a synthetic manifest when omitted.
        #[arg(long = "company", value_name = "DIR")]
        company: Option<PathBuf>,
        /// Print the report as JSON instead of aligned text.
        #[arg(long)]
        json: bool,
    },
    /// Issue a sign-in password for a company, from the host (#1718).
    ///
    /// The way in when a deployment cannot mail a sign-in link: the magic-link
    /// code is minted and stored hashed, every admin route needs an admin that
    /// does not exist yet, and the console's own advice — "an admin can issue
    /// you one" — has nobody to ask on a first boot.
    ///
    /// Only for an address the company ALREADY admits: named in the manifest's
    /// `[users] admins`, or injected as the deployment's bootstrap admin
    /// (`OPENCOMPANY_ADMIN_EMAIL`). It makes a standing grant usable without
    /// mail; it does not create one.
    IssuePassword {
        /// The company id, as `serve` registers it. In shared-database mode
        /// this is the namespaced `<tenant>--<id>` form.
        #[arg(long)]
        company: String,
        /// The address to issue for.
        #[arg(long)]
        email: String,
        /// The password. Omit to read it from stdin, which keeps it out of
        /// shell history and out of `ps` — argv is world-readable for the
        /// lifetime of the exec, and this is a credential.
        #[arg(long)]
        password: Option<String>,
        /// Do not require the holder to replace this password on first use.
        ///
        /// The default requires a change, matching an admin-issued temporary
        /// password: whoever runs this knows the value, and usually conveys it
        /// over a channel they do not control. Pass this when the operator and
        /// the holder are the same person.
        #[arg(long)]
        no_change_required: bool,
        /// Data root, for backends that resolve one. Defaults the same way
        /// `serve` does.
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Report companies whose durable owner row is missing, and owner rows
    /// naming no company (issue #1077).
    ///
    /// Read-only. A company with no owner row is unreachable by its own tenant
    /// — every tenant-scoped request for it answers 403 — and nothing else in
    /// the product will tell you it exists. Repairing one is deliberately not
    /// offered: adopting it means guessing its tenant, and a wrong guess hands
    /// one tenant's company to another.
    ///
    /// Separate from `doctor` on purpose: `doctor` explains configuration and
    /// needs no database, and making it open storage would leave it unable to
    /// answer at all when the backend is the thing that is broken.
    Orphans {
        /// Data root, for backends that resolve one. Defaults the same way
        /// `serve` does.
        #[arg(long)]
        home: Option<PathBuf>,
        /// Print the report as JSON instead of aligned text.
        #[arg(long)]
        json: bool,
    },
    /// Export a company's bundle: read everything through the storage ports —
    /// the env-selected backend (`OPENCOMPANY_STORAGE`) with the env-selected
    /// memory engine (`OPENCOMPANY_MEMORY*`) overlaid, so the bundle captures
    /// what the deployment actually remembers, operator facts included — and
    /// write the canonical filesystem layout. With `--features export` the
    /// output is a single `.tar`; otherwise an unpacked bundle directory.
    /// Secrets and keys are excluded unless `--include-secrets` is set.
    Export {
        /// Company id (slug) to export.
        company: String,
        /// Output path (`<slug>.tar` under `--features export`, else a bundle
        /// directory). Defaults to the current directory.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Include the fs-only `secrets/` and `keys/` directories.
        #[arg(long)]
        include_secrets: bool,
        /// OpenCompany home holding company bundles. Defaults to
        /// `$HOME/.opencompany`, with bundles under `companies/<slug>`.
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Import a company bundle (a `.tar` under `--features export`, else an
    /// unpacked bundle directory) through the storage ports — the same
    /// env-selected backend + memory engine an export reads, so a bundle
    /// lands on whatever this deployment actually runs.
    Import {
        /// Bundle `.tar` or unpacked bundle directory to import.
        path: PathBuf,
        /// OpenCompany home to import into. Defaults to
        /// `$HOME/.opencompany`, with bundles under `companies/<slug>`.
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Memory-engine operations: today, migrating every record from the
    /// env-selected engine into another one — the data half of the
    /// engine-switch runbook (`docs/spec/runtime/memory-engine.md`).
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Launch a sibling OpenHuman checkout: the core binary (`--mode core`)
    /// or the Tauri desktop host (`--mode desktop`). Desktop calls `cargo tauri`
    /// directly and performs the preflight OpenHuman's own scripts do — install
    /// the vendored CEF-aware `tauri-cli`, pin `CEF_PATH`, load `<root>/.env`,
    /// and on macOS seed the Chromium keychain + signing identity (CEF on macOS,
    /// `wry` on Linux/Windows; Tauri still drives the Vite dev server). Pass
    /// `--dry-run` to preview.
    OpenHuman {
        /// OpenHuman checkout path.
        #[arg(long, default_value = "vendor/openhuman")]
        root: PathBuf,
        /// Launch target.
        #[arg(long, value_enum, default_value_t = ModeArg::Core)]
        mode: ModeArg,
        /// Build a release bundle instead of launching a dev session
        /// (`cargo run --release` for core; `cargo tauri build` for desktop —
        /// a signed `.app`/dmg on macOS, a deb/AppImage elsewhere).
        #[arg(long)]
        release: bool,
        /// Print the command without executing it.
        #[arg(long)]
        dry_run: bool,
        /// Arguments passed after `--` to the OpenHuman core binary. Ignored
        /// (and rejected) in desktop mode, which drives fixed pnpm scripts.
        #[arg(last = true)]
        args: Vec<String>,
    },
}

/// The `memory` subcommands.
#[derive(clap::Subcommand)]
enum MemoryCmd {
    /// Copy every record from the env-selected memory engine (the FROM side —
    /// `OPENCOMPANY_MEMORY*`, exactly what a boot would bind today) into
    /// another engine, over the contract's Portability family. Namespaces,
    /// record kinds and provenance taint round-trip untouched. Run it BEFORE
    /// flipping the environment: migrate, then set the variables, restart,
    /// and verify `/spec`.
    Migrate {
        /// Target driver: `namespace`, `supermemory`, `mem0`, or `cognee`.
        #[arg(long)]
        to: String,
        /// Target endpoint (hosted engines only).
        #[arg(long)]
        to_url: Option<String>,
        /// Target credential (hosted engines only).
        #[arg(long)]
        to_api_key: Option<String>,
        /// Records per page.
        #[arg(long, default_value_t = 500)]
        page_size: usize,
        /// Count what would move without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Re-enter a stopped migration at the cursor it printed.
        #[arg(long)]
        resume_cursor: Option<String>,
    },
}

impl std::fmt::Debug for MemoryCmd {
    /// Renders the target identity and never the credential — the same
    /// `<set>` convention as `StorageSettings`' manual impl, because the
    /// parent `Command` derives `Debug` and a derived impl here would carry
    /// the key into any future `{:?}` of the parsed CLI.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Migrate {
                to,
                to_url,
                to_api_key,
                page_size,
                dry_run,
                resume_cursor,
            } => f
                .debug_struct("Migrate")
                .field("to", to)
                .field("to_url", &to_url.as_ref().map(|_| "<set>"))
                .field("to_api_key", &to_api_key.as_ref().map(|_| "<set>"))
                .field("page_size", page_size)
                .field("dry_run", dry_run)
                .field("resume_cursor", &resume_cursor.is_some())
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ModeArg {
    Core,
    Desktop,
}

impl From<ModeArg> for LaunchMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Core => LaunchMode::Core,
            ModeArg::Desktop => LaunchMode::Desktop,
        }
    }
}

/// Resolves a `--company` argument to its source *directory*. `--company`
/// accepts either the company directory (`companies/<name>`) or the manifest
/// file inside it (`companies/<name>/company.toml`); the file form is normalized
/// to its parent so workspace seeding and the skill/workflow read resolvers look
/// under the company directory rather than under `company.toml/…`.
/// `opencompany prompt`: render each agent's composed system prompt.
///
/// Selection is by id and is **fail-loud** — a `--agent` naming nobody is an
/// error listing the roster, not an empty report. A typo'd id that printed
/// nothing would read exactly like an agent whose prompt is empty, which is the
/// one thing this command exists to distinguish.
fn run_prompt(
    company: &std::path::Path,
    agents: &[String],
    raw: bool,
    json: bool,
    out: Option<&std::path::Path>,
) -> Result<()> {
    let manifest = CompanyManifest::from_path(company)?;
    let all = opencompany::company::prompt_dump::dump(&manifest);

    let selected: Vec<_> = if agents.is_empty() {
        all
    } else {
        for wanted in agents {
            if !all.iter().any(|agent| &agent.agent_id == wanted) {
                let roster: Vec<&str> = all.iter().map(|a| a.agent_id.as_str()).collect();
                return Err(opencompany::error::OpenCompanyError::Config(format!(
                    "no agent `{wanted}` in {} — the roster is {roster:?}",
                    company.display()
                )));
            }
        }
        all.into_iter()
            .filter(|agent| agents.contains(&agent.agent_id))
            .collect()
    };

    if raw {
        // One agent, because raw output has no framing to say whose prompt is
        // whose: concatenating two would produce a document that looks like one
        // agent's prompt and is not.
        let [agent] = &selected[..] else {
            return Err(opencompany::error::OpenCompanyError::Config(format!(
                "`--raw` prints one prompt with no framing around it, so it needs exactly one \
                 `--agent` (got {})",
                selected.len()
            )));
        };
        print!("{}", agent.body());
        return Ok(());
    }

    if let Some(dir) = out {
        std::fs::create_dir_all(dir)?;
        for agent in &selected {
            let path = dir.join(format!("{}.prompt.md", agent.agent_id));
            std::fs::write(&path, agent.to_markdown())?;
            println!("wrote {}", path.display());
        }
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&selected)?);
        return Ok(());
    }

    for (index, agent) in selected.iter().enumerate() {
        if index > 0 {
            println!("---\n");
        }
        print!("{}", agent.to_markdown());
    }
    Ok(())
}

fn company_source_dir(path: &std::path::Path) -> std::path::PathBuf {
    if path.is_file() {
        path.parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

/// Loads the manifest under `dir`, builds a runtime over `home`, and registers
/// it in `state`. Returns the derived company id and display name.
///
/// Uses [`CompanyManifest::from_path_for_reload`], not `from_path`: this runs
/// on every `serve` boot for every company directory, hosted-tenant restarts
/// included, so it must not refuse an already-running company over a
/// validation rule (e.g. `RESERVED_AGENT_IDS`) that tightened after that
/// company's `company.toml` was written — see that method's doc comment
/// (issue #1781 review, Codex P1).
async fn register_company(
    state: &AppState,
    home: &std::path::Path,
    dir: &std::path::Path,
    discoverable: bool,
) -> Result<(String, String, Vec<Schedule>)> {
    let mut manifest = CompanyManifest::from_path_for_reload(dir)?;
    // `serve --discoverable` opts this company into going public regardless of
    // its manifest: mark it discoverable and synthesize a @handle when absent so
    // Agent Card generation and validation succeed.
    if discoverable {
        manifest.place.discoverable = true;
        if manifest.company.handle.is_none() {
            let handle = opencompany::runtime::company_id_from_name(&manifest.company.name)
                .as_ref()
                .to_string();
            manifest.company.handle = Some(handle);
        }
    }
    let name = manifest.company.name.clone();
    // Capture the schedules before the manifest is moved into the builder; boot
    // uses them to start this company's cron scheduler (lifecycle step 4).
    let schedules = manifest.schedules.clone();
    // The company's on-disk source directory (`companies/<name>`) seeds the
    // workspace tree on first boot and lets read resolvers find its committed
    // skills/workflows content.
    let source_dir = company_source_dir(dir);
    // Issue #85: this company was seeded from a template directory, so record
    // that directory's slug as the durable source-template provenance. The
    // builder stamps it only on first launch and carries it forward on rebuilds;
    // a raw-manifest `POST /api/v1/companies` provision has no template dir and
    // records no provenance.
    let provenance = source_dir.file_name().and_then(|s| s.to_str()).map(|slug| {
        opencompany::ports::types::TemplateProvenance {
            source_id: slug.to_string(),
            version: None,
            // Record only the template directory's basename, never the raw
            // absolute host path: `path` is exposed verbatim on the GraphQL and
            // REST provenance surfaces, and the absolute source dir would leak
            // the host filesystem layout + username. `slug` is already the final
            // path component (the `file_name()` guard above makes this `None`
            // when there is no basename, e.g. `serve --company .`).
            path: Some(slug.to_string()),
        }
    });
    // Shared-single-DB mode: namespace the derived id with this tenant so the
    // same boot template (`OPENCOMPANY_COMPANY`) does not collide across tenants
    // in one logical database. A no-op when `tenant_namespace` is unset.
    let derived = opencompany::runtime::company_id_from_name(&name);
    let company_id = state.config().namespaced_company_id(derived);
    let mut builder = company_builder(
        state,
        home,
        manifest,
        &company_id,
        Some(source_dir.clone()),
        discoverable,
    )?;
    if let Some(provenance) = provenance {
        builder = builder.with_template_provenance(provenance);
    }
    let runtime = builder.build().await?;
    // A company with no sign-in on a host anyone can reach is an unauthenticated
    // admin console, not a desktop app. `none` mode's whole premise is that the
    // only caller is the person at the machine, so a routable bind contradicts
    // it — and the contradiction is silent, because the host does start and does
    // serve. Refuse at boot, where somebody is looking, exactly as a
    // selected-but-unavailable storage backend does.
    if !runtime.auth_mode().has_login() && !state.config().is_local_only() {
        return Err(opencompany::error::OpenCompanyError::Config(format!(
            "company `{}` is configured with `[users].mode = \"none\"`, which has no sign-in, \
             but this host binds `{}` and would serve it to anyone who can reach that address. \
             Bind loopback, or choose `email` or `wallet`.",
            runtime.id().as_ref(),
            state.config().bind,
        )));
    }
    let company_id = runtime.id().clone();
    let id = company_id.as_ref().to_string();
    // Record boot-company ownership so a shared-DB manager can later purge by
    // tenant. Only meaningful in tenant-namespace mode; otherwise skipped so
    // db-per-tenant / self-hosted deployments keep their in-memory-only stub.
    if let Some(tenant) = state.config().tenant_namespace.clone() {
        // Canonical (bare-slug) form so the persisted `owners` row matches what
        // tenant-scoped auth compares a `tenant:acme` claim against.
        let tenant = opencompany::app::canonical_tenant(&tenant).to_string();
        state.set_owner(company_id.clone(), tenant.clone());
        if let Some(ownership) = state.stores().and_then(|s| s.ownership.clone())
            && let Err(err) = ownership.set_owner(&company_id, &tenant).await
        {
            eprintln!("failed to persist ownership for `{id}`: {err}");
        }
    }
    // Issue #290: stash what a later in-place rebuild cannot recover any other
    // way. `--discoverable` is the case that forces this to exist: it lives only
    // in the `serve` stack frame and mutates the manifest before the build.
    state.set_boot_inputs(
        company_id.clone(),
        opencompany::runtime::BootInputs {
            source_dir: Some(source_dir),
            discoverable,
        },
    );
    state.registry().insert(company_id, Arc::new(runtime));
    Ok((id, name, schedules))
}

/// Whether `OPENCOMPANY_SKIP_ACTIVATION_GATE` enables the account-activation
/// funnel's blocking-gate bypass (issue #1844) for a company that has never
/// booted before.
///
/// Pulled out of [`company_builder`]'s inline `== Ok("1")` comparison so the
/// exact-match contract — only the literal `"1"` enables it; unset, `"true"`,
/// and `"0"` all stay disabled — is a unit test instead of something only a
/// full e2e boot exercises. The one setter today is the e2e host script,
/// which sets it so its shared fixture company does not gate ~100 unrelated
/// specs that know nothing about the funnel; every other value (including a
/// truthy-looking `"true"`) staying disabled means a typo in that script
/// fails closed onto the real gate rather than silently bypassing it.
fn activation_gate_bypass_enabled(value: Option<&str>) -> bool {
    value == Some("1")
}

/// Assembles the `RuntimeBuilder` for one company with this host's full boot
/// wiring: the OpenHuman RPC transport, the harness pool and its managed
/// backends, feedback routing, the opened storage backend and memory overlay,
/// the shared skill library, and the manager-injected per-tenant mailbox.
///
/// Extracted from [`register_company`] so an in-place rebuild (issue #290) runs
/// the *same* wiring boot ran. A rebuild that assembled its own would drift from
/// boot silently, and the first symptom would be a company that came back from a
/// rebuild missing a capability nobody changed.
fn company_builder(
    state: &AppState,
    home: &std::path::Path,
    manifest: CompanyManifest,
    company_id: &CompanyId,
    source_dir: Option<PathBuf>,
    discoverable: bool,
) -> Result<RuntimeBuilder> {
    let mut builder = attach_tinyhumans_feedback(
        attach_harness(attach_openhuman(RuntimeBuilder::new(
            home.to_path_buf(),
            manifest,
        ))),
        state.config(),
    )
    .with_tinyplace_api_url(state.config().tinyplace_api_url.clone())
    // Install-wide MCP defaults (issue #527): every company built on this
    // instance gets them, which is what makes a fresh install useful with no
    // per-company setup. Already normalized when the config resolved.
    .with_default_mcp_servers(state.config().default_mcp_servers.clone())
    .with_host_base_url(state.config().host_base_url())
    .with_workspace_quota(state.config().workspace_quota)
    .with_workspace_git_enabled(state.config().workspace_git_enabled)
    // Issue #752: the backend that serves this host's secrets, which the
    // repository-credential gates refuse on. Threaded through `company_builder`
    // rather than read from the environment further down, so a rebuild gets the
    // same answer boot got.
    .with_storage_kind(state.storage_kind())
    // Issue #661 / M8: the deployment's standing bootstrap admin, normalized,
    // so a fresh tenant's `owner` report reaches its creator before that first
    // sign-in mints a user record. `None` (self-hosted, no injected address) is
    // a no-op. BootRebuilder reuses this builder, so the grant survives rebuild.
    .with_bootstrap_admin(state.config().bootstrap_admin())
    // How humans sign in, when the host names it for every company it serves
    // (`OPENCOMPANY_AUTH_MODE` / `config.toml`). `None` leaves each manifest's
    // `[users].mode` to answer, which is the normal case.
    .with_auth_mode_override(state.auth_mode_override())
    // Issue #1739. Process-wide and inherited from the state, so boot, the
    // in-place rebuilder and provisioning all get the same one — a
    // `NullTracker` in every build but a hosted tenant's.
    .with_analytics(state.analytics())
    .with_skills_registry(state.shared_skill_registry()?)
    // The setup cards a real operator should find waiting on a real board. Turned
    // on here rather than inferred from the seed directory, so a test or a
    // fixture that builds a company gets the empty board it is asserting about —
    // see `RuntimeBuilder::with_task_seeding`. First boot only.
    .with_task_seeding(true)
    // Issue #1844: `OPENCOMPANY_SKIP_ACTIVATION_GATE=1` skips the
    // account-activation funnel's blocking gate for a company that has never
    // booted before — see `RuntimeBuilder::skip_activation_gate`'s own doc
    // comment. Absent (the default) is a no-op; the one setter today is the
    // e2e host script, which sets it so its shared fixture company does not
    // gate ~100 unrelated specs that know nothing about the funnel.
    .skip_activation_gate(activation_gate_bypass_enabled(
        std::env::var("OPENCOMPANY_SKIP_ACTIVATION_GATE")
            .ok()
            .as_deref(),
    ))
    .with_id(company_id.clone());
    if let Some(source_dir) = source_dir {
        builder = builder.with_seed_dir(source_dir);
    }
    if let Some(stores) = state.stores() {
        builder = builder.with_stores(stores);
    }
    if let Some(overlay) = state.memory_overlay() {
        builder = builder.with_memory_overlay(&overlay);
    } else {
        // The engine is (now) the base backend. On a live rebuild this must
        // clear the outgoing provider engine's ports rather than inherit them —
        // a company switched to `store` must not keep reading the provider it
        // just deselected. See `RuntimeBuilder::with_memory_overlay_cleared`.
        builder = builder.with_memory_overlay_cleared();
    }
    #[cfg(feature = "smtp")]
    if let Ok(Some(cfg)) = opencompany::server::ops::mailer::TenantMailboxConfig::from_env() {
        // Same guard as `spawn_mailbox_poller`: the injected mailbox belongs to
        // exactly one company (the one whose id matches its local-part). In a
        // multi-company process, wiring it to every company would make every
        // one of them send outbound mail from the same injected address.
        let mailbox_slug = opencompany::server::ops::smtp::local_part(&cfg.address);
        if company_id.as_ref() == mailbox_slug {
            builder = builder.with_mail(opencompany::company::runtime::CompanyMail {
                sender: std::sync::Arc::new(opencompany::server::ops::smtp::LettreMailSender),
                smtp: cfg.smtp.clone(),
            });
        }
    }
    if discoverable {
        builder = builder.with_discoverable(true);
    }
    Ok(builder)
}

/// This host's in-place runtime rebuilder (issue #290).
///
/// Lives in the binary because [`company_builder`] does: the harness pool, the
/// OpenHuman transport and the managed media/search backends are assembled here
/// from the process environment and feature flags, and a rebuild that did not
/// reuse that assembly would quietly produce a differently-wired company.
struct BootRebuilder;

#[async_trait::async_trait]
impl opencompany::runtime::RuntimeRebuilder for BootRebuilder {
    async fn rebuild(
        &self,
        state: &AppState,
        request: opencompany::runtime::RebuildRequest,
    ) -> Result<opencompany::CompanyRuntime> {
        company_builder(
            state,
            state.home(),
            request.manifest,
            &request.id,
            request.boot.source_dir,
            request.boot.discoverable,
        )?
        // The whole point: the successor adopts the live journal, approval gate,
        // grant set, stores, harness pool, MCP runtime and serialising mutexes
        // rather than constructing a second copy of any of them.
        .with_handover(request.handover)
        .build()
        .await
    }
}

/// Starts a company's cron scheduler as a background task, if it has schedules.
///
/// A schedule whose cron fails to parse (which `opencompany check` does not
/// catch beyond field count) logs a warning and is skipped rather than aborting
/// boot. The returned handle is held by the caller and stops when `shutdown`
/// fires.
fn spawn_scheduler(
    state: &AppState,
    id: &str,
    schedules: &[Schedule],
    shutdown: &Arc<Notify>,
) -> Option<tokio::task::JoinHandle<()>> {
    // **This early return was issue #971.** Until the maintenance ticker
    // existed, sweeping expired approvals, expired grants and stale fire claims
    // rode this scheduler's minute loop — so a company with no `[[schedule]]`
    // returned here, spawned nothing, and swept nothing, forever, at any age.
    // The tenant that reported it drove everything through *workflow* schedules,
    // which run on a different loop that never called maintenance.
    //
    // It is correct now, and only because maintenance no longer depends on it:
    // `spawn_maintenance_ticker` is process-wide and always on. Nothing that
    // has to happen for every company may be attached below this line.
    if schedules.is_empty() {
        return None;
    }
    let runtime = state.registry().get(&CompanyId::new(id))?;
    match CompanyScheduler::new(runtime, schedules, Arc::new(SystemClock)) {
        // Follow the registry so an in-place rebuild (issue #290) reaches cron.
        // Without this the scheduler keeps driving the runtime it snapshotted —
        // which after a rebuild is the replaced, quiesced one, i.e. exactly the
        // "scheduled workflows never fire" surface #266 was reported against.
        Ok(scheduler) => Some(
            scheduler
                .following(state.registry().clone())
                .spawn(shutdown.clone()),
        ),
        Err(err) => {
            eprintln!("skipping scheduler for `{id}`: {err}");
            None
        }
    }
}

/// Starts the process-wide workflow scheduler: one task that fires every saved
/// workflow whose `trigger` node carries a cron (issue #169).
///
/// Deliberately NOT per company, unlike [`spawn_scheduler`]. Workflow schedules
/// are runtime data — creating a workflow in the console adds a cron with no
/// reboot, and a hosted tenant can be registered after boot — so the scheduler
/// re-reads the registry on every tick instead of snapshotting a company's
/// schedules at registration time.
fn spawn_workflow_scheduler(
    state: &AppState,
    shutdown: &Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    WorkflowScheduler::new(state.registry().clone(), Arc::new(SystemClock)).spawn(shutdown.clone())
}

/// Starts the process-wide maintenance ticker: one task that retires overdue
/// approvals, expired grants and stale fire claims for every registered company
/// (issue #971).
///
/// Process-wide for the same reason as [`spawn_workflow_scheduler`], and it is
/// the whole fix. The per-company [`spawn_scheduler`] above has to be reached at
/// every place a company can come into existence — a `--company` flag, adoption
/// of an existing data root, a hosted tenant registered after boot — and it
/// declines to start at all for a company with no manifest cron. Maintenance
/// must happen for every company unconditionally, so it hangs off the registry
/// and is started once, here, whether or not any company is loaded yet.
fn spawn_maintenance_ticker(
    state: &AppState,
    shutdown: &Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    MaintenanceTicker::new(state.registry().clone(), Arc::new(SystemClock))
        .with_evictor(Arc::new(
            opencompany::server::provision::RegistryEvictor::new(state.clone()),
        ))
        .spawn(shutdown.clone())
}

/// Starts the process-wide week-1 nudge scheduler (issue #1845): one daily
/// task that emails + files an in-app notification for a signup who hit
/// their day-7 boundary without saving a workflow.
///
/// Process-wide for the same reason [`spawn_workflow_scheduler`] and
/// [`spawn_maintenance_ticker`] are: it re-reads the registry every tick, so
/// a company registered after boot is covered without a restart.
///
/// The mail sender/credentials are read from `state.connections()`, already
/// resolved by [`connections_runtime`] before this is called — `None` in the
/// default build (no `smtp` feature) or when `OPENCOMPANY_MAIL_*` is unset,
/// which the scheduler treats as "degrade to in-app only", never a reason to
/// skip spawning. `cutoff_millis` comes from
/// [`load_or_create_cutoff_millis`] against `state.home()` — pinned the
/// first time this ever runs on this data root, not re-stamped on every
/// boot, so a restart does not move the "signed up after deploy" instant
/// issue #1845 scopes the nudge to. See that function's docs for why a
/// re-stamped cutoff would permanently disqualify eligible signups.
fn spawn_lifecycle_scheduler(
    state: &AppState,
    shutdown: &Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    let connections = state.connections();
    LifecycleScheduler::new(
        state.registry().clone(),
        Arc::new(SystemClock),
        connections.mail.clone(),
        connections.mail_credentials.clone(),
        state.config().host_base_url(),
        load_or_create_cutoff_millis(state.home()),
    )
    .spawn(shutdown.clone())
}

/// Starts the process-wide presence sweep (issue: "Bound client-supplied
/// console leases"). See [`opencompany::server::presence::PresenceSweeper`]
/// for why this is a separate task from the maintenance ticker above rather
/// than folded into it: presence is host-global, not scoped to a registered
/// company.
fn spawn_presence_sweeper(state: &AppState, shutdown: &Arc<Notify>) -> tokio::task::JoinHandle<()> {
    opencompany::server::presence::PresenceSweeper::new(state.presence_handle())
        .spawn(shutdown.clone())
}

/// Starts a company's IMAP mailbox poller as a background task, if the
/// platform injected mailbox credentials for this tenant.
///
/// Mirrors [`spawn_scheduler`]: reads [`TenantMailboxConfig::from_env`]
/// (`Ok(None)` means the manager did not wire mail for this tenant — a no-op,
/// not an error; `Err` logs and skips rather than aborting boot). The actual
/// IMAP transport only exists when the crate is built with the `imap`
/// feature, so the poll itself is feature-gated; without the feature this
/// still validates the env (surfacing config typos) but starts nothing.
fn spawn_mailbox_poller(
    state: &AppState,
    id: &str,
    shutdown: &Arc<Notify>,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let cfg = match opencompany::server::ops::mailer::TenantMailboxConfig::from_env() {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return,
        Err(err) => {
            eprintln!("mailbox config error: {err}");
            return;
        }
    };
    // The injected mailbox belongs to exactly one company: the one whose id
    // equals the mailbox address's local-part. In a multi-company process
    // every OTHER registered company must skip this poller entirely, or
    // inbound mail addressed to one company would be filed into another's
    // inbox.
    let mailbox_slug = opencompany::server::ops::smtp::local_part(&cfg.address);
    if id != mailbox_slug {
        return;
    }
    #[cfg(feature = "imap")]
    {
        let Some(runtime) = state.registry().get(&CompanyId::new(id)) else {
            return;
        };
        let receiver: Arc<dyn opencompany::server::ops::mailer::MailReceiver> =
            Arc::new(opencompany::server::ops::imap::AsyncImapReceiver);
        let interval = std::env::var("OPENCOMPANY_MAIL_POLL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        let poller = opencompany::runtime::mailbox_poller::MailboxPoller::new(
            runtime,
            receiver,
            cfg.imap.clone(),
            cfg.address.clone(),
            interval,
        )
        // See `spawn_scheduler`: follow the registry so a rebuild reaches
        // inbound mail instead of stranding it on the replaced runtime.
        .following(state.registry().clone());
        handles.push(poller.spawn(shutdown.clone()));
    }
    #[cfg(not(feature = "imap"))]
    {
        let _ = (state, id, shutdown, handles, cfg);
    }
}

/// Attaches an OpenHuman JSON-RPC transport when the `openhuman-rpc` feature is
/// enabled and `OPENCOMPANY_OPENHUMAN_URL` is set (the attach path).
///
/// Without the feature this is the identity function, so the default build
/// stays network-free and degrades to built-in tools and the operator channel.
#[cfg(not(feature = "openhuman-rpc"))]
fn attach_openhuman(builder: RuntimeBuilder) -> RuntimeBuilder {
    builder
}

#[cfg(feature = "openhuman-rpc")]
fn attach_openhuman(builder: RuntimeBuilder) -> RuntimeBuilder {
    use opencompany::openhuman::HttpOpenHumanRpc;
    use opencompany::ports::SecretValue;

    match std::env::var("OPENCOMPANY_OPENHUMAN_URL") {
        Ok(url) if !url.trim().is_empty() => {
            let bearer =
                SecretValue(std::env::var("OPENCOMPANY_OPENHUMAN_TOKEN").unwrap_or_default());
            builder.with_openhuman_rpc(Arc::new(HttpOpenHumanRpc::attach(url, bearer)))
        }
        _ => builder,
    }
}

/// Attaches the embedded OpenHuman harness under the `openhuman` feature.
///
/// One line, because the sequence itself lives in the library
/// ([`opencompany::app::attach_harness`]) — the desktop shell builds companies
/// through `desktop::register` rather than through this binary, and a second
/// copy of the wiring here is exactly how that path came to build companies
/// with no harness at all.
fn attach_harness(builder: RuntimeBuilder) -> RuntimeBuilder {
    opencompany::app::attach_harness(builder)
}

/// Routes feedback to the TinyHumans hub when this instance is provisioned with
/// a credential, so reports are recorded on behalf of the credential's owner
/// instead of being filed as issues from here.
///
/// Without the feature this is the identity function, so the default build stays
/// network-free and keeps the local capture → GitHub/manual-link path.
#[cfg(not(feature = "tinyhumans"))]
fn attach_tinyhumans_feedback(builder: RuntimeBuilder, _config: &AppConfig) -> RuntimeBuilder {
    builder
}

/// Wires the hub identity exchange, when this build can reach the hub.
///
/// Rides the existing `tinyhumans` feature rather than earning one of its own:
/// that flag already means "this instance talks to the hub about its
/// credential's owner", and asking the hub whose sign-in token this is is the
/// same conversation about the same owner.
///
/// Unwired, `…/auth/hub` reports no providers and the console shows only the
/// magic-link form — which is the right answer for a self-hosted host that has
/// no ecosystem to sign in against.
#[cfg(not(feature = "tinyhumans"))]
fn attach_hub_identity(state: AppState) -> AppState {
    state
}

#[cfg(feature = "tinyhumans")]
fn attach_hub_identity(state: AppState) -> AppState {
    use opencompany::server::hub_identity::HttpHubIdentityExchange;

    let exchange = HttpHubIdentityExchange::new(state.config().api_url.clone());
    state.with_hub_identity(std::sync::Arc::new(exchange))
}

#[cfg(feature = "tinyhumans")]
fn attach_tinyhumans_feedback(builder: RuntimeBuilder, config: &AppConfig) -> RuntimeBuilder {
    use opencompany::feedback::HttpTinyHumansClient;

    match &config.tinyhumans_credential {
        Some(credential) => builder.with_tinyhumans_feedback(Arc::new(HttpTinyHumansClient::new(
            config.api_url.clone(),
            credential.clone(),
        ))),
        // Unprovisioned: keep the local path rather than dropping reports.
        None => builder,
    }
}

/// Parses a non-empty `usize` environment variable, ignoring unset/empty/invalid
/// values (an invalid value logs a warning and is treated as unset).
fn env_usize(key: &str) -> Option<usize> {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => match value.trim().parse::<usize>() {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                eprintln!("ignoring {key}=`{value}`: expected a non-negative integer");
                None
            }
        },
        _ => None,
    }
}

/// Default build: outbound webhooks require the `webhooks` feature; without it a
/// configured URL is warned and dropped.
#[cfg(not(feature = "webhooks"))]
fn webhook_config(_url: String) -> Option<opencompany::server::webhook::WebhookConfig> {
    eprintln!(
        "OPENCOMPANY_WEBHOOK_URL is set but the `webhooks` feature is not built; webhooks disabled"
    );
    None
}

/// Feature build: post to the configured URL with an HMAC-SHA256 signature.
#[cfg(feature = "webhooks")]
fn webhook_config(url: String) -> Option<opencompany::server::webhook::WebhookConfig> {
    use opencompany::server::webhook::{HmacSha256Signer, HttpWebhookSink, WebhookConfig};
    let secret = std::env::var("OPENCOMPANY_WEBHOOK_SECRET").unwrap_or_default();
    Some(WebhookConfig {
        sink: Arc::new(HttpWebhookSink::new(url)),
        signer: Arc::new(HmacSha256Signer),
        secret,
    })
}

/// Builds the injected connection seams for the credential surfaces. A real DNS
/// resolver is wired under the `dns` feature and a real SMTP sender under
/// `smtp`; the default build injects neither, so those surfaces 404 as
/// "not wired yet".
///
/// Host-level mail credentials (`OPENCOMPANY_MAIL_*`) are resolved here too. A
/// malformed or half-finished mail configuration is an error rather than a
/// silent `None`: a deployment that set those vars meant to have working mail.
fn connections_runtime() -> Result<opencompany::server::ops::ConnectionsRuntime> {
    #[allow(unused_mut)]
    let mut connections = opencompany::server::ops::ConnectionsRuntime::new();
    #[cfg(feature = "dns")]
    {
        match opencompany::company::dns::HickoryDnsResolver::from_system() {
            Ok(resolver) => connections = connections.with_dns(Arc::new(resolver)),
            Err(err) => eprintln!("dns resolver unavailable: {err}"),
        }
    }
    #[cfg(feature = "smtp")]
    {
        connections =
            connections.with_mail(Arc::new(opencompany::server::ops::smtp::LettreMailSender));
    }
    if let Some(mail) = opencompany::server::ops::mailer::MailConfig::from_env()? {
        connections = connections.with_mail_credentials(mail.credentials);
    }
    Ok(connections)
}

/// Resolves the OpenCompany home and moves any legacy doubled install up before
/// anything reads it.
///
/// Every command that touches bundles resolves through this rather than calling
/// `store::resolve_home` directly. `serve` needs it or the operator's companies
/// vanish from the console; `export` and `import` need it because otherwise an
/// un-migrated install's first post-upgrade command is the one that fails to
/// find its bundles. See `store::migrate` for the rules and the hosted no-op.
fn resolve_home_migrated(flag: Option<PathBuf>) -> Result<PathBuf> {
    let home = opencompany::store::resolve_home(flag)?;
    opencompany::store::migrate_legacy_nest_announced(&home)?;
    Ok(home)
}

/// Layers the data root's `config.toml` `[memory]` section onto the env-built
/// settings, the exact resolution the `serve` path uses.
///
/// A self-hosted operator's engine selection lives only in that file — the
/// console never exports environment variables — so bundle export/import must
/// see it or they would read and write the base stores instead of the engine
/// the host actually remembers with. An absent file layers nothing, and
/// `OPENCOMPANY_MEMORY` still owns the choice when set
/// (`StorageSettings::with_memory_config`).
fn layer_config_memory(
    settings: opencompany::store::StorageSettings,
    config_dir: &std::path::Path,
) -> Result<opencompany::store::StorageSettings> {
    let section = ConfigFile::load(config_dir)?
        .map(|c| c.memory.clone())
        .unwrap_or_default();
    settings.with_memory_config(&section)
}

/// The bundle ports plus the fact port, resolved the way `serve` resolves
/// them: the env-selected storage backend (`OPENCOMPANY_STORAGE`), with the
/// memory engine overlaid on top — `OPENCOMPANY_MEMORY*` from the environment,
/// or the instance's `config.toml` `[memory]` section under it (an env-owned
/// selection keeps the file layer inert).
///
/// Export and import used to hardwire the fs ports over `home`, which made a
/// bundle capture the *base* stores rather than what the deployment actually
/// remembers — on a host running a memory engine (or a sqlite/mongodb base),
/// that is the wrong data, silently. Routing through the same selection
/// `serve` uses means a bundle now reads and writes the live engine, and a
/// misconfigured engine refuses here exactly as it refuses a boot.
///
/// One deployment per bundle, enforced: with a non-default selection (an env
/// or `config.toml` that names a live storage backend or a memory engine), an
/// explicit `--home` is refused rather than mixed in — the base ports would
/// come from the flag while the engine roots at `OPENCOMPANY_DATA_DIR`, and a
/// bundle spanning two deployments is a company that never existed. Under the
/// fs+store default the environment is inert and `--home` means exactly what
/// it always has. `null` is refused in both directions (an export of nothing,
/// an import into a black hole, both exiting 0), and so is shared-single-DB
/// tenant mode (bundle ops write no owner rows; raw slugs would miss the
/// `<tenant>--` namespaced ids).
async fn live_ports(
    home: &std::path::Path,
    home_was_flagged: bool,
) -> Result<(
    opencompany::store::export::Ports,
    Option<Arc<dyn opencompany::ports::FactStore>>,
    Option<Arc<dyn opencompany::store::MemoryScopes>>,
    opencompany::store::StorageKind,
)> {
    use opencompany::store::{
        FsCompanyStore, FsContextStore, FsEventLog, FsMemoryStore, FsOps, StorageSettings,
        open_memory_overlay, open_storage,
    };
    let settings = StorageSettings::from_env()?;
    // Layer the instance's own `config.toml` `[memory]` section under the
    // environment, the same resolution `serve` uses: a self-hosted operator's
    // console selection lives only in that file, and a bundle must read and
    // write the engine the host actually remembers with. The env still owns
    // the choice when it names one (`with_memory_config`).
    let settings = layer_config_memory(settings, &opencompany::app::config::data_dir_from_env())?;
    // Every refusal lives in the lib (`store::select::refuse_bundle_env`)
    // where the feature lanes execute its tests; the bin only reports.
    opencompany::store::refuse_bundle_env(&settings, home_was_flagged)?;
    let (store, events, mut memory, mut context, mut facts, mut scopes) =
        match open_storage(&settings, home).await? {
            Some(h) => (
                h.company,
                h.events,
                h.memory,
                h.context,
                Some(h.facts),
                None,
            ),
            // The fs default: the same ports the old hardwired path built,
            // plus the fs fact store the old path silently left behind.
            None => (
                Arc::new(FsCompanyStore::new(home.to_path_buf())) as _,
                Arc::new(FsEventLog::new(home.to_path_buf())) as _,
                Arc::new(FsMemoryStore::new(home.to_path_buf())) as _,
                Arc::new(FsContextStore::new(home.to_path_buf())) as _,
                Some(Arc::new(FsOps::new(home.to_path_buf()))
                    as Arc<dyn opencompany::ports::FactStore>),
                None,
            ),
        };
    if let Some(overlay) = open_memory_overlay(&settings)? {
        memory = overlay.memory;
        context = overlay.context;
        if let Some(s) = overlay.scopes {
            scopes = Some(s);
        }
        if let Some(f) = overlay.facts {
            facts = Some(f);
        }
    }
    Ok((
        (store, events, memory, context),
        facts,
        scopes,
        settings.kind,
    ))
}

/// A process-unique temporary path under the system temp dir. Used only by the
/// `.tar` staging paths, which are compiled under the `export` feature.
#[cfg(feature = "export")]
fn unique_temp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("opencompany-{tag}-{}-{nanos}", std::process::id()))
}

/// Exports `id`'s bundle over the selected ports into the directory `dest`.
async fn export_to_dir(
    home: &std::path::Path,
    home_was_flagged: bool,
    id: &CompanyId,
    include_secrets: bool,
    dest: &std::path::Path,
) -> Result<()> {
    use opencompany::store::export::{ExportOpts, export_bundle_with_scopes};
    use opencompany::store::paths::Bundle;

    // The same exclusive root lock `serve` holds: a bundle read while the host
    // is writing is torn, and the refusal here names the running process
    // instead of silently racing it.
    let _home_lock = opencompany::store::lock::acquire(home)?;
    let ((store, events, memory, context), facts, scopes, _) =
        live_ports(home, home_was_flagged).await?;
    let opts = ExportOpts {
        include_secrets,
        fs_bundle: Some(Bundle::new(home.to_path_buf(), id).dir().to_path_buf()),
    };
    export_bundle_with_scopes(
        id, dest, store, events, memory, context, facts, scopes, opts,
    )
    .await
}

/// Default build: export writes an unpacked bundle directory (no `.tar` support
/// without the `export` feature).
#[cfg(not(feature = "export"))]
async fn run_export(
    company: String,
    out: Option<PathBuf>,
    include_secrets: bool,
    home: Option<PathBuf>,
) -> Result<()> {
    let home_was_flagged = home.is_some();
    let home = resolve_home_migrated(home)?;
    let id = CompanyId::new(company);
    let dest = out.unwrap_or_else(|| PathBuf::from(format!("{}-bundle", id.as_ref())));
    export_to_dir(&home, home_was_flagged, &id, include_secrets, &dest).await?;
    println!(
        "exported bundle for `{id}` to {} (build with --features export to produce a .tar)",
        dest.display()
    );
    Ok(())
}

/// `opencompany issue-password` — the host-side way into a company whose
/// deployment cannot mail a sign-in link (issue #1718).
///
/// Opens the configured storage directly rather than going through a running
/// server, because the authority here is possession of the process and its
/// data — which an operator has and an HTTP caller never does.
async fn run_issue_password(
    company: String,
    email: String,
    password: Option<String>,
    require_change: bool,
    home: Option<PathBuf>,
) -> Result<()> {
    use opencompany::ports::CompanyStore;
    use opencompany::server::users::bootstrap;

    // stdin when not passed, so the value stays out of shell history and out of
    // `ps`. Deliberately not a TTY prompt: a pipe is the case that matters
    // here — from a password manager, a secret file, or a provisioning script —
    // and reading stdin serves an interactive operator too.
    let password = match password {
        Some(value) => value,
        None => {
            use std::io::Read as _;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).map_err(|e| {
                opencompany::error::OpenCompanyError::Config(format!(
                    "could not read a password from stdin: {e}"
                ))
            })?;
            // Only the trailing newline goes: a password may legitimately end
            // in a space, and trimming both ends would silently change it.
            buf.trim_end_matches(['\n', '\r']).to_string()
        }
    };

    let home = resolve_home_migrated(home)?;
    let settings = opencompany::store::StorageSettings::from_env()?;
    // Serialize filesystem mutations with serve/import/export — but only for
    // the filesystem store. `serve` holds this same root lock for its whole
    // lifetime even on a MongoDB-backed tenant, so contending with it here
    // would refuse this command in exactly the environment it exists for: a
    // live hosted container that cannot mail a sign-in link. Those mutations
    // go through the storage handles and never touch `home`, so the lock has
    // nothing to protect there. The guard must outlive storage opening and
    // the complete password operation.
    let _home_lock = match settings.kind {
        opencompany::store::StorageKind::Fs => Some(opencompany::store::lock::acquire(&home)?),
        _ => None,
    };
    let config_root = opencompany::app::config::data_dir_from_env();
    let config_file = ConfigFile::load(&config_root)?;
    let fs_ops = Arc::new(opencompany::store::FsOps::new(home.clone()));
    let handles = opencompany::store::open_storage(&settings, &home).await?;
    let users: Arc<dyn opencompany::ports::users::UserStore> = handles
        .as_ref()
        .map(|handles| handles.users.clone())
        .unwrap_or_else(|| fs_ops.clone());
    let sessions: Arc<dyn opencompany::ports::sessions::SessionStore> = handles
        .as_ref()
        .map(|handles| handles.sessions.clone())
        .unwrap_or_else(|| fs_ops.clone());
    let login_codes: Arc<dyn opencompany::ports::login_codes::LoginCodeStore> = handles
        .as_ref()
        .map(|handles| handles.login_codes.clone())
        .unwrap_or_else(|| fs_ops.clone());

    // In shared-single-DB mode the `--company` argument is also a tenant
    // namespace carrier. Try the tenant-prefixed candidate first, because a
    // bare company id may itself contain `--`; then accept an already-expanded
    // id only when it carries this tenant's prefix. A different prefix is
    // refused rather than allowing a caller to select another tenant.
    let (id, record) = match std::env::var("OPENCOMPANY_TENANT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        None => {
            let id = CompanyId::new(company);
            let record = if let Some(handles) = handles.as_ref() {
                handles.company.load(&id).await?
            } else {
                opencompany::store::FsCompanyStore::new(home.clone())
                    .load(&id)
                    .await?
            };
            (id, record)
        }
        Some(tenant) => {
            if let Err(reason) = opencompany::app::validate_tenant_namespace(&tenant) {
                return Err(opencompany::error::OpenCompanyError::Config(reason));
            }
            let prefix = format!("{tenant}--");
            let (id, record) = if let Some(bare) = company.strip_prefix(&prefix) {
                if bare.is_empty() {
                    return Err(opencompany::error::OpenCompanyError::Config(format!(
                        "company id `{company}` is only the `{tenant}--` namespace prefix; \
                         it names no company"
                    )));
                }
                let id = CompanyId::new(company);
                let record = if let Some(handles) = handles.as_ref() {
                    handles.company.load(&id).await?
                } else {
                    opencompany::store::FsCompanyStore::new(home.clone())
                        .load(&id)
                        .await?
                };
                (id, record)
            } else {
                let bare_id = CompanyId::new(format!("{prefix}{company}"));
                let bare_record = if let Some(handles) = handles.as_ref() {
                    handles.company.load(&bare_id).await?
                } else {
                    opencompany::store::FsCompanyStore::new(home.clone())
                        .load(&bare_id)
                        .await?
                };
                if let Some(record) = bare_record {
                    (bare_id, Some(record))
                } else if company.contains("--") {
                    return Err(opencompany::error::OpenCompanyError::Config(format!(
                        "company id `{company}` is namespaced for another tenant; this deployment is \
                         `{tenant}`, whose ids take the `<tenant>--<name>` form"
                    )));
                } else {
                    (bare_id, None)
                }
            };
            (id, record)
        }
    };
    let record = record.ok_or_else(|| {
        opencompany::error::OpenCompanyError::Config(format!(
            "no company `{}` in this storage. Check the id — in shared-database mode it is the \
             namespaced `<tenant>--<id>` form.",
            id.as_ref()
        ))
    })?;
    let manifest_admins: Vec<String> = record.manifest.users.admins.clone();
    let auth_mode = {
        use std::str::FromStr as _;
        let raw = std::env::var("OPENCOMPANY_AUTH_MODE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| config_file.as_ref().and_then(|c| c.auth_mode.clone()))
            .unwrap_or_else(|| record.manifest.users.mode.clone());
        opencompany::app::config::AuthMode::from_str(&raw)?
    };
    if !auth_mode.uses_email() {
        return Err(opencompany::error::OpenCompanyError::Config(format!(
            "issue-password requires effective email auth mode, but the host is configured for `{auth_mode}`"
        )));
    }

    // The same variable `serve` reads for the deployment's standing admin, read
    // the same way. `standing_admins` normalizes and drops a blank, so this is
    // the value `AppConfig::bootstrap_admin` would produce.
    let bootstrap_admin = std::env::var("OPENCOMPANY_ADMIN_EMAIL")
        .ok()
        .filter(|value| !value.trim().is_empty());

    let issued = bootstrap::issue_password(
        bootstrap::PasswordIssueContext {
            users: &users,
            sessions: &sessions,
            login_codes: &login_codes,
            company: &id,
            manifest_admins: &manifest_admins,
            bootstrap_admin: bootstrap_admin.as_deref(),
        },
        &email,
        &password,
        require_change,
    )
    .await?;

    let verb = if issued.created { "created" } else { "updated" };
    println!("{verb} {} in `{}`", issued.email, id.as_ref());
    if issued.must_change_password {
        println!("they will be asked to replace this password on first sign-in");
    }
    Ok(())
}

/// `opencompany orphans` — the on-demand form of the boot check (issue #1077).
///
/// Opens storage, reads the two collections, and prints the set difference both
/// ways. Nothing is written.
///
/// Only meaningful in tenant-namespace mode: without
/// [`OPENCOMPANY_TENANT_ID`], no durable owner rows are ever written, so the
/// report would show every company as orphaned on every invocation. Refuses
/// to run when the variable is unset.
///
/// # Exit code
///
/// Zero whether or not orphans were found. This is a report, not an assertion:
/// a non-zero exit would make the command unusable in the one place it is most
/// wanted — a health check or a deploy script that wants the *answer* — and
/// "the query ran and found three" is a success, not a failure. The findings
/// are on stdout for a human and behind `--json` for anything else.
async fn run_orphans(home: Option<PathBuf>, json: bool) -> Result<()> {
    run_orphans_from(home, json, &ProcessEnv).await
}

async fn run_orphans_from(
    home: Option<PathBuf>,
    json: bool,
    env: &dyn opencompany::app::config::EnvSource,
) -> Result<()> {
    // Gate on OPENCOMPANY_TENANT_ID: the same condition that gates the
    // durable owner-row write at register_company. Without a tenant
    // namespace no owner rows are ever persisted, and reading zero owners
    // against N companies would report every company as orphaned.
    let tenant_id = env
        .get("OPENCOMPANY_TENANT_ID")
        .filter(|v| !v.trim().is_empty());
    if let Some(tenant) = &tenant_id {
        // A namespace containing the `--` id delimiter would make the
        // `<tenant>--` prefix ambiguous between tenants (see
        // `validate_tenant_namespace`). Reject it here, the boundary that
        // reads the variable.
        if let Err(reason) = opencompany::app::validate_tenant_namespace(tenant) {
            return Err(opencompany::error::OpenCompanyError::Config(reason));
        }
    }
    if tenant_id.is_none() {
        return Err(opencompany::error::OpenCompanyError::Config(
            "OPENCOMPANY_TENANT_ID is not set: this deployment does not persist \
             durable owner rows, so no company can be orphaned from one. \
             This check applies to shared-database deployments only."
                .into(),
        ));
    }

    let home = resolve_home_migrated(home)?;
    let settings = opencompany::store::StorageSettings::from_env_source(env)?;
    let Some(handles) = opencompany::store::open_storage(&settings, &home).await? else {
        // `StorageKind::Fs` yields no handles at all, so there is no `owners`
        // collection and this condition cannot arise. Say that plainly rather
        // than printing an empty report, which would read as "checked, all
        // clear" for a check that never ran.
        return Err(opencompany::error::OpenCompanyError::Config(format!(
            "storage backend `{:?}` keeps no ownership rows, so no company can be orphaned from \
             one. This check applies to shared-database deployments.",
            settings.kind
        )));
    };
    let Some(ownership) = handles.ownership.as_ref() else {
        return Err(opencompany::error::OpenCompanyError::Config(format!(
            "storage backend `{:?}` is open but persists no company -> tenant ownership, so there \
             is nothing to reconcile.",
            settings.kind
        )));
    };

    // Read list() before owners() (issue #1077): provisioning writes the
    // owner row before the company (#1050), so a provision crossing the
    // two reads lands as a benign dangling owner row rather than an
    // alarming unowned company.
    let companies = handles.company.list().await?;
    let owners = ownership.owners().await?;
    let report = opencompany::app::find_orphans(&companies, &owners);

    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else if report.is_empty() {
        println!(
            "No orphans: {} companies, {} owner rows, every one accounted for.",
            companies.len(),
            owners.len()
        );
    } else {
        print!("{}", report.to_text());
    }
    Ok(())
}

/// Feature build: export writes a single-file `.tar`.
#[cfg(feature = "export")]
async fn run_export(
    company: String,
    out: Option<PathBuf>,
    include_secrets: bool,
    home: Option<PathBuf>,
) -> Result<()> {
    use opencompany::store::export::pack_tar;

    let home_was_flagged = home.is_some();
    let home = resolve_home_migrated(home)?;
    let id = CompanyId::new(company);
    let out = out.unwrap_or_else(|| PathBuf::from(format!("{}.tar", id.as_ref())));

    // Stage the unpacked bundle under a slug-named dir so the tar nests cleanly.
    let staging = unique_temp("export");
    let bundle_dir = staging.join(id.as_ref());
    let result = async {
        export_to_dir(&home, home_was_flagged, &id, include_secrets, &bundle_dir).await?;
        pack_tar(&bundle_dir, &out)
    }
    .await;
    tokio::fs::remove_dir_all(&staging).await.ok();
    result?;
    println!("exported bundle for `{id}` to {}", out.display());
    Ok(())
}

/// Default build: import reads an unpacked bundle directory (no `.tar` support
/// without the `export` feature).
#[cfg(not(feature = "export"))]
async fn run_import(path: PathBuf, home: Option<PathBuf>) -> Result<()> {
    use opencompany::OpenCompanyError;

    if !path.is_dir() {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "{} is not a directory; rebuild with --features export to import a .tar",
            path.display()
        )));
    }
    import_from_dir(&path, home).await
}

/// Feature build: import a `.tar` (unpacked to a temp dir first) or a directory.
#[cfg(feature = "export")]
async fn run_import(path: PathBuf, home: Option<PathBuf>) -> Result<()> {
    use opencompany::store::export::unpack_tar;

    if path.is_dir() {
        return import_from_dir(&path, home).await;
    }
    let staging = unique_temp("import");
    let result = async {
        unpack_tar(&path, &staging)?;
        import_from_dir(&staging, home.clone()).await
    }
    .await;
    tokio::fs::remove_dir_all(&staging).await.ok();
    result
}

/// Imports the bundle rooted under `dir` into `home` through the fs ports,
/// restoring any fs-only secrets/keys the bundle carried.
async fn import_from_dir(dir: &std::path::Path, home: Option<PathBuf>) -> Result<()> {
    let home_was_flagged = home.is_some();
    use opencompany::store::export::{
        find_bundle_root, import_bundle_with_scopes, restore_fs_artifacts,
    };
    use opencompany::store::paths::Bundle;

    let home = resolve_home_migrated(home)?;
    let root = find_bundle_root(dir)?;
    // Exclusive, same as `serve`: an import into stores a running host has
    // open is the single-writer violation the lock module exists to prevent.
    let _home_lock = opencompany::store::lock::acquire(&home)?;
    let ((store, events, memory, context), facts, scopes, storage_kind) =
        live_ports(&home, home_was_flagged).await?;
    let id =
        import_bundle_with_scopes(&root, store, events, memory, context, facts, scopes).await?;
    restore_fs_artifacts(&root, Bundle::new(home.clone(), &id).dir()).await?;
    // On a non-fs base backend the records above went to the live backend
    // while these artifacts (secrets/, keys/) are fs-only by design and land
    // under the resolved home. Same split serve itself runs with there — but
    // say it, so a restore onto a fresh pod knows to mount that home.
    // The kind comes back from live_ports rather than a second from_env():
    // an env re-read that failed HERE would report an error for an import
    // that already committed.
    if storage_kind != opencompany::store::StorageKind::Fs {
        eprintln!(
            "note: records imported into the live `{}` backend; fs-only artifacts (secrets/, \
             keys/) restored under {} — on an ephemeral filesystem, re-provision them there \
             after a pod replacement.",
            storage_kind.as_str(),
            home.display()
        );
    }
    println!("imported company `{id}` into {}", home.display());
    Ok(())
}

/// `memory migrate`: the data half of the engine-switch runbook.
///
/// FROM is deliberately not a flag: it is the env-selected engine, exactly
/// what a boot would bind — you migrate *before* flipping the environment, so
/// the environment still names the source. Only provider-backed engines can
/// migrate (the seam is what `export_page`/`import_records` live on); the
/// `store` default is refused by name.
#[cfg(feature = "tinymemory")]
async fn run_memory_cmd(cmd: MemoryCmd) -> Result<()> {
    use opencompany::store::StorageSettings;
    use opencompany::store::memory::driver::open_driver;
    use opencompany::store::memory::migrate::migrate;

    let MemoryCmd::Migrate {
        to,
        to_url,
        to_api_key,
        page_size,
        dry_run,
        resume_cursor,
    } = cmd;

    let settings = StorageSettings::from_env()?;
    // The credential prefers the environment — and the environment WINS over
    // the flag, not just fills its absence: argv is world-readable in
    // /proc/<pid>/cmdline for the whole (possibly long) run, so when both are
    // set the one that was passed safely is the one that counts. --to-api-key
    // stays only for compatibility.
    let to_api_key = std::env::var("OPENCOMPANY_MEMORY_TARGET_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .or(to_api_key);
    // Every refusal and both configurations come from the lib
    // (`store::memory::migrate::resolve_migrate_configs`), where the feature
    // lanes execute the guards' tests; the bin drives the loop and reports.
    let (from_config, to_config) = opencompany::store::memory::migrate::resolve_migrate_configs(
        &settings, &to, to_url, to_api_key,
    )?;

    // Hosted providers have no local memory store to lock. The pause-first
    // precondition printed below still applies to remote writers.
    let _home_lock: Option<()> = None;

    let (from, _) = open_driver(&from_config)?.ok_or_else(|| {
        opencompany::error::OpenCompanyError::Config(
            "the source configuration bound no provider (host bug — the routing above should \
             have refused)."
                .into(),
        )
    })?;
    // Dry run touches ONLY the source: opening the target would create its
    // store (a namespace target mints the SQLite dir on open), and "without
    // writing anything" must mean the filesystem too.
    if dry_run {
        let resumed = resume_cursor.is_some();
        let total = opencompany::store::memory::migrate::count_records(
            &from,
            resume_cursor.clone(),
            page_size,
        )
        .await?;
        if resumed {
            println!(
                "dry run (from --resume-cursor): {} records remain to migrate {} -> {}",
                total,
                from.driver_id(),
                to
            );
        } else {
            println!(
                "dry run: {} records would migrate {} -> {}",
                total,
                from.driver_id(),
                to
            );
        }
        return Ok(());
    }

    let (target, target_class) = open_driver(&to_config)?.ok_or_else(|| {
        opencompany::error::OpenCompanyError::Config(
            "the target configuration bound no provider (host bug).".into(),
        )
    })?;

    if matches!(target_class, tinymemory::registry::DriverClass::External) {
        eprintln!(
            "note: `{}` is a hosted engine — its exact-CRUD writes are enumeration-based, so a \
             large import is slow and chatty. Prefer off-peak, and expect wall-clock to grow \
             with store size.",
            target.driver_id()
        );
    }

    // The one operational precondition this command cannot enforce itself:
    // there is no dual-write, so live cycles writing the source mid-copy are
    // lost to the target. Said here as well as in the runbook, because the
    // runbook is optional reading and this line is not.
    eprintln!(
        "note: pause the workload first — writes landing on the source during the copy do not \
         reach the target."
    );
    println!(
        "migrating {} -> {} ({} records/page)…",
        from.driver_id(),
        target.driver_id(),
        page_size
    );
    let outcome = migrate(&from, &target, page_size, resume_cursor, |progress| {
        println!(
            "  page {}: {} exported, {} imported, {} skipped",
            progress.pages, progress.exported, progress.imported, progress.skipped
        );
    })
    .await?;
    match outcome {
        Ok(summary) => {
            // The receipt: count the TARGET's own export, so the operator's
            // evidence is the target's answer rather than the migration's own
            // counters. Costs one enumeration of the target — the same order
            // of work the migration itself just did. Best-effort: a target
            // that cannot re-export right now degrades the receipt to a
            // warning, not the completed migration to a failure.
            match opencompany::store::memory::migrate::count_records(&target, None, page_size).await
            {
                Ok(target_total) => {
                    println!(
                        "done: {} exported, {} imported, {} already present, over {} pages. \
                         Target now exports {target_total} records (its own count, not ours). \
                         Now flip OPENCOMPANY_MEMORY* to the target, restart, and verify /spec.",
                        summary.exported, summary.imported, summary.skipped, summary.pages
                    );
                }
                Err(e) => {
                    println!(
                        "done: {} exported, {} imported, {} already present, over {} pages. Now \
                         flip OPENCOMPANY_MEMORY* to the target, restart, and verify /spec.",
                        summary.exported, summary.imported, summary.skipped, summary.pages
                    );
                    eprintln!(
                        "note: could not verify by re-counting the target ({e}); the counters \
                         above are the migration's own."
                    );
                }
            }
            Ok(())
        }
        Err(stopped) => {
            for error in &stopped.errors {
                eprintln!("target error: {error}");
            }
            let resume = stopped
                .resume_cursor
                .as_deref()
                .map(|c| format!("--resume-cursor {c}"))
                .unwrap_or_else(|| "the beginning (the first page failed)".into());
            Err(opencompany::error::OpenCompanyError::Store(format!(
                "migration stopped after {} imported / {} skipped of {} exported; fix the \
                 target and re-run with {resume} — import is idempotent by (namespace, key), \
                 so re-running the failed page cannot duplicate.",
                stopped.summary.imported, stopped.summary.skipped, stopped.summary.exported
            )))
        }
    }
}

/// Without the provider seam there is nothing to migrate through; refuse by
/// naming the feature, the same shape as the selection refusals in
/// `store::select`.
#[cfg(not(feature = "tinymemory"))]
async fn run_memory_cmd(cmd: MemoryCmd) -> Result<()> {
    let MemoryCmd::Migrate { .. } = cmd;
    Err(opencompany::error::OpenCompanyError::Config(
        "`memory migrate` requires a build with the `tinymemory` feature (the provider seam \
         its Portability family lives on)."
            .into(),
    ))
}

/// Handle the `openhuman` subcommand: build the launch request, reject
/// passthrough args in Desktop mode via [`OpenHumanLaunch::validate`]
/// (before the dry-run branch so `--dry-run -- --arg` reports the same error
/// as a real launch instead of printing an unlaunchable command), then either
/// print the preview or run to completion and exit with the child's code.
async fn run_openhuman(
    root: PathBuf,
    mode: ModeArg,
    release: bool,
    dry_run: bool,
    args: Vec<String>,
) -> Result<()> {
    let mut launch = match LaunchMode::from(mode) {
        LaunchMode::Core => OpenHumanLaunch::core(root),
        LaunchMode::Desktop => OpenHumanLaunch::desktop(root),
    }
    .with_args(args);
    if release {
        launch = launch.release();
    }

    // validate() rejects passthrough args in Desktop mode; run it before
    // the dry-run branch so `--dry-run -- --arg` reports the same error
    // as an actual launch instead of printing an unlaunchable command.
    launch.validate()?;

    if dry_run {
        println!("{}", launch.dry_run_preview());
        return Ok(());
    }

    let status = launch.run().await?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(feature = "openhuman")]
const WORKER_STACK_BYTES: usize = openhuman_core::core::runtime::AGENT_WORKER_STACK_BYTES;
#[cfg(not(feature = "openhuman"))]
const WORKER_STACK_BYTES: usize = 2 * 1024 * 1024;

#[cfg(feature = "openhuman")]
const MAX_BLOCKING_THREADS: usize = openhuman_core::core::runtime::MAX_BLOCKING_THREADS;
#[cfg(not(feature = "openhuman"))]
const MAX_BLOCKING_THREADS: usize = 512;

/// The log filter used when `RUST_LOG` says nothing.
///
/// The bare `error` is exactly what `EnvFilter::from_default_env()` fell back to,
/// so no target in this binary becomes chattier than it was. The one added
/// directive is the exception the default cannot express, and it is not cosmetic.
///
/// `tinyagents::observability` is the target the vendored durable-append writer
/// (`AppendWorker`, in
/// `vendor/openhuman/vendor/tinyagents/src/harness/observability/worker.rs`)
/// reports on — the writer behind the embedded runtime's durable agent journal.
/// It reports only the *first* failure of a failure run at `error`; every line
/// after that is `warn`:
///
/// - "still failing after N consecutive observations" — the run is ongoing,
/// - "recovered; N observation(s) lost" — the run ended, and how much it cost,
/// - "never recovered before shutdown, N observation(s) lost" — the run outlived
///   the process.
///
/// Those `warn` lines are the only signal that the durable log is losing data and
/// how much of it. Under a bare `error` filter an operator sees one line when a
/// degraded run begins and then nothing — no reminder that it is still degraded,
/// no recovery, no loss count — which is the visibility gap issue #450 is about.
/// The writer's own subscriber-independent fallback, `AppendWorker::append_failures()`,
/// is `pub(crate)` in tinyagents and cannot be read from here, so the subscriber
/// is the only channel we have (see `docs/spec/runtime/workspace-layout.md`).
///
/// Setting `RUST_LOG` replaces this string wholesale — the operator keeps full
/// control, and behaviour with `RUST_LOG` set is unchanged.
const DEFAULT_LOG_FILTER: &str = "error,tinyagents::observability=warn";

fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_BYTES)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .build()?
        .block_on(async_main())
}

/// The filter to install, given whatever `RUST_LOG` holds.
///
/// Split out from [`async_main`] and taking the variable as an argument so the
/// choice can be tested without mutating process-global environment state,
/// which no test can do without racing every other test in the binary.
///
/// `Some` — the operator set the variable, so it is parsed exactly the way it
/// was before [`DEFAULT_LOG_FILTER`] existed: **lossily**. A single malformed
/// directive drops itself and the valid ones around it still apply. Parsing it
/// strictly and falling back on error would silently discard an operator's
/// entire working configuration over one typo, which is a worse failure than
/// the one this constant was added to fix.
///
/// `None` — the variable is unset, so [`DEFAULT_LOG_FILTER`] applies.
fn log_filter(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter {
    // `EnvFilter::new` is `parse_lossy` over the same `ERROR` default directive
    // that `from_default_env` uses, so passing the variable's value through it
    // is exactly what the old code did with it — just reachable from a test.
    match rust_log {
        Some(directives) => tracing_subscriber::EnvFilter::new(directives),
        None => tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER),
    }
}

async fn async_main() -> Result<()> {
    let rust_log = std::env::var(tracing_subscriber::EnvFilter::DEFAULT_ENV).ok();
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(rust_log.as_deref()))
        .init();

    match Cli::parse().command {
        Some(Command::Serve {
            bind,
            openhuman_root,
            companies,
            home,
            discoverable,
        }) => {
            // `--home` > OPENCOMPANY_DATA_DIR > $HOME/.opencompany, then any
            // legacy doubled install is moved up before a single bundle is read.
            let home = resolve_home_migrated(home)?;
            // Exclusive ownership of the data root, held for the whole process.
            //
            // `docs/spec/runtime/storage.md` has always said the runtime journal
            // is single-writer and that two hosts sharing a home write over each
            // other; nothing enforced it. The common way to hit that is not
            // exotic — a `serve` left running in another terminal, then a second
            // one started against the same default `~/.opencompany`.
            //
            // Bound to a name that lives until `serve` returns. Binding it to
            // `_` would drop it immediately and release the root while this
            // process carried on writing, which is worse than not locking.
            let _home_lock = opencompany::store::lock::acquire(&home)?;
            // Materialize the canonical data-dir workspace layout and empty the
            // ephemeral `tmp/` scratch so nothing stale survives a restart. The
            // `[workspace]` section of `config.toml` (in the data dir) toggles
            // the tmp clear; absent config keeps the default (clear on startup).
            let data_root = opencompany::app::config::data_dir_from_env();
            // An explicit `--home` pointing away from the data root splits one
            // instance in two: bundles here, shared workspace there. Printed
            // rather than `warn!`d — the default `EnvFilter` drops warnings
            // unless RUST_LOG is set, so a `warn!` would be exactly as silent
            // as the bug it reports.
            if let Some(warning) = opencompany::store::home_divergence_warning(&home, &data_root) {
                eprintln!("opencompany: {warning}");
            }
            // Kept whole rather than mapped straight to `.workspace`: the
            // `bind` key is read off the same file further down (issue #425).
            let config_file = ConfigFile::load(&data_root)?;
            let workspace_cfg = config_file
                .as_ref()
                .map(|c| c.workspace.resolve())
                .unwrap_or_default();
            let layout = opencompany::store::DataLayout::new(&data_root);
            layout.ensure(workspace_cfg.clear_tmp_on_startup).await?;
            // Point the embedded OpenHuman runtime's durable agent journal at
            // the data volume and prove it is writable before a single agent
            // exists. Left alone the vendored runtime roots it under the user's
            // home directory, which in a tenant container is the read-only root
            // filesystem: every observation then fails to persist and the
            // vendored append worker reports it per event, forever, drowning
            // the container log (issue #446). An unwritable root aborts boot —
            // same precedent as a selected-but-unavailable storage backend
            // below — rather than serving an agent whose work is never
            // recorded. `println!` for the same reason as the divergence
            // warning above: the default `EnvFilter` would swallow an `info!`.
            let journal = opencompany::app::journal::prepare(&data_root).await?;
            println!("{}", journal.summary());
            // Register the same root as the vendored keyring's directory, so
            // credential storage cannot resolve to `$HOME` — or, further down
            // the same fallback chain, to `/tmp` at no log level (issue #451).
            // The export above already steers it there in practice, but only
            // because nothing has touched the keyring yet; the resolved value is
            // cached in a `OnceLock`, so the guarantee rests on startup ordering
            // nobody is checking. Registering says it outright.
            //
            // Here, and not later: this runs before any company runtime, agent
            // harness or HTTP listener exists, so the pin cannot lose a race to
            // a first keyring touch. `println!` for the same reason as the lines
            // around it — the default `EnvFilter` would swallow an `info!`, and
            // an operator needs to be able to see where the credentials live.
            #[cfg(feature = "openhuman")]
            println!(
                "{}",
                opencompany::app::journal::pin_keyring(&journal).summary()
            );
            // Tag every request the embedded openhuman_core makes to the
            // TinyHumans backend as opencompany's, not the vendored
            // runtime's own `openhuman` default (issue #376). This must run
            // here — before any company runtime, agent harness or HTTP
            // listener exists — because core's `IntegrationClient` reads the
            // identity into its default headers AT CONSTRUCTION
            // (`harness/toolbelt.rs`, `harness/composio.rs`,
            // `harness/search.rs` each build one the first time a company
            // needs it), so a call after the first client already exists
            // would not retroactively re-tag it. Same startup-ordering
            // reasoning as the keyring pin directly above: say it here, once,
            // rather than leaving it implicit in which line happens to run
            // first.
            // The call itself lives in the library so a test can reach it —
            // this arm cannot be exercised from one.
            #[cfg(feature = "openhuman")]
            opencompany::product::install_into_embedded_core();
            // Soft disk-quota alerting. Hard enforcement is the container /
            // StorageClass layer's job (EFS access point, k8s ResourceQuota);
            // here we surface an operator-visible warning when a workspace
            // exceeds its configured `[workspace]` quota.
            if let Some(limit) = workspace_cfg.storage_quota_bytes {
                let used = layout.usage_bytes().await?;
                if used > limit {
                    tracing::warn!(
                        used_bytes = used,
                        quota_bytes = limit,
                        data_dir = %data_root.display(),
                        "workspace storage over quota — enforce hard limits at the container/StorageClass layer",
                    );
                }
            }
            if let Some(limit) = workspace_cfg.tmp_quota_bytes {
                let used = layout.tmp_bytes().await?;
                if used > limit {
                    tracing::warn!(
                        used_bytes = used,
                        quota_bytes = limit,
                        "workspace tmp/ scratch over quota",
                    );
                }
            }
            // tiny.place economy + public-card configuration resolved from the
            // environment (with built-in defaults); the a2a routes and boot
            // going-public flow read these off `AppConfig`.
            let tinyplace_api_url = std::env::var("TINYPLACE_API_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| opencompany::app::config::DEFAULT_TINYPLACE_API_URL.to_string());
            let public_url = std::env::var("OPENCOMPANY_PUBLIC_URL")
                .ok()
                .filter(|value| !value.trim().is_empty());
            // A friendly name for this instance, shown by a client that holds
            // several connections at once. Cosmetic only: nothing selects,
            // authorizes, or routes on it, and `/spec` is unauthenticated, so
            // an operator naming a host "prod-eu" is publishing that.
            let instance_name = std::env::var("OPENCOMPANY_INSTANCE_NAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.chars().take(120).collect::<String>());
            // Shared-single-DB tenant identity. When set, company ids are
            // namespaced with this value so many tenants can share one logical
            // database without colliding on the `companies` unique index. Unset
            // (db-per-tenant / single-tenant) keeps every id derivation as-is.
            let tenant_namespace = std::env::var("OPENCOMPANY_TENANT_ID")
                .ok()
                .filter(|value| !value.trim().is_empty());
            if let Some(tenant) = &tenant_namespace {
                // A namespace containing the `--` id delimiter would make the
                // `<tenant>--` prefix ambiguous between tenants (see
                // `validate_tenant_namespace`), so a shared-DB workload with
                // one would namespace ids that collide with another tenant's.
                // Refuse to boot rather than misattribute at runtime.
                if let Err(reason) = opencompany::app::validate_tenant_namespace(tenant) {
                    return Err(opencompany::error::OpenCompanyError::Config(reason));
                }
            }
            // The address the platform records as this instance's creator. A
            // provisioned company's manifest names no admin, so without this
            // nobody is eligible to log in and there is no operator token to
            // send the first invite with (issue #321). Treated exactly like a
            // manifest `[users].admins` entry — a standing invite, not an
            // account. Unset (self-hosted, local `serve`) is a full no-op.
            let admin_email = std::env::var("OPENCOMPANY_ADMIN_EMAIL")
                .ok()
                .filter(|value| !value.trim().is_empty());
            // Hosted-brain credential, resolved with the same precedence the
            // harness uses (`harness_inference_from_env`) so `/spec`'s
            // `cycles_available` reflects whether cognition can actually run.
            let tinyhumans_credential = std::env::var("OPENCOMPANY_INFERENCE_KEY")
                .or_else(|_| std::env::var("TINYHUMANS_API_KEY"))
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(opencompany::ports::types::SecretValue);
            // Honor TINYHUMANS_API_URL (e.g. staging) — the config layer reads
            // it, but this manual AppConfig build otherwise falls to the prod
            // default, so a staging credential could never reach staging.
            let api_url = std::env::var("TINYHUMANS_API_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| AppConfig::default().api_url);
            // The listener address, across every layer that may name it. Until
            // issue #425 only the flag reached this struct, so the manager's
            // injected `OPENCOMPANY_BIND` (and any `config.toml` `bind`) moved
            // `doctor`'s report while the host kept serving on the default —
            // silently, because the host does start and does serve.
            // Install-wide MCP defaults (issue #527). Normalized here, at the
            // boundary that reads the file, so a rejected entry is named at boot
            // where somebody is looking — rather than silently thinning the list
            // on every company's first agent turn. A bad entry is a warning, not
            // a boot failure: these are additive convenience, and refusing to
            // start over one malformed default would turn a cosmetic packaging
            // error into an outage.
            //
            // Read before `resolve_serve_bind` below, which consumes
            // `config_file`.
            let default_mcp_servers = {
                let raw = config_file
                    .as_ref()
                    .map(|c| c.default_mcp_servers.clone())
                    .unwrap_or_default();
                let (kept, problems) = opencompany::company::mcp::normalize_default_servers(&raw);
                for problem in &problems {
                    tracing::warn!(target: "opencompany::config", "{problem}");
                }
                kept
            };
            // A host-wide sign-in mode, when the deployment names one. Read
            // before `resolve_serve_bind` consumes `config_file`. An unparseable
            // value aborts boot rather than silently falling back to email:
            // "the login screen you asked for is not the one you got" is not a
            // failure anyone would notice from a running host.
            let auth_mode_override = {
                use std::str::FromStr as _;
                let raw = std::env::var("OPENCOMPANY_AUTH_MODE")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| config_file.as_ref().and_then(|c| c.auth_mode.clone()));
                match raw {
                    Some(raw) => Some(opencompany::app::config::AuthMode::from_str(&raw)?),
                    None => None,
                }
            };
            // Read before `config_file` is consumed for `bind` just below.
            // Whether this data root has been through the first-run setup flow
            // (`server::setup`); absent — a fresh root, or one predating the
            // flow — leaves it false, which is what puts the console into the
            // wizard instead of a sign-in form for a host nobody configured.
            let setup_complete = config_file
                .as_ref()
                .is_some_and(|c| c.setup_completed_at.is_some());
            // Read off the file before it is consumed for `bind` below: the
            // memory engine is resolved further down, after the state exists.
            let memory_section = config_file
                .as_ref()
                .map(|c| c.memory.clone())
                .unwrap_or_default();
            let (bind, bind_source) = opencompany::app::config::resolve_serve_bind(
                bind,
                &ProcessEnv,
                config_file.and_then(|c| c.bind),
            );
            let state = AppState::new(AppConfig {
                bind,
                default_mcp_servers,
                openhuman_root,
                api_url,
                tinyplace_api_url,
                public_url,
                instance_name,
                tenant_namespace,
                admin_email,
                auth_mode_override,
                tinyhumans_credential,
                // Issue #553: the workspace's enforced byte limits, read from
                // the same `[workspace]` section as the soft disk quotas above
                // and handed to every company's builder below.
                workspace_quota: workspace_cfg.quota,
                workspace_git_enabled: workspace_cfg.git_enabled,
                ..AppConfig::default()
            })
            .with_cors(opencompany::server::cors::CorsConfig::from_env()?)
            .with_home(home.clone())
            // `setup_complete` above, and every `config.toml` read/write the
            // first-run setup flow does, resolve against `data_root` — not
            // `home`, which an explicit `--home` can point elsewhere (see
            // `home_divergence_warning`). Recording it here is what lets
            // `server::setup` resolve the same file rather than its own
            // `state.home()`.
            .with_config_root(data_root.clone())
            .with_setup_complete(setup_complete)
            .with_quota(
                env_usize("OPENCOMPANY_MAX_COMPANIES"),
                env_usize("OPENCOMPANY_MAX_COMPANIES_PER_TENANT"),
            );
            let mut state = attach_hub_identity(state);
            // Storage backend selection: fs (default) needs nothing; sqlite and
            // mongodb are opened once here and injected into every company's
            // builder. A selected-but-unavailable backend aborts boot rather
            // than silently falling back to fs.
            // The environment first, then the instance's own `config.toml`
            // `[memory]` section under it — the engine an operator chose from
            // the console. A deployment that injects `OPENCOMPANY_MEMORY` keeps
            // ownership and the file layer is inert; see `MemorySection`.
            let storage_settings = opencompany::store::StorageSettings::from_env()?
                .with_memory_config(&memory_section)?;
            if let Some(handles) =
                opencompany::store::open_storage(&storage_settings, &home).await?
            {
                // Shared-database platform mode: restore the durable company →
                // tenant map so ownership survives restarts. In shared-single-DB
                // mode the `owners` collection holds every tenant's rows; hydrate
                // only this tenant's own mappings so the in-memory map never
                // leaks other tenants' companies (which are unaddressable here
                // regardless, since the registry only holds locally-loaded ones).
                if let Some(ownership) = &handles.ownership {
                    let self_tenant = state.config().tenant_namespace.clone();
                    // Issue #1077: report companies orphaned from their tenant.
                    //
                    // A company whose owner row is missing is unreachable by its
                    // own tenant — `authorize_address` finds no owner and answers
                    // 403 — and until now nothing anywhere reported it. #1050's
                    // fix stopped provisioning creating new ones; it does nothing
                    // for the rows the old behaviour already left behind.
                    //
                    // Gated on tenant_namespace: only tenant-namespace mode writes
                    // durable owner rows at all, and without it every company
                    // would appear orphaned on every boot. The gate also spares
                    // non-namespaced deployments the whole-collection scan the
                    // report needs.
                    //
                    // Read list() BEFORE owners() (issue #1077): provisioning
                    // writes the owner row before the company (#1050), so a
                    // provision crossing the two reads lands as a benign dangling
                    // owner row rather than an alarming unowned company.
                    let companies = if self_tenant.is_some() {
                        match handles.company.list().await {
                            Ok(c) => Some(c),
                            // A failed listing must not abort a boot that would
                            // otherwise succeed: this is a diagnostic, and the
                            // server is perfectly able to serve every company
                            // whose ownership *is* intact without it.
                            Err(e) => {
                                eprintln!("warning: could not check company ownership: {e}");
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let owners = ownership.owners().await?;
                    if let (Some(me), Some(companies)) = (&self_tenant, &companies) {
                        // Filtered to this workload's own tenant: in
                        // shared-single-DB the `owners` collection holds every
                        // tenant's rows, and printing them all to one tenant
                        // pod's stderr would leak other tenants' company ids and
                        // tenant strings. `opencompany orphans` is the
                        // unfiltered, platform-scoped form.
                        let report = opencompany::app::find_orphans(companies, &owners);
                        let filtered = opencompany::app::filter_to_tenant(report, me);
                        if !filtered.is_empty() {
                            eprintln!(
                                "warning: ownership rows and companies disagree \
                                 (`opencompany orphans` for this report)\n{}",
                                filtered.to_text()
                            );
                        }
                    }
                    for (id, tenant) in owners {
                        match &self_tenant {
                            // Compare in canonical (bare-slug) form so a row
                            // persisted as `tenant:acme` still hydrates under the
                            // workload's bare `acme` namespace, and vice versa.
                            Some(me)
                                if opencompany::app::canonical_tenant(&tenant)
                                    != opencompany::app::canonical_tenant(me) =>
                            {
                                continue;
                            }
                            _ => state.set_owner(id, tenant),
                        }
                    }
                }
                state = state
                    .with_stores(handles)
                    .with_storage_kind(storage_settings.kind);
                println!("storage backend: {:?}", storage_settings.kind);
            }
            // Memory engine overlay (`OPENCOMPANY_MEMORY`): swaps just the
            // memory + context ports onto a dedicated engine on top of the base
            // backend. A selected-but-unavailable engine aborts boot, same as
            // the storage backend.
            if let Some(mut overlay) = opencompany::store::open_memory_overlay(&storage_settings)? {
                // One bounded reachability probe — after `BoundMemory::bind`,
                // BEFORE the TCP listener binds — so a dead endpoint or a
                // revoked key shows on `/spec` at boot instead of surfacing as
                // a mid-cycle failure days later. The placement means a
                // blackholed hosted endpoint costs up to the timeout on a cold
                // wake (the manager's wake proxy blocks on `/healthz`); taken
                // knowingly — it only fires when OPENCOMPANY_MEMORY selects an
                // engine, and moving it post-listen needs a mutable descriptor
                // seam this deliberately avoids. Advisory: it warns and
                // records, never refuses — config errors already refused
                // above, and a transient vendor outage must not crash-loop
                // the tenant.
                overlay
                    .refresh_health(std::time::Duration::from_secs(5))
                    .await;
                state = state.with_memory_overlay(overlay);
                // `as_str`, not `{:?}`: the enum's Debug name is `Tinycortex`
                // while `/spec` and the docs call that engine `embedded`. An
                // operator comparing a boot log against a status response should
                // not have to work out that those are the same thing.
                println!(
                    "memory backend: {}",
                    storage_settings.memory_backend.as_str()
                );
            }
            // Platform (multi-tenant) auth: either credential enables the
            // provisioning/lifecycle surface. Without both the prosumer operator
            // path stays in force. A signing secret this build cannot verify
            // aborts boot here rather than silently degrading — same precedent
            // as a selected-but-unavailable storage backend above.
            {
                use opencompany::server::platform_auth::{
                    PLATFORM_JWT_SECRET_ENV, PLATFORM_TOKEN_ENV, configure,
                };
                if let Some((platform_auth, mode)) = configure(
                    std::env::var(PLATFORM_TOKEN_ENV).ok(),
                    std::env::var(PLATFORM_JWT_SECRET_ENV).ok(),
                )? {
                    state = state.with_platform_auth(platform_auth);
                    // The mode only — never the secret, or anything derived
                    // from it.
                    println!("platform auth: {mode}");
                }
            }
            // Outbound webhooks: a URL wires the HTTP sink under `webhooks`;
            // without the feature the request is warned and dropped.
            if let Some(url) = std::env::var("OPENCOMPANY_WEBHOOK_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
                && let Some(webhook) = webhook_config(url)
            {
                state = state.with_webhook(webhook);
            }
            // Connection seams: a real DNS resolver (feature `dns`) enables custom
            // domain verification; a real SMTP sender (feature `smtp`) enables the
            // test send and outbound mail. Absent the features these stay `None`
            // and the surfaces degrade to "not wired yet" (404).
            state = state.with_connections(connections_runtime()?);
            // The repo-level shared skill library (`skills/`) sits beside the
            // `companies/` dir; derive it from the first loaded company's source
            // dir so the `skillRegistry` query resolves the committed library.
            if let Some(skills_root) = companies.first().and_then(|path| {
                let dir = company_source_dir(path);
                dir.parent()
                    .and_then(|companies_dir| companies_dir.parent())
                    .map(|repo_root| repo_root.join("skills"))
            }) {
                state = state.with_skills_root(skills_root);
            }
            // Issue #290: with every builder input above now resolved, this host
            // can rebuild a company's runtime in place. Wired BEFORE the
            // companies register, so the very first `PUT …/inference` on a
            // freshly booted tenant already has a rebuilder to reach for.
            state = state.with_rebuilder(Arc::new(BootRebuilder));
            // Issue #1739. The handle goes on the state now, before any company
            // is built, because a company's usage meter is wrapped at build
            // time; the tracker behind it is chosen further down, once a
            // runtime exists to read cognition off. See
            // `analytics::DeferredTracker`.
            let analytics = Arc::new(opencompany::analytics::DeferredTracker::new());
            state = state.with_analytics(analytics.clone());
            // Schedulers stop cleanly when this is notified (Ctrl-C below).
            let shutdown = Arc::new(Notify::new());
            let mut scheduler_handles = Vec::new();
            for dir in &companies {
                let (id, name, schedules) =
                    register_company(&state, &home, dir, discoverable).await?;
                let visibility = if discoverable {
                    " [discoverable: public]"
                } else {
                    ""
                };
                if let Some(handle) = spawn_scheduler(&state, &id, &schedules, &shutdown) {
                    scheduler_handles.push(handle);
                    println!(
                        "registered company `{id}` ({name}) from {} with {} schedule(s){visibility}",
                        dir.display(),
                        schedules.len()
                    );
                } else {
                    println!(
                        "registered company `{id}` ({name}) from {}{visibility}",
                        dir.display()
                    );
                }
                spawn_mailbox_poller(&state, &id, &shutdown, &mut scheduler_handles);
            }
            if companies.is_empty() {
                // Nothing was named on the command line, so adopt whatever this
                // data root already holds. A company can reach a root without
                // ever being named on a command line — the first-run setup flow
                // (`server::setup`) seeds one — and without this an operator who
                // completed setup, was told to restart for their settings to
                // take effect, and did, came back to an empty host with their
                // company sitting unread on disk.
                //
                // Adopting creates nothing: an empty root still starts empty,
                // rather than inventing the starter company that only the
                // packaged desktop's `bootstrap_companies` seeds.
                let adopted = opencompany::desktop::adopt_companies(&state).await?;
                for (id, manifest) in &adopted {
                    let slug = id.as_ref();
                    if let Some(handle) =
                        spawn_scheduler(&state, slug, &manifest.schedules, &shutdown)
                    {
                        scheduler_handles.push(handle);
                    }
                    spawn_mailbox_poller(&state, slug, &shutdown, &mut scheduler_handles);
                    println!(
                        "adopted company `{slug}` ({}) from {}",
                        manifest.company.name,
                        home.display()
                    );
                }
                if adopted.is_empty() {
                    println!("serving with no companies; pass --company <dir> to load one");
                }
            }

            // Take the port BEFORE anything is reported. `instance_started`
            // means an instance that started, and a host whose address is
            // occupied or malformed never listened and never answered
            // `/healthz` — but the shutdown flush below still runs on the way
            // out, so an event queued above the bind would be sent for a
            // process that served nothing, and hosted install counts would
            // include every crash-looping container.
            //
            // Binding here rather than inside `server::serve` also fails fast:
            // a refused address now aborts before four background tasks are
            // spawned, instead of after. Nothing between this and
            // `serving.run()` can return early, so the port is never taken and
            // then abandoned. The state is cloned rather than moved because
            // `install_analytics` below needs it — `AppState` is `Clone` by
            // design, and its registry is `Arc`-shared, so both handles see the
            // same companies.
            let (_bound, serving) =
                opencompany::server::bind(&state.config().bind, state.clone()).await?;

            // Issue #1739: with the companies registered and the port taken,
            // this host knows who it is, which brain it is on and how much it is
            // serving — so the tracker can be chosen and `instance_started`
            // reported. On every build but a hosted tenant's this installs a
            // no-op and the line below says so.
            println!(
                "{}",
                opencompany::analytics::boot::describe(&opencompany::analytics::install_analytics(
                    &state,
                    analytics.as_ref(),
                    &opencompany::app::config::ProcessEnv,
                ))
            );

            // One workflow scheduler for the whole process, started even with no
            // companies loaded: it re-reads the registry each minute, so a
            // company registered later is picked up without a restart.
            scheduler_handles.push(spawn_workflow_scheduler(&state, &shutdown));

            // And one maintenance ticker, for the same reason and started the
            // same way (issue #971). This is the only place approvals, grants
            // and fire claims are retired, and it covers a company registered
            // after boot — which the per-company scheduler spawn above does not.
            scheduler_handles.push(spawn_maintenance_ticker(&state, &shutdown));
            scheduler_handles.push(spawn_presence_sweeper(&state, &shutdown));

            // Issue #1845: the week-1 nudge, process-wide and always started —
            // same reasoning as the workflow scheduler and maintenance ticker
            // above. `state.connections()` is already wired (line above the
            // registration loop), so the scheduler sees a real mail sender
            // whenever `OPENCOMPANY_MAIL_*` + `smtp` are configured.
            scheduler_handles.push(spawn_lifecycle_scheduler(&state, &shutdown));

            // Stop the schedulers on a termination signal so background cycle
            // work halts with the process (lifecycle shutdown).
            //
            // `SIGTERM` as well as `Ctrl-C` since issue #986: a hosted tenant is
            // only ever asked to stop by `SIGTERM`, so listening for `SIGINT`
            // alone meant the schedulers kept firing new cycles right through
            // the shutdown the server was busy draining. Both listeners resolve
            // on the same signal — tokio delivers it to every registration — so
            // this and `serve`'s drain start together rather than in sequence.
            {
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    opencompany::server::shutdown::signal().await;
                    shutdown.notify_waiters();
                });
            }

            // Name the address *and* the layer that chose it. An operator who
            // set `OPENCOMPANY_BIND` and finds the host elsewhere can read the
            // disagreement off this one line instead of inferring it from a
            // refused connection. `println!` for the same reason as the lines
            // above — the default `EnvFilter` would swallow an `info!`.
            println!("listening on {} (from {bind_source})", state.config().bind);
            let served = serving.run().await;
            // Issue #1739: after the server has drained, so anything a
            // last-moment turn reported still leaves — and bounded, because the
            // drain budget is the whole point. The client's own 5s timeout is
            // not a bound on *this*: added to the 25s drain and the 2s
            // connection grace it takes the worst case to 32s, past the 30s
            // those two were sized to fit inside, so a slow collector during a
            // rollout would have bought a `SIGKILL` mid-shutdown. Giving up the
            // batch is the right trade — a dropped batch costs a line in a
            // dashboard, an overrun costs a half-finished turn.
            if tokio::time::timeout(
                opencompany::server::shutdown::flush_budget(
                    opencompany::server::shutdown::grace_from_env(),
                ),
                opencompany::analytics::Tracker::flush(analytics.as_ref()),
            )
            .await
            .is_err()
            {
                tracing::debug!("analytics: flush did not finish inside the shutdown budget");
            }
            served
        }
        Some(Command::Spec { openhuman_root }) => {
            let state = AppState::new(AppConfig {
                openhuman_root,
                ..AppConfig::default()
            });
            println!("{}", serde_json::to_string_pretty(&state.spec()).unwrap());
            Ok(())
        }
        Some(Command::Check { path }) => {
            if opencompany::company::run_check(&path) {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        Some(Command::Prompt {
            company,
            agents,
            raw,
            json,
            out,
        }) => run_prompt(&company, &agents, raw, json, out.as_deref()),
        Some(Command::Doctor { company, json }) => {
            let env = ProcessEnv;
            // Locate config.toml under the resolved data dir (env override or
            // the default `$HOME/.opencompany`).
            let config_dir = opencompany::app::config::data_dir_from_env();
            let config_toml = ConfigFile::load(&config_dir)?;
            let manifest = match &company {
                Some(dir) => CompanyManifest::from_path(dir)?,
                None => toml::from_str("[company]\nname = \"opencompany\"\n")
                    .expect("synthetic manifest is valid"),
            };
            let (cfg, prov) = resolve(&env, config_toml.as_ref(), &manifest)?;
            let report = doctor::report(&cfg, &prov);
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print!("{}", report.to_text());
            }
            Ok(())
        }
        Some(Command::IssuePassword {
            company,
            email,
            password,
            no_change_required,
            home,
        }) => run_issue_password(company, email, password, !no_change_required, home).await,
        Some(Command::Orphans { home, json }) => run_orphans(home, json).await,
        Some(Command::Export {
            company,
            out,
            include_secrets,
            home,
        }) => run_export(company, out, include_secrets, home).await,
        Some(Command::Import { path, home }) => run_import(path, home).await,
        Some(Command::Memory { cmd }) => run_memory_cmd(cmd).await,
        Some(Command::OpenHuman {
            root,
            mode,
            release,
            dry_run,
            args,
        }) => run_openhuman(root, mode, release, dry_run, args).await,
        None => {
            // The commit as well as the version: `0.1.0` has been thousands of
            // commits wide, so this line could not tell an operator which
            // build they are actually holding.
            println!(
                "opencompany {} ({})",
                opencompany::VERSION,
                opencompany::BUILD_COMMIT
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // PR #1875 review finding: `activation_gate_bypass_enabled` guards
    // whether a company's activation-funnel gate is bypassed at first boot
    // (issue #1844). Only the literal `"1"` may enable it — unset, and
    // anything that merely *looks* truthy, must stay disabled so a typo in
    // the e2e host script's env fails closed onto the real gate.
    #[test]
    fn activation_gate_bypass_disabled_when_unset() {
        assert!(!activation_gate_bypass_enabled(None));
    }

    #[test]
    fn activation_gate_bypass_enabled_on_literal_one() {
        assert!(activation_gate_bypass_enabled(Some("1")));
    }

    #[test]
    fn activation_gate_bypass_disabled_on_truthy_lookalike() {
        assert!(!activation_gate_bypass_enabled(Some("true")));
    }

    #[test]
    fn activation_gate_bypass_disabled_on_zero() {
        assert!(!activation_gate_bypass_enabled(Some("0")));
    }

    #[tokio::test]
    async fn openhuman_desktop_dry_run_rejects_passthrough_args() {
        // The handler validates before the dry-run branch, so `--dry-run`
        // with passthrough args in Desktop mode reports the same 400 as a
        // real launch instead of printing a command that could never run.
        let tmp = std::env::temp_dir().join(format!(
            "oc-bin-oh-desktop-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let err = run_openhuman(
            tmp.clone(),
            ModeArg::Desktop,
            false,
            true,
            vec!["--flag".into()],
        )
        .await
        .unwrap_err();
        assert!(matches!(
            &err,
            opencompany::OpenCompanyError::OpenHuman { code: 400, .. }
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_home_flag_is_taken_verbatim() {
        // The binary owns no home policy of its own: it delegates to
        // `store::resolve_home`, whose precedence chain (flag >
        // OPENCOMPANY_DATA_DIR > $HOME/.opencompany) is covered in
        // `src/store/paths.rs`. This only pins the wiring.
        assert_eq!(
            opencompany::store::resolve_home(Some(PathBuf::from("/flag"))).unwrap(),
            PathBuf::from("/flag")
        );
    }

    #[test]
    fn every_home_resolving_command_migrates_the_legacy_nest() {
        // `serve`, `export`, and `import` all resolve through
        // `resolve_home_migrated`, so an un-migrated install's first
        // post-upgrade command is not the one that finds no bundles.
        let home = std::env::temp_dir().join(format!(
            "oc-bin-migrate-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let nested = home.join("companies/companies/acme");
        std::fs::create_dir_all(&nested).expect("legacy bundle");
        std::fs::write(nested.join("company.toml"), "[company]\n").expect("manifest");

        let resolved = resolve_home_migrated(Some(home.clone())).expect("resolves and migrates");

        assert_eq!(resolved, home);
        assert!(home.join("companies/acme/company.toml").exists());
        assert!(!home.join("companies/companies").exists());
        // The wiring is what is under test, but the bundle path it produces is
        // the point of the whole change.
        assert_eq!(
            opencompany::store::Bundle::new(resolved, &CompanyId::new("acme"))
                .dir()
                .to_path_buf(),
            home.join("companies/acme")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn no_command_resolves_a_home_without_migrating_it() {
        // The test above pins the helper; this pins that the helper is the only
        // door. A command that called `store::resolve_home` directly would read
        // an un-migrated install and find no companies — and no runtime test
        // would catch it, because the defect is a call that never happens. The
        // needle is split so this assertion does not match its own source line.
        let needle = concat!("store::", "resolve_home(");
        let source = include_str!("opencompany.rs");
        let production = source.split("\nmod test {").next().unwrap_or(source);

        let direct: Vec<&str> = production
            .lines()
            .filter(|line| line.contains(needle))
            .collect();

        assert_eq!(
            direct.len(),
            1,
            "`resolve_home` belongs to `resolve_home_migrated` alone; found {direct:?}"
        );
        let (before_helper, _) = production
            .split_once("fn resolve_home_migrated")
            .expect("the helper is declared");
        assert!(
            !before_helper.contains(needle),
            "the one call must be the one inside `resolve_home_migrated`"
        );
    }

    #[tokio::test]
    async fn export_and_import_migrate_before_they_read() {
        // Both commands run against an install whose first post-upgrade command
        // may well be one of them, so neither may be the one that finds no
        // bundles. Their results are irrelevant here — the migration happens
        // before either touches a path, which is the whole point.
        for command in ["export", "import"] {
            let home = std::env::temp_dir().join(format!(
                "oc-bin-{command}-migrate-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&home);
            let nested = home.join("companies/companies/acme");
            std::fs::create_dir_all(&nested).expect("legacy bundle");
            std::fs::write(nested.join("company.toml"), "[company]\n").expect("manifest");

            match command {
                "export" => {
                    let out = home.join("out");
                    let _ =
                        run_export("acme".to_string(), Some(out), false, Some(home.clone())).await;
                }
                _ => {
                    let _ = import_from_dir(&home.join("nothing-here"), Some(home.clone())).await;
                }
            }

            assert!(
                home.join("companies/acme/company.toml").exists(),
                "`{command}` resolved a home without migrating it"
            );
            assert!(!home.join("companies/companies").exists());
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn live_ports_layers_the_config_file_memory_section() {
        // A self-hosted operator's engine selection lives only in `config.toml`
        // (the console never exports environment variables), so `live_ports`
        // must layer that section the way `serve` does — otherwise a bundle
        // would read and write the base stores instead of the engine the host
        // actually remembers with. This pins the load-and-layer composition
        // `live_ports` feeds its settings through.
        let tmp = std::env::temp_dir().join(format!(
            "oc-bin-memcfg-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("config.toml"),
            "[memory]\nbackend = \"remote\"\ndriver = \"supermemory\"\nurl = \"https://memory.example\"\n",
        )
        .unwrap();
        let settings =
            layer_config_memory(opencompany::store::StorageSettings::default(), &tmp).unwrap();
        assert_eq!(
            settings.memory_backend,
            opencompany::store::MemoryBackend::Remote
        );
        assert_eq!(settings.memory_driver.as_deref(), Some("supermemory"));
        assert_eq!(
            settings.memory_url.as_deref(),
            Some("https://memory.example")
        );

        // The absent-file shape (a fresh root, or the fs default) stays on the
        // base backend's own memory.
        let absent = std::env::temp_dir().join(format!(
            "oc-bin-memcfg-absent-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&absent);
        std::fs::create_dir_all(&absent).unwrap();
        let settings =
            layer_config_memory(opencompany::store::StorageSettings::default(), &absent).unwrap();
        assert_eq!(
            settings.memory_backend,
            opencompany::store::MemoryBackend::Store
        );
        let _ = std::fs::remove_dir_all(&absent);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn register_company_loads_manifest_and_registers() {
        let home = std::env::temp_dir().join(format!("oc-bin-{}", std::process::id()));
        let state = AppState::new(AppConfig::default());
        let dir = std::path::Path::new("companies/agentic_law_firm");

        let (id, name, _schedules) = register_company(&state, &home, dir, false).await.unwrap();

        assert_eq!(name, "Agentic Law Firm");
        assert_eq!(id, "agentic-law-firm");
        assert_eq!(state.registry().list().len(), 1);
        let runtime = state.registry().sole().expect("sole company");

        // The serve path records the source dir and seeds the workspace from
        // `companies/<name>/workspace/**` on first boot.
        assert_eq!(runtime.source_dir(), Some(dir));
        assert!(
            !runtime.workspace().is_empty(runtime.id()).await.unwrap(),
            "workspace seeded from the company source dir"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn company_source_dir_normalizes_manifest_file_to_its_directory() {
        let dir = std::path::Path::new("companies/agentic_law_firm");
        // A directory argument is returned unchanged.
        assert_eq!(company_source_dir(dir), dir);
        // A manifest-file argument resolves to its parent company directory, so
        // the serve-path `workspace`/`skills`/`workflows` lookups stay correct.
        assert_eq!(company_source_dir(&dir.join("company.toml")), dir);
    }

    #[tokio::test]
    async fn register_company_accepts_a_manifest_file_path() {
        let home = std::env::temp_dir().join(format!("oc-bin-file-{}", std::process::id()));
        let state = AppState::new(AppConfig::default());
        // `--company` also accepts the manifest file inside the company dir.
        let file = std::path::Path::new("companies/agentic_law_firm/company.toml");

        let (_id, name, _schedules) = register_company(&state, &home, file, false).await.unwrap();

        assert_eq!(name, "Agentic Law Firm");
        let runtime = state.registry().sole().expect("sole company");
        // The recorded source dir is the company directory, not `company.toml`.
        assert_eq!(
            runtime.source_dir(),
            Some(std::path::Path::new("companies/agentic_law_firm"))
        );
        assert!(
            !runtime.workspace().is_empty(runtime.id()).await.unwrap(),
            "workspace still seeds when --company is a manifest file"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Issue #85: launching from a template *directory* derives the provenance
    /// from that directory's basename — `source_id` and `path` both the slug,
    /// `version` faithfully `None` (the serve path exposes no template version).
    /// Distinct from the builder-injection test in `runtime::builder`: this
    /// exercises the serve-path derivation in `register_company` itself. Per
    /// 8b40fa7 the stamped `path` is the basename, never the absolute host path.
    #[tokio::test]
    async fn register_company_stamps_provenance_from_directory() {
        let home = std::env::temp_dir().join(format!("oc-prov-dir-{}", std::process::id()));
        let state = AppState::new(AppConfig::default());
        let dir = std::path::Path::new("companies/agentic_law_firm");

        register_company(&state, &home, dir, false).await.unwrap();

        let runtime = state.registry().sole().expect("sole company");
        let record = runtime
            .store()
            .load(runtime.id())
            .await
            .unwrap()
            .expect("persisted record");
        let provenance = record
            .template_provenance
            .expect("a directory launch stamps template provenance");
        assert_eq!(provenance.source_id, "agentic_law_firm");
        assert_eq!(provenance.version, None, "serve path records no version");
        assert_eq!(
            provenance.path.as_deref(),
            Some("agentic_law_firm"),
            "path is the template basename, not the absolute host path"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Issue #85: launching from a `company.toml` *file* path normalizes to its
    /// parent company directory before deriving provenance, so the stamped
    /// provenance matches the directory launch exactly (basename `source_id` +
    /// `path`, `None` version). Guards the file-path input shape of the
    /// serve-path derivation.
    #[tokio::test]
    async fn register_company_stamps_provenance_from_manifest_file() {
        let home = std::env::temp_dir().join(format!("oc-prov-file-{}", std::process::id()));
        let state = AppState::new(AppConfig::default());
        let file = std::path::Path::new("companies/agentic_law_firm/company.toml");

        register_company(&state, &home, file, false).await.unwrap();

        let runtime = state.registry().sole().expect("sole company");
        let record = runtime
            .store()
            .load(runtime.id())
            .await
            .unwrap()
            .expect("persisted record");
        let provenance = record
            .template_provenance
            .expect("a manifest-file launch stamps provenance from the parent dir");
        assert_eq!(provenance.source_id, "agentic_law_firm");
        assert_eq!(provenance.version, None);
        assert_eq!(
            provenance.path.as_deref(),
            Some("agentic_law_firm"),
            "path is the template basename, not the absolute host path"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Records the target and level of every event a subscriber was actually
    /// asked to record, so a filter can be tested on behaviour rather than on
    /// the contents of its own string.
    #[derive(Clone)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<(String, tracing::Level)>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Captured {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let meta = event.metadata();
            self.0
                .lock()
                .expect("capture lock")
                .push((meta.target().to_string(), *meta.level()));
        }
    }

    #[test]
    fn the_default_filter_passes_durable_append_warnings_and_still_drops_other_ones() {
        // The point of `DEFAULT_LOG_FILTER` is the vendored append worker's
        // warn-level reports — "still failing after N", "recovered, N lost",
        // "never recovered before shutdown, N lost". They are the only account
        // of how much of the durable agent journal was lost, and a bare `error`
        // filter drops all three (issue #450).
        //
        // Asserted by running the filter, not by reading it: this builds the
        // real `EnvFilter` from the real constant, installs it over a capturing
        // layer exactly as `async_main` installs it over `fmt`, and emits the
        // four events that matter. Revert the constant to `"error"` and the
        // first assertion fails.
        use tracing_subscriber::layer::SubscriberExt;

        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(Captured(std::sync::Arc::clone(&captured)))
            .with(log_filter(None));

        tracing::subscriber::with_default(subscriber, || {
            // The worker's recovery summary — the line the operator needs.
            tracing::warn!(
                target: "tinyagents::observability",
                sink = "journal",
                lost = 3_u64,
                "durable append recovered"
            );
            // An unrelated warning stays dropped: the default is still `error`
            // for everything the exception does not name.
            tracing::warn!(target: "opencompany::unrelated", "ordinary warning");
            // And a real error still gets through, unchanged from before.
            tracing::error!(target: "opencompany::unrelated", "ordinary error");
            // The exception is scoped to `warn`, not opened wide.
            tracing::info!(target: "tinyagents::observability", "chatter");
        });

        let events = captured.lock().expect("capture lock").clone();
        let seen = |target: &str, level: tracing::Level| {
            events.iter().any(|(t, l)| t == target && *l == level)
        };

        assert!(
            seen("tinyagents::observability", tracing::Level::WARN),
            "the durable-append recovery/reminder/shutdown reports must survive \
             the default filter; captured {events:?}"
        );
        assert!(
            !seen("opencompany::unrelated", tracing::Level::WARN),
            "the exception is one target, not a global level bump; captured {events:?}"
        );
        assert!(
            seen("opencompany::unrelated", tracing::Level::ERROR),
            "errors kept passing exactly as they did before; captured {events:?}"
        );
        assert!(
            !seen("tinyagents::observability", tracing::Level::INFO),
            "the exception stops at `warn`; captured {events:?}"
        );
    }

    /// A `RUST_LOG` the operator set is theirs, even when part of it is junk.
    ///
    /// `try_from_default_env` rejects the whole variable over one malformed
    /// directive, so falling back to [`DEFAULT_LOG_FILTER`] on that error would
    /// throw away every valid directive the operator wrote — turning a typo into
    /// a silent, total loss of their logging configuration. [`log_filter`] parses
    /// it lossily instead, which is what this binary did before the constant
    /// existed. Flagged by review on PR #1186.
    #[test]
    fn a_malformed_directive_does_not_discard_the_rest_of_rust_log() {
        use tracing_subscriber::layer::SubscriberExt;

        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // One good directive, one that cannot parse.
        let subscriber = tracing_subscriber::registry()
            .with(Captured(std::sync::Arc::clone(&captured)))
            .with(log_filter(Some(
                "opencompany::kept=info,@@@not a directive@@@",
            )));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "opencompany::kept", "the operator asked for this");
        });

        let events = captured.lock().expect("capture lock").clone();
        assert!(
            events
                .iter()
                .any(|(t, l)| t == "opencompany::kept" && *l == tracing::Level::INFO),
            "the valid directive must survive the invalid one beside it; captured {events:?}"
        );
    }

    /// `RUST_LOG=` — set, but empty — must not silence the binary.
    ///
    /// This is the sharper half of the same mistake as
    /// `a_malformed_directive_does_not_discard_the_rest_of_rust_log`, and it is
    /// worth its own test because it fails in the opposite direction from the
    /// bug this file's constant exists to fix. `from_default_env` carries an
    /// `ERROR` default directive; `try_from_default_env` carries none, so an
    /// empty value parsed strictly yields a filter with no directives at all
    /// and drops **everything** — errors included. That is worse than the
    /// silence issue #450 is about, and it is reachable from an empty
    /// environment variable in a compose file or a shell export.
    #[test]
    fn an_empty_rust_log_still_reports_errors() {
        use tracing_subscriber::layer::SubscriberExt;

        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(Captured(std::sync::Arc::clone(&captured)))
            .with(log_filter(Some("")));

        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "opencompany::anything", "something broke");
        });

        let events = captured.lock().expect("capture lock").clone();
        assert!(
            events
                .iter()
                .any(|(t, l)| t == "opencompany::anything" && *l == tracing::Level::ERROR),
            "an empty RUST_LOG must keep the ERROR default, not silence the binary; \
             captured {events:?}"
        );
    }

    /// Issue #1077: the `orphans` command refuses to run without a tenant
    /// namespace.
    ///
    /// Without `OPENCOMPANY_TENANT_ID` no durable owner rows are ever written,
    /// so the report would claim every company is orphaned on every run. The
    /// gate is the same condition that guards the owner-row write at
    /// `register_company`, and it fires before storage is even opened — the
    /// reviewer's false-positive case, with no database needed to hit it.
    #[tokio::test]
    async fn orphans_refuses_to_run_without_a_tenant_namespace() {
        let err = run_orphans_from(None, false, &opencompany::app::config::MapEnv::default())
            .await
            .unwrap_err();

        assert!(
            matches!(&err, opencompany::error::OpenCompanyError::Config(_)),
            "expected a Config refusal, got: {err:?}"
        );
    }
}

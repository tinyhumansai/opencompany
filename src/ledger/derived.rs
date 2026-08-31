//! The `derived/` folder: ledger files written by code, never by hand.
//!
//! Every ledger renders into one Markdown file under
//! [`DERIVED_DIR`](super::spec::DERIVED_DIR) in the company's shared workspace,
//! rewritten on every write to that ledger. That is what makes a ledger legible
//! to everything that already reads the workspace — an agent's file tools, the
//! console's workspace view, a search, an export — without any of them learning
//! a new API.
//!
//! # Why the folder is the invariant
//!
//! The rule is not *these particular files are generated*; it is **nothing in
//! this folder is hand-written**. A per-file rule fails open: a ledger declared
//! next week renders a file no guard has heard of, an agent edits it, the edit
//! is silently erased by the next derivation, and the agent has no way to know.
//! A folder rule fails closed — a path under `derived/` is refused whether or
//! not a ledger currently claims it, and the refusal names the tool that
//! actually writes the row.
//!
//! [`refusal`] is that message. It is deliberately per-ledger rather than
//! generic, because *what to do instead* is not the same for every ledger: an
//! events ledger takes `record_entry`, and the task board does not — telling
//! the board's caller otherwise sends them to a tool that refuses them a second
//! time, which is barely better than refusing them with no remedy at all.

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};
use crate::ports::{generate_id, now_millis};

use super::registry::Registry;
use super::spec::{DERIVED_DIR, LedgerSpec};

/// Whether `path` lives in the derived folder, whatever it names.
///
/// Accepts the folder itself and everything under it, with or without a leading
/// slash, and case-insensitively on the folder segment — a guard that can be
/// stepped around by capitalising a letter is not a guard.
pub fn is_derived_path(path: &str) -> bool {
    let trimmed = path.trim().trim_start_matches('/');
    let Some(head) = trimmed.split('/').next() else {
        return false;
    };
    head.eq_ignore_ascii_case(DERIVED_DIR)
}

/// The message a refused write to `path` carries.
///
/// Names the ledger that owns the file and how that ledger is actually written,
/// when one owns it; falls back to the folder's rule when nothing does, which
/// is the case for a stale file left behind by a retired ledger.
pub fn refusal(registry: &Registry, path: &str) -> String {
    match registry.owner_of_derived(path) {
        Some(spec) => format!(
            "`{path}` is written by the `{}` ledger and is re-derived on every write to it — an \
             edit here would be erased without anything saying so. Write it with {}.",
            spec.slug, spec.written_by
        ),
        None => format!(
            "`{path}` is under `{DERIVED_DIR}/`, which is written by code and never by hand: the \
             next derivation would erase this edit without anything saying so. List the ledgers \
             with `list_ledgers` and record into the one that owns this file."
        ),
    }
}

/// Refuses a write to a derived path, or lets it through.
///
/// # Errors
///
/// Returns [`OpenCompanyError::InvalidRequest`] when `path` is under
/// [`DERIVED_DIR`], carrying [`refusal`]'s message.
pub fn guard(registry: &Registry, path: &str) -> Result<()> {
    if is_derived_path(path) {
        return Err(OpenCompanyError::InvalidRequest(refusal(registry, path)));
    }
    Ok(())
}

/// The file name a derived path ends in (`derived/goals.md` → `goals.md`).
fn file_name(spec: &LedgerSpec) -> &str {
    spec.derived
        .rsplit('/')
        .next()
        .unwrap_or(spec.derived.as_str())
}

/// Writes a ledger's rendered file into the workspace, creating the folder and
/// the file the first time.
///
/// Best-effort by contract: the ledger's own store is the record, and the
/// derived file is a projection of it. A workspace that refused the write —
/// quota, a permission, a backend hiccup — must not fail the `record_entry`
/// that produced it, because a row that was recorded and then reported as
/// failed is the one outcome a caller cannot recover from. The error is
/// returned so a caller that *is* the derivation (a rebuild) can report it, and
/// the write path logs it instead.
///
/// # Errors
///
/// Returns whatever the [`WorkspaceStore`] returns.
pub async fn publish(
    workspace: &dyn WorkspaceStore,
    company: &CompanyId,
    spec: &LedgerSpec,
    body: &str,
) -> Result<()> {
    let folder = workspace
        .adopt_or_create_folder(company, None, DERIVED_DIR, WorkspaceOrigin::Seed)
        .await?;
    let name = file_name(spec);
    // Matched on the normalized name, not the literal one. The derived file is
    // `derived/tasks.md` under the workspace naming rule
    // ([`crate::company::workspace_names`]) and was `derived/tasks.md` before
    // it — so a literal match would leave the old file in place, untouched and
    // stale, while a second one appeared beside it carrying the live rows. Two
    // files claiming to be one ledger is exactly the ambiguity `derived/` exists
    // to prevent.
    let canonical = crate::company::workspace_names::kebab_name(name);
    let existing = workspace.tree(company).await?.into_iter().find(|node| {
        node.kind == NodeKind::File
            && node.parent_id.as_deref() == Some(folder.node().id.as_str())
            && (node.name == name
                || crate::company::workspace_names::kebab_name(&node.name) == canonical)
    });
    match existing {
        Some(node) => {
            workspace
                .write(company, &node.id, body, WorkspaceOrigin::Seed)
                .await?;
        }
        None => {
            let node = WorkspaceNode {
                id: generate_id(),
                name: name.to_string(),
                kind: NodeKind::File,
                parent_id: Some(folder.node().id.clone()),
                updated_at_millis: now_millis(),
                // `Seed` rather than an operator or an agent, deliberately: the
                // file's author is the runtime's derivation, and stamping
                // whoever happened to record the row that triggered it would
                // credit a person for a file they did not write and cannot
                // edit.
                created_by: WorkspaceOrigin::Seed,
                updated_by: WorkspaceOrigin::Seed,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
            };
            workspace.create(company, &node, Some(body)).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn the_whole_folder_is_guarded_not_the_files_in_it() {
        assert!(is_derived_path("derived/goals.md"));
        assert!(is_derived_path("/derived/goals.md"));
        assert!(is_derived_path("derived"));
        // A ledger declared next week renders a file no guard has heard of.
        assert!(is_derived_path("derived/SOMETHING-NOBODY-HAS-DECLARED.md"));
        // Case is not a way around it.
        assert!(is_derived_path("Derived/goals.md"));
        assert!(!is_derived_path("notes/derived/goals.md"));
        assert!(!is_derived_path("derivedish/goals.md"));
        assert!(!is_derived_path("goals.md"));
    }

    /// The refusal has to name a remedy the caller can actually follow. The
    /// board's is not `record_entry`, and saying it was would send them to a
    /// tool that refuses them a second time.
    #[test]
    fn the_refusal_names_the_owning_ledgers_real_write_path() {
        let registry = Registry::build([]);
        let board = refusal(&registry, "derived/tasks.md");
        assert!(board.contains("tasks"), "{board}");
        assert!(
            !board.contains("`record_entry` to add"),
            "the board does not take record_entry: {board}"
        );
        let goals = refusal(&registry, "derived/goals.md");
        assert!(goals.contains("record_entry"), "{goals}");
    }

    #[test]
    fn an_unclaimed_derived_file_is_still_refused() {
        let registry = Registry::build([]);
        let message = refusal(&registry, "derived/LEFTOVER.md");
        assert!(message.contains("list_ledgers"), "{message}");
        assert!(guard(&registry, "derived/LEFTOVER.md").is_err());
        assert!(guard(&registry, "notes/plan.md").is_ok());
    }
}

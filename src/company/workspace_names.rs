//! The one naming rule for everything the runtime puts in a workspace:
//! **lowercase, dashed**.
//!
//! A workspace mixed `Agents/`, `Playbooks/Close checklist.md`, `Page.tsx` and
//! `page.toml` — three conventions in one tree, none of them stated anywhere.
//! That is not only untidy. Identity in the workspace is *by path*, so the
//! shape of a name is load-bearing:
//!
//! * A path with a space in it needs quoting in every place a human or an agent
//!   types one — a tool argument, a `[[wikilink]]`, a URL, a shell command.
//! * `Close checklist.md` and `Close Checklist.md` are two distinct nodes on
//!   the sqlite and mongodb backends and one node on a case-insensitive
//!   filesystem, so the same tree means different things per backend.
//! * An agent asked to "put it in Playbooks" has to guess the capitalization,
//!   and a wrong guess mints a rival folder rather than failing.
//!
//! Collapsing the alphabet to `[a-z0-9]`, `-` and `.` removes all three
//! problems at once: one spelling per name, no quoting anywhere, and the same
//! meaning on every backend.
//!
//! # Where the rule is applied
//!
//! At every point the runtime *mints* a name — the system roots, an agent's own
//! folder, a page's files, the folders and file a publish mirrors into, and the
//! names offered by the workspace write tools. Normalizing at the boundary is
//! the same call issue #580 made for workflow ids: the host owns the name so
//! the model cannot pick an unsafe or unspellable one, and the tool reply says
//! where the write actually landed.
//!
//! # Where it is deliberately *not* applied
//!
//! To nodes that already exist. Renaming a tenant's tree on boot is the thing
//! issues #570, #645, #700 and #759 each refused to do — an operator must not
//! find their workspace rearranged by an upgrade they did not ask for, and a
//! rename breaks every reference somebody has kept to the old name.
//!
//! Existing trees therefore keep their names, and every reader is widened to
//! reach them instead:
//!
//! * [`workspace_scaffold::find`](super::workspace_scaffold::find) matches a
//!   name case-insensitively, so a legacy `Agents/` root is *adopted* rather
//!   than joined by a lowercase twin — which would split one agent's home in
//!   two. The agent-folder minter additionally adopts the roster id spelled
//!   verbatim (`page_builder` before `page-builder`).
//! * The agent tools' path index carries a normalized key beside the literal
//!   one, so `playbooks/close-checklist.md` and `Playbooks/Close checklist.md`
//!   name the same note whichever one is typed.
//! * Context routing, the page tools and the pages route match the same way,
//!   for the same reason.
//!
//! So an old tree keeps working, a new one is uniform, and a mixed one resolves
//! either spelling. Converting an existing tree in place is deliberately left
//! as an operator action, and there is no such action yet: it belongs beside the
//! [`workspace_repair`](super::workspace_repair) pass, whose dry-run-then-apply
//! shape it needs, rather than in this rule.

/// The name a node gets when its raw name normalizes to nothing at all — an
/// emoji, a run of punctuation, an empty string.
///
/// Callers that have something better (an id, a slug) pass it to
/// [`kebab_name_or`]; this is the last resort, so that a node always has a name
/// and the write never fails on a name the caller cannot fix.
pub const FALLBACK_NAME: &str = "untitled";

/// The longest name this produces, in bytes.
///
/// Well under every backend's limit (255 on the filesystem, unbounded in
/// sqlite/mongodb) and long enough that truncation is not something a real
/// title runs into. Bounded at all because a name is a path segment, and a path
/// is assembled from several of them.
pub const MAX_NAME_BYTES: usize = 96;

/// Normalizes one workspace node name to the lowercase-dashed rule.
///
/// * ASCII letters lowercase; digits kept.
/// * `.` kept, so an extension survives (`Page.compiled.mjs` →
///   `page.compiled.mjs`) — but never as the first character, so this can
///   neither produce a hidden file nor `.`/`..`.
/// * Everything else — spaces, `_`, `&`, `/`, punctuation, non-ASCII — becomes a
///   single `-`, and runs collapse.
/// * Dashes never sit beside a dot, at either end (`Q2 close .md` →
///   `q2-close.md`).
///
/// Returns [`FALLBACK_NAME`] rather than an empty string: a name is a path
/// segment, and an empty segment is not addressable.
pub fn kebab_name(raw: &str) -> String {
    kebab_name_or(raw, FALLBACK_NAME)
}

/// [`kebab_name`] with a caller-chosen fallback for a name that normalizes to
/// nothing — an id, usually, so the node stays identifiable rather than joining
/// every other unnameable node at `untitled`.
///
/// The fallback is normalized too, so a caller cannot smuggle a name past the
/// rule by handing it in as the fallback.
pub fn kebab_name_or(raw: &str, fallback: &str) -> String {
    match normalize(raw) {
        Some(name) => name,
        None => normalize(fallback).unwrap_or_else(|| FALLBACK_NAME.to_string()),
    }
}

/// Whether `name` is already in the canonical form — i.e. whether
/// [`kebab_name`] would leave it alone.
///
/// The predicate the repair pass filters on, so "needs renaming" and "was
/// renamed to" cannot disagree: it is defined *as* a fixed-point test rather
/// than as a second transcription of the grammar above.
pub fn is_kebab_name(name: &str) -> bool {
    normalize(name).is_some_and(|normalized| normalized == name)
}

/// Normalizes every segment of a `/`-separated logical path, dropping empty
/// segments.
///
/// For the publish mirror, whose `source` is a whole relative path
/// (`specs/Launch Plan.md`) that becomes a chain of folders plus a file.
pub fn kebab_path(path: &str) -> String {
    path.split('/')
        .filter(|segment| !segment.trim().is_empty())
        .map(kebab_name)
        .collect::<Vec<_>>()
        .join("/")
}

/// The extension of a name: what follows its last dot, when that dot is not the
/// first character and at least one ASCII alphanumeric follows it. `None` for
/// a name with no extension — `.hidden` has none (leading dot) and `dir.` has
/// none (nothing after the dot).
fn trailing_extension(raw: &str) -> Option<&str> {
    let dot = raw.rfind('.')?;
    let extension = &raw[dot + 1..];
    if dot == 0 || extension.is_empty() || !extension.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(extension)
}

/// The shared implementation. `None` means "nothing survived", which is the
/// distinction [`is_kebab_name`] and [`kebab_name_or`] both need and a `String`
/// return would flatten into the fallback.
fn normalize(raw: &str) -> Option<String> {
    // The trailing extension is reserved before the loop runs. A long-named
    // upload must keep the suffix that identifies its format: the stored name
    // keys both `resolve_mime` and `ingest::extract` dispatch, so a name that
    // sheds its `.docx` at the cap is refused as unreadable at recall time.
    let extension = trailing_extension(raw).map(str::to_ascii_lowercase);

    let mut out = String::with_capacity(raw.len().min(MAX_NAME_BYTES));
    // Suppresses a dash run, and any dash that would lead the name.
    let mut pending_dash = false;
    // True when the budget stopped the loop before the end of `raw` — the only
    // case where the reserved extension has to be grafted back on.
    let mut truncated = false;

    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            let dash = usize::from(pending_dash && !out.is_empty());
            // Budget checked before the push — including the separator the push
            // would need — so the result never exceeds the cap, and never ends
            // on a separator the next character was going to justify.
            if out.len() + dash + 1 > MAX_NAME_BYTES {
                truncated = true;
                break;
            }
            if dash == 1 {
                out.push('-');
            }
            pending_dash = false;
            out.extend(ch.to_lowercase());
        } else if ch == '.' {
            if out.len() + 1 > MAX_NAME_BYTES {
                truncated = true;
                break;
            }
            // A dot only counts once something precedes it, so a leading dot is
            // dropped rather than producing a hidden file or a `.`/`..` segment.
            if !out.is_empty() {
                // A dash immediately before a dot is noise, not separation.
                pending_dash = false;
                while out.ends_with('-') {
                    out.pop();
                }
                if !out.is_empty() && !out.ends_with('.') {
                    out.push('.');
                }
            }
        } else {
            pending_dash = true;
        }
    }

    // A trailing dash or dot is separation with nothing after it.
    while out.ends_with('-') || out.ends_with('.') {
        out.pop();
    }

    // The cap stopped the loop with the extension unread — possibly mid-way
    // into it. Trim the basename to the budget for `basename.ext` (popping any
    // partial extension, and any separator left at the cut) and graft the
    // reserved suffix back on whole, so the stored name still names its format.
    // A suffix that could eat the whole budget is not an extension; the plain
    // truncated basename is kept instead.
    if truncated && let Some(ext) = extension {
        let suffix = format!(".{ext}");
        if suffix.len() <= MAX_NAME_BYTES / 2 {
            let room = MAX_NAME_BYTES.saturating_sub(suffix.len());
            while out.len() > room {
                out.pop();
            }
            while out.ends_with('-') || out.ends_with('.') {
                out.pop();
            }
            if !out.is_empty() {
                out.push_str(&suffix);
            }
        }
    }

    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn lowercases_and_dashes_the_shapes_the_seeds_actually_carry() {
        assert_eq!(kebab_name("Agents"), "agents");
        assert_eq!(kebab_name("Playbooks"), "playbooks");
        assert_eq!(kebab_name("Close checklist.md"), "close-checklist.md");
        assert_eq!(kebab_name("Q2 close.md"), "q2-close.md");
        assert_eq!(kebab_name("LiveOps calendar.md"), "liveops-calendar.md");
        assert_eq!(kebab_name("README.md"), "readme.md");
        assert_eq!(kebab_name("Page.tsx"), "page.tsx");
        assert_eq!(kebab_name("Page.compiled.mjs"), "page.compiled.mjs");
        assert_eq!(kebab_name("page_builder"), "page-builder");
    }

    #[test]
    fn collapses_separator_runs_and_trims_the_ends() {
        assert_eq!(kebab_name("  Spring   launch  "), "spring-launch");
        assert_eq!(kebab_name("a---b"), "a-b");
        assert_eq!(kebab_name("Q3 report -.md"), "q3-report.md");
        assert_eq!(kebab_name("trailing-"), "trailing");
    }

    #[test]
    fn never_produces_a_segment_that_is_not_addressable() {
        // A path separator is a separator, not a name.
        assert_eq!(kebab_name("a/b"), "a-b");
        // No hidden files, no `.`/`..`, no empty segment.
        assert_eq!(kebab_name(".hidden"), "hidden");
        assert_eq!(kebab_name("."), FALLBACK_NAME);
        assert_eq!(kebab_name(".."), FALLBACK_NAME);
        assert_eq!(kebab_name(""), FALLBACK_NAME);
        assert_eq!(kebab_name("🎉"), FALLBACK_NAME);
    }

    #[test]
    fn the_fallback_is_itself_normalized() {
        assert_eq!(kebab_name_or("🎉", "Task 42"), "task-42");
        // A caller cannot smuggle a name past the rule through the fallback.
        assert_eq!(kebab_name_or("", "Not A Slug"), "not-a-slug");
        // And a fallback that is itself unnameable still yields a name.
        assert_eq!(kebab_name_or("", "🎉"), FALLBACK_NAME);
    }

    #[test]
    fn is_kebab_name_is_the_fixed_point_of_kebab_name() {
        for raw in [
            "Agents",
            "Close checklist.md",
            "page.toml",
            "readme.md",
            "a-b-c",
            "🎉",
            "",
            "UPPER",
            "under_score",
        ] {
            let once = kebab_name(raw);
            assert_eq!(kebab_name(&once), once, "{raw}: not idempotent");
            assert!(is_kebab_name(&once), "{raw}: normalized form rejected");
        }
        assert!(!is_kebab_name("Agents"));
        assert!(!is_kebab_name("Close checklist.md"));
        assert!(is_kebab_name("close-checklist.md"));
    }

    #[test]
    fn bounds_the_length_without_leaving_a_dangling_separator() {
        let long = "Very Long Title ".repeat(40);
        let name = kebab_name(&long);
        assert!(name.len() <= MAX_NAME_BYTES, "{} bytes", name.len());
        assert!(!name.ends_with('-'));
        assert!(is_kebab_name(&name));
    }

    /// A name long enough to hit the cap keeps the extension that identifies
    /// its format: the stored name keys both mime inference and
    /// `ingest::extract` dispatch, so a `.docx` shed at the cap would make the
    /// upload unreadable at recall time (codex review finding on #1682).
    #[test]
    fn a_long_name_keeps_the_extension_that_identifies_its_format() {
        let raw = format!(
            "Quarterly Financial Review & Board Deck {} .docx",
            "FINAL ".repeat(20)
        );
        let name = kebab_name(&raw);
        assert!(name.len() <= MAX_NAME_BYTES, "{} bytes", name.len());
        assert!(name.ends_with(".docx"), "{name}");
        assert!(is_kebab_name(&name), "{name}");
    }

    /// The extension reserve only engages on truncation — a name that fits is
    /// byte-for-byte the same as before the reserve existed.
    #[test]
    fn the_extension_reserve_does_not_rewrite_names_that_fit() {
        assert_eq!(kebab_name("README.md"), "readme.md");
        assert_eq!(kebab_name("Page.compiled.mjs"), "page.compiled.mjs");
        assert_eq!(kebab_name("Close checklist.md"), "close-checklist.md");
    }

    #[test]
    fn paths_normalize_segment_by_segment() {
        assert_eq!(kebab_path("specs/Launch Plan.md"), "specs/launch-plan.md");
        assert_eq!(kebab_path("/Docs//A B/"), "docs/a-b");
    }
}

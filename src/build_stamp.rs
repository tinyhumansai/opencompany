// How the build commit stamp is chosen.
//
// This file is compiled twice on purpose. `build.rs` pulls it in with
// `include!` and supplies the impure inputs — environment, `git` — while
// `src/lib.rs` compiles it as a module under `cfg(test)` so `cargo test`
// executes the same code. Two implementations, one built and one tested,
// would be worse than none: the fallbacks below are the entire risk of
// stamping a commit at build time, and a test that exercises a second copy of
// them proves nothing about the binary that ships.
//
// Regular `//` comments rather than `//!`: an inner doc comment is not
// permitted where `build.rs` performs the `include!`.
//
// Nothing here may reach for `crate::`, `std::process` or the environment.
// Keeping it pure is what makes "there is no git" a case a unit test can
// state, instead of one that needs a container without git in it.

/// The stamp used when no source can name a commit at all.
const UNKNOWN_COMMIT: &str = "unknown";

/// The length a full object id is shortened to. Twelve hex digits is what
/// `git` itself considers unambiguous well past this repository's size, and
/// normalizing to it means an injected `GITHUB_SHA` reads the same as a
/// locally-derived one instead of being forty characters wide in analytics.
const SHORT_COMMIT_LEN: usize = 12;

/// The longest stamp accepted from any source.
///
/// The value is interpolated into a `cargo:rustc-env` line and then served on
/// an HTTP surface, so it is bounded and filtered rather than trusted: a
/// newline in it would forge a second build-script directive.
const MAX_COMMIT_LEN: usize = 64;

/// Normalizes a candidate commit string, or `None` when it names nothing.
///
/// Everything outside `[A-Za-z0-9._-]` is dropped rather than rejected, so a
/// value with a stray quote or trailing newline still yields a usable stamp
/// instead of silently degrading the build to `"unknown"`.
fn sanitize_commit(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(MAX_COMMIT_LEN)
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    // ASCII throughout by construction, so slicing on a byte index is safe.
    if cleaned.len() > SHORT_COMMIT_LEN && cleaned.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(cleaned[..SHORT_COMMIT_LEN].to_string());
    }
    Some(cleaned)
}

/// Chooses the commit stamp from whichever sources can answer.
///
/// The order is a claim about which source knows the most, and each step is
/// reached only because the one before it could not answer:
///
/// 1. `OPENCOMPANY_BUILD_COMMIT` — someone deliberately said what this build
///    is. A builder that bothers to set it knows more than this script can
///    work out, and it is the only escape hatch for a build environment
///    nothing below covers.
/// 2. `git` — ground truth about the tree actually being compiled, and the
///    only source that can also say whether it was clean. Preferred over
///    `GITHUB_SHA` because `GITHUB_SHA` describes what CI *intended* to check
///    out; a workflow that checks out a different ref would otherwise stamp a
///    commit that was never built.
/// 3. `GITHUB_SHA` — what CI believes, used exactly when there is no usable
///    repository to ask: a source tarball, a vendored crate, or a container
///    build whose context omits `.git`. These are the cases that make an
///    environment source necessary rather than merely convenient.
/// 4. `"unknown"` — an honest absence. A missing stamp must never fail a
///    build; a build that cannot say which commit it is remains a build.
///
/// `git_dirty` is consulted only when the stamp came from `git`, because it is
/// the only branch where the answer describes the same thing the stamp names.
/// An injected value is the injector's to be right about, and appending a
/// locally-measured suffix to it would mix two claims into one string.
fn resolve_build_commit(
    explicit: Option<String>,
    github_sha: Option<String>,
    git_head: impl FnOnce() -> Option<String>,
    git_dirty: impl FnOnce() -> bool,
) -> String {
    if let Some(commit) = explicit.as_deref().and_then(sanitize_commit) {
        return commit;
    }
    if let Some(commit) = git_head().as_deref().and_then(sanitize_commit) {
        return if git_dirty() {
            format!("{commit}-dirty")
        } else {
            commit
        };
    }
    if let Some(commit) = github_sha.as_deref().and_then(sanitize_commit) {
        return commit;
    }
    UNKNOWN_COMMIT.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full object id, as `git rev-parse HEAD` or `GITHUB_SHA` would give it.
    const FULL_SHA: &str = "d31e532f7c8a4b19e0f6c25a1d8b7e3049fa62c1";

    fn no_git() -> Option<String> {
        None
    }

    fn head(sha: &str) -> impl FnOnce() -> Option<String> {
        let sha = sha.to_string();
        move || Some(sha)
    }

    fn clean() -> bool {
        false
    }

    fn dirty() -> bool {
        true
    }

    #[test]
    fn an_injected_commit_outranks_git() {
        // The escape hatch has to actually win, or a builder that knows better
        // than this script has no way to say so.
        let stamp = resolve_build_commit(
            Some("abc123abc123".into()),
            Some(FULL_SHA.into()),
            head("999999999999"),
            dirty,
        );
        assert_eq!(stamp, "abc123abc123");
    }

    #[test]
    fn git_outranks_github_sha() {
        // `GITHUB_SHA` describes the ref CI meant to check out. When a
        // repository is present it is git, not CI, that knows what was built.
        let stamp = resolve_build_commit(None, Some(FULL_SHA.into()), head("999999999999"), clean);
        assert_eq!(stamp, "999999999999");
    }

    #[test]
    fn a_dirty_tree_says_so() {
        let stamp = resolve_build_commit(None, None, head("999999999999"), dirty);
        assert_eq!(stamp, "999999999999-dirty");
    }

    #[test]
    fn an_injected_commit_is_never_marked_dirty() {
        // The suffix is a claim about the tree the stamp names. An injected
        // value names someone else's tree, so measuring this one would be a
        // statement about the wrong thing.
        let stamp = resolve_build_commit(Some("abc123abc123".into()), None, head(FULL_SHA), dirty);
        assert_eq!(stamp, "abc123abc123");
    }

    /// The tarball / vendored-crate / no-`.git`-in-the-Docker-context case.
    #[test]
    fn github_sha_answers_when_there_is_no_git() {
        let stamp = resolve_build_commit(None, Some(FULL_SHA.into()), no_git, clean);
        assert_eq!(stamp, "d31e532f7c8a");
    }

    /// The whole-point case: no git binary, no `.git`, no CI. The build still
    /// has to produce a stamp rather than fail.
    #[test]
    fn no_source_at_all_degrades_to_unknown() {
        let stamp = resolve_build_commit(None, None, no_git, clean);
        assert_eq!(stamp, "unknown");
        assert!(!stamp.is_empty(), "an empty stamp would break `env!`");
    }

    #[test]
    fn a_source_that_is_present_but_empty_is_treated_as_absent() {
        // An exported-but-blank `GITHUB_SHA` and a `git` that exits zero with
        // nothing on stdout both reach here. Neither may win over a source
        // that actually knows something.
        let stamp = resolve_build_commit(
            Some("   ".into()),
            Some(FULL_SHA.into()),
            || Some("\n".into()),
            clean,
        );
        assert_eq!(stamp, "d31e532f7c8a");
    }

    #[test]
    fn a_full_object_id_is_shortened_to_twelve() {
        assert_eq!(sanitize_commit(FULL_SHA).as_deref(), Some("d31e532f7c8a"));
    }

    #[test]
    fn a_short_id_is_left_alone() {
        assert_eq!(sanitize_commit("d31e532f").as_deref(), Some("d31e532f"));
    }

    #[test]
    fn a_non_hex_tag_is_not_shortened() {
        // Only an object id gets truncated. A branch or tag name someone
        // injected stays legible.
        assert_eq!(
            sanitize_commit("release-2026-08-25").as_deref(),
            Some("release-2026-08-25")
        );
    }

    #[test]
    fn a_newline_cannot_forge_a_build_script_directive() {
        // The stamp is interpolated into `cargo:rustc-env=…`. A value carrying
        // a newline would otherwise inject a second directive into cargo's
        // stdout protocol.
        let stamp = sanitize_commit("abc123\ncargo:rustc-link-lib=evil").expect("a stamp");
        assert!(!stamp.contains('\n'), "{stamp:?} still spans lines");
        assert!(!stamp.contains(':'), "{stamp:?} kept a directive separator");
    }

    #[test]
    fn a_stamp_is_bounded() {
        let stamp = sanitize_commit(&"z".repeat(500)).expect("a stamp");
        assert_eq!(stamp.len(), MAX_COMMIT_LEN);
    }

    #[test]
    fn nothing_at_all_sanitizes_to_nothing() {
        for raw in ["", "   ", "\n\t", "///", "→→→"] {
            assert_eq!(sanitize_commit(raw), None, "{raw:?} named a commit");
        }
    }

    /// The stamp this very binary was compiled with. Not a test of the
    /// resolver but of the wiring around it: an `env!` that never ran, or a
    /// `build.rs` that emitted a malformed line, shows up here and nowhere
    /// else.
    #[test]
    fn the_compiled_in_stamp_is_well_formed() {
        let stamp = crate::BUILD_COMMIT;
        assert!(!stamp.is_empty(), "the stamp must never be empty");
        assert!(
            stamp.len() <= MAX_COMMIT_LEN + "-dirty".len(),
            "{stamp:?} is longer than any source can produce"
        );
        assert!(
            stamp
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "{stamp:?} carries characters the sanitizer should have dropped"
        );
    }
}

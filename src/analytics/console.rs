//! The closed vocabulary of console views (issue #1739).
//!
//! The console tells the host which page an operator opened, and that string
//! arrives over HTTP from a client this crate does not control. It is therefore
//! folded onto a fixed list here before it can reach a payload — the same rule
//! every other textual property in this module follows, and for the same
//! reason: an unanticipated value is the one that leaks something.
//!
//! **The view, never the hash.** `#/chat/dm:ada-1f3k` and `#/tasks/<uuid>` name
//! a teammate and a task; `chat` and `tasks` name a page. Only the second half
//! is a fact about the product rather than about the company using it, so the
//! route's second segment is not accepted here at all — the caller sends the
//! view alone, and anything it sends that is not on this list becomes `other`.

/// Every routed console view, mirroring `frontend/src/lib/console-routes.ts`.
///
/// Kept in step by
/// [`a_console_view_matches_the_console_route_table`](test::a_console_view_matches_the_console_route_table),
/// which parses `ROUTABLE` out of that TypeScript rather than trusting this
/// copy, and fails in **both** directions. A view added to the console and
/// missed here would otherwise silently report as `other`, which reads as
/// "operators do not use that page" — a page nobody visits and a page nobody
/// listed are indistinguishable once the fold has happened.
const VIEWS: &[&str] = &[
    "overview",
    "company",
    "chat",
    "conversation",
    "inbox",
    "tasks",
    "ledgers",
    "team",
    "workspace",
    "brain",
    "approvals",
    "workflows",
    "observatory",
    "pages",
    "finances",
    "settings",
    "feedback",
    "setup",
    "not-found",
];

/// The stable slug for a console view, or `other` for anything unrecognised.
///
/// Returns a `&'static str` from [`VIEWS`] rather than the caller's string, so
/// the value that reaches a payload is a literal compiled into this repository
/// even though the input arrived over the network.
pub fn console_view_slug(raw: &str) -> &'static str {
    let key = raw.trim().to_ascii_lowercase();
    VIEWS
        .iter()
        .find(|view| **view == key)
        .copied()
        .unwrap_or(crate::analytics::types::OTHER)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn every_known_view_keeps_its_own_name() {
        for view in VIEWS {
            assert_eq!(console_view_slug(view), *view);
        }
    }

    /// The point of the fold: what arrives is not what is reported.
    #[test]
    fn anything_else_becomes_other() {
        for raw in [
            "",
            "Overview ",
            "OVERVIEW",
            // The shapes that carry ids, and the reason the route takes a view
            // rather than a hash.
            "chat/dm:ada-1f3k",
            "tasks/9a8b3a85-85db-4efd-878a-efdeee0b0417",
            "#/settings/brain",
            "a page nobody has written yet",
        ] {
            let slug = console_view_slug(raw);
            if raw.trim().eq_ignore_ascii_case("overview") {
                assert_eq!(slug, "overview", "case and padding fold: {raw:?}");
                continue;
            }
            assert_eq!(slug, crate::analytics::types::OTHER, "{raw:?}");
        }
    }

    /// The returned value is always a literal from this file, never the input.
    ///
    /// A classifier that echoed its argument would pass the two tests above for
    /// every known view and leak on every unknown one, which is exactly the
    /// direction this module refuses to fail in.
    #[test]
    fn the_slug_is_never_the_callers_string() {
        let needle = "NotARealViewNameThatWouldLeak";
        let slug = console_view_slug(needle);
        assert!(
            !slug.contains("NotARealView"),
            "the classifier echoed its input: {slug}"
        );
        // The self-check: the needle really is findable when it is not folded,
        // so the assertion above is refusing something findable.
        assert!(needle.contains("NotARealView"));
    }
    /// The console's own route table, as this crate reads it.
    ///
    /// Relative to `CARGO_MANIFEST_DIR`, which is the repository root: the
    /// crate is rooted there and the console lives beside it.
    const CONSOLE_ROUTES_TS: &str = "frontend/src/lib/console-routes.ts";

    /// Every routable console view, read out of the TypeScript that defines
    /// them.
    ///
    /// Parses `ROUTABLE` rather than the `View` union above it, because
    /// `ROUTABLE` is what the console's own `VIEWS` is derived from and the two
    /// cannot disagree: `Record<View, true>` is a compile error with a member
    /// missing, and `npm run typecheck` is the first step of every console CI
    /// lane. Reading the union instead would pin this crate to a declaration
    /// the router does not consult.
    ///
    /// Comments are stripped before anything is matched, so the header's own
    /// prose about `ROUTABLE` — and the per-entry doc comments inside it, which
    /// are most of the file — cannot be mistaken for entries.
    fn routable_views_from_typescript() -> std::collections::BTreeSet<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(CONSOLE_ROUTES_TS);
        let source = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "the console route table at {} is unreadable: {err}\n\
                 This test is the only thing keeping `VIEWS` in `src/analytics/console.rs` in \
                 step with the console. If the table moved, point `CONSOLE_ROUTES_TS` at its \
                 new address — do NOT delete the test, because a stale `VIEWS` reports real \
                 pages as `other` and nothing else would notice.",
                path.display()
            )
        });

        let stripped = strip_comments(&source);
        let body = brace_body(&stripped, "const ROUTABLE").unwrap_or_else(|| {
            panic!(
                "no `const ROUTABLE` object found in {CONSOLE_ROUTES_TS}. \
                 The console's route table was renamed or restructured; this test must be \
                 taught the new shape rather than dropped."
            )
        });

        body.split(',')
            .filter_map(|entry| {
                let (key, value) = entry.split_once(':')?;
                let key = key.trim().trim_matches('"').trim_matches('\'').trim();
                // Every entry is `<view>: true`. Anything else is not a route.
                (!key.is_empty() && value.trim() == "true").then(|| key.to_string())
            })
            .collect()
    }

    /// Removes `/* … */` and whole-line `//` comments.
    ///
    /// Whole-line only for `//`, so a string such as `"#/pages"` inside an entry
    /// survives; the block form is what the doc comments in this file use.
    fn strip_comments(source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        let mut rest = source;
        while let Some(start) = rest.find("/*") {
            out.push_str(&rest[..start]);
            match rest[start + 2..].find("*/") {
                Some(end) => rest = &rest[start + 2 + end + 2..],
                None => {
                    rest = "";
                    break;
                }
            }
        }
        out.push_str(rest);
        out.lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The text between the braces of the first `{ … }` following `marker`.
    fn brace_body(source: &str, marker: &str) -> Option<String> {
        let at = source.find(marker)?;
        let open = at + source[at..].find('{')?;
        let mut depth = 0usize;
        for (offset, ch) in source[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(source[open + 1..open + offset].to_string());
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The list in this file is the console's list — in **both** directions.
    ///
    /// One direction alone is worth little. A view present in the console and
    /// absent here folds to `other`, so the page reads as unvisited; a view
    /// present here and absent from the console is a slug that can never be
    /// reported, which is how a vocabulary quietly accumulates dead words that
    /// a reviewer then trusts as the live set. Set equality catches both, and
    /// names the offending views in the failure rather than only their count.
    #[test]
    fn a_console_view_matches_the_console_route_table() {
        let console = routable_views_from_typescript();
        let rust: std::collections::BTreeSet<String> =
            VIEWS.iter().map(|view| (*view).to_string()).collect();

        // The parser found something. Without this a regex that stopped
        // matching would make the whole test pass by comparing two empty sets
        // — except that the difference below would then report every Rust view
        // as missing, so this is belt and braces on a legible failure.
        assert!(
            console.len() >= 10,
            "only {} view(s) parsed out of {CONSOLE_ROUTES_TS} — the parser, not the table, \
             is what changed",
            console.len()
        );
        assert_eq!(
            rust.len(),
            VIEWS.len(),
            "`VIEWS` contains a duplicate, which would hide a missing view here"
        );

        let missing_from_rust: Vec<&String> = console.difference(&rust).collect();
        assert!(
            missing_from_rust.is_empty(),
            "the console routes {missing_from_rust:?}, which `VIEWS` in \
             src/analytics/console.rs does not list — every visit to those pages reports as \
             `other`, which reads as nobody using them. Add them to `VIEWS`."
        );

        let missing_from_console: Vec<&String> = rust.difference(&console).collect();
        assert!(
            missing_from_console.is_empty(),
            "`VIEWS` lists {missing_from_console:?}, which the console no longer routes — a \
             slug that can never be reported. Remove them from `VIEWS`."
        );

        assert_eq!(console, rust, "the two lists must be the same set");

        // And the fold itself answers for every one of them, not merely the
        // list: `console_view_slug` is what the route actually calls.
        for view in &console {
            assert_eq!(
                console_view_slug(view),
                view.as_str(),
                "the console routes `{view}` but the fold does not keep its name"
            );
        }
    }
}

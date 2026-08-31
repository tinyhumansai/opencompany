//! Shared `composio_execute` call fixtures for the approval tests (issue #470).
//!
//! # Why this module exists
//!
//! `composio_execute` carries every Composio action under one tool name, so the
//! approval verdict is a property of the **action slug in its arguments**. The
//! tool's own schema declares that slug under `"tool"`
//! (`required: ["tool"]`, `additionalProperties: false`, see
//! [`crate::harness::composio`]), and
//! [`consequence_of`](crate::policy::consequence::consequence_of) reads it back
//! under [`COMPOSIO_ACTION_KEY`].
//!
//! Fixtures in five modules used to hard-code a key of their own — `tool_slug`
//! — which neither side accepts. Every one of them was therefore a call with
//! **no recognisable action**: the classifier could not find a slug, fell
//! through to the cautious "unknown is a send" verdict, and the test passed
//! without ever reaching the catalogue lookup it was credited with exercising.
//! The assertions were right; the inputs never arrived. A regression in the
//! read/send split would not have failed one of them.
//!
//! # What keeps it from drifting again
//!
//! [`composio_args`] builds the key from [`COMPOSIO_ACTION_KEY`] itself rather
//! than from a literal, so a fixture cannot disagree with the classifier: the
//! two now read the same constant. Renaming the wire key moves both together.
//!
//! # Choosing a slug
//!
//! The slugs below are real entries in the vendored provider catalogue, so they
//! exercise the lookup rather than the fallback. Do not invent a plausible-
//! looking slug for a fixture that is meant to classify — an uncatalogued slug
//! reads as a send, which is often the verdict the test expected anyway, and
//! that is exactly how #470 stayed invisible. When a test genuinely wants an
//! action nobody has classified, reach for
//! [`COMPOSIO_UNCLASSIFIED_SLUG`] and say so in its name.
//!
//! # A note on the two lanes
//!
//! The catalogue is vendored with openhuman and linked in only under that
//! feature. Without it [`composio_action_is_read`] answers `false` for
//! everything, so [`composio_read_args`] classifies as a **send** in the
//! default build. That is deliberate and pinned below in both directions — a
//! test that asserts the read verdict must be `#[cfg(feature = "openhuman")]`.
//!
//! [`composio_action_is_read`]: crate::policy::consequence
//! [`COMPOSIO_ACTION_KEY`]: crate::policy::consequence::COMPOSIO_ACTION_KEY

use serde_json::Value;

use crate::policy::consequence::COMPOSIO_ACTION_KEY;

/// A Composio action the vendored catalogue tags `Write`: sending mail.
///
/// Classifies as [`EffectGroup::Send`](crate::ports::types::EffectGroup::Send)
/// with [`Standing::PerCall`](crate::policy::Standing::PerCall) — and does so
/// through the catalogue, not through the unknown-slug fallback.
pub(crate) const COMPOSIO_SEND_SLUG: &str = "GMAIL_SEND_EMAIL";

/// A second catalogued `Write`, on a different toolkit.
///
/// For tests that need two calls the queue must treat as distinct without
/// either of them being unclassifiable.
pub(crate) const COMPOSIO_OTHER_SEND_SLUG: &str = "SLACK_POST_MESSAGE_TO_CHANNEL";

/// A Composio action the vendored catalogue tags `Read`: listing a repository's
/// pull requests.
///
/// Classifies as [`EffectGroup::Other`](crate::ports::types::EffectGroup::Other)
/// with [`Standing::Grantable`](crate::policy::Standing::Grantable) — **under
/// the `openhuman` feature only**. See the lane note in the module docs.
pub(crate) const COMPOSIO_READ_SLUG: &str = "GITHUB_LIST_PULL_REQUESTS";

/// An action slug whose toolkit the catalogue has never heard of.
///
/// The cautious fallback deserves its own coverage, so this exists to be
/// requested deliberately and named as such at the call site — never as the
/// accidental result of a typo'd key.
///
/// Its verb matters since issue #1818: `DO` is in neither the read list nor
/// the mutating one, so the fallback finds no evidence of a read and this stays
/// a send. A fixture spelled `..._LIST_...` would now classify as an inferred
/// read and would quietly stop covering what it is named for.
pub(crate) const COMPOSIO_UNCLASSIFIED_SLUG: &str = "NOTAREALTOOLKIT_DO_SOMETHING";

/// A `composio_execute` argument object naming `slug`, keyed the way the tool
/// and the classifier both key it.
pub(crate) fn composio_args(slug: &str) -> Value {
    let mut args = serde_json::Map::new();
    args.insert(COMPOSIO_ACTION_KEY.to_string(), Value::String(slug.into()));
    Value::Object(args)
}

/// [`composio_args`], plus the action's own parameters under `"arguments"` —
/// the only other property the tool's schema admits.
pub(crate) fn composio_args_with(slug: &str, arguments: Value) -> Value {
    let mut args = composio_args(slug);
    args["arguments"] = arguments;
    args
}

/// A call the catalogue classifies as a send.
pub(crate) fn composio_send_args() -> Value {
    composio_args(COMPOSIO_SEND_SLUG)
}

/// A call the catalogue classifies as a read — under the `openhuman` feature.
pub(crate) fn composio_read_args() -> Value {
    composio_args(COMPOSIO_READ_SLUG)
}

/// A call naming an action the catalogue cannot place, so it reads as a send
/// through the cautious fallback rather than through the lookup.
pub(crate) fn composio_unclassified_args() -> Value {
    composio_args(COMPOSIO_UNCLASSIFIED_SLUG)
}

/// The `i`th of a run of distinct, deliberately uncatalogued calls.
///
/// For the over-cap tests, whose point is the *number* of parked requests: they
/// need many calls the queue treats as distinct and care nothing for what any
/// of them classify as. The slug still lands under the real key, so these
/// reach the catalogue lookup and miss it — an honest fallback rather than a
/// call with no action at all.
pub(crate) fn composio_unclassified_args_numbered(i: usize) -> Value {
    composio_args(&format!("NOTAREALTOOLKIT_ACTION_{i}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Standing;
    #[cfg(feature = "openhuman")]
    use crate::policy::consequence::Reach;
    use crate::policy::consequence::{COMPOSIO_EXECUTE, consequence_of};
    use crate::ports::types::EffectGroup;

    /// The guarantee the whole module exists for: a fixture reaches the
    /// classifier's action key. Asserted against the constant *and* against the
    /// literal, because agreeing with `COMPOSIO_ACTION_KEY` while both drift off
    /// the tool's declared schema would reproduce #470 with the helper in place.
    #[test]
    fn a_fixture_names_its_action_under_the_key_the_tool_declares() {
        let args = composio_send_args();
        assert_eq!(
            args.get(COMPOSIO_ACTION_KEY).and_then(|v| v.as_str()),
            Some(COMPOSIO_SEND_SLUG)
        );
        assert_eq!(
            args.get("tool").and_then(|v| v.as_str()),
            Some(COMPOSIO_SEND_SLUG),
            "`tool` is what `composio_execute`'s schema requires; if this fails \
             the wire key moved and the tool's schema has to move with it"
        );
        assert!(
            args.get("tool_slug").is_none(),
            "the key #470 was about must not come back"
        );
    }

    /// The action's own parameters ride under `arguments`, which is the only
    /// other property the tool's schema admits — a fixture that spread them at
    /// the top level would be rejected by `additionalProperties: false`.
    #[test]
    fn action_parameters_ride_under_arguments() {
        let args = composio_args_with(COMPOSIO_SEND_SLUG, serde_json::json!({ "to": "a@b.test" }));
        assert_eq!(
            args.get("tool").and_then(|v| v.as_str()),
            Some(COMPOSIO_SEND_SLUG)
        );
        assert_eq!(args["arguments"]["to"], "a@b.test");
        assert_eq!(
            args.as_object().map(|o| o.len()),
            Some(2),
            "`tool` and `arguments` are the whole of the declared schema"
        );
    }

    /// The send fixture is a send *through the catalogue*. True in both lanes,
    /// but for different reasons — which is why the read fixture below is
    /// pinned separately per lane.
    #[test]
    fn the_send_fixture_classifies_as_a_send() {
        for args in [
            composio_send_args(),
            composio_args(COMPOSIO_OTHER_SEND_SLUG),
        ] {
            let verdict = consequence_of(COMPOSIO_EXECUTE, &args);
            assert_eq!(verdict.group, EffectGroup::Send, "{args}");
            assert_eq!(verdict.standing, Standing::PerCall, "{args}");
        }
    }

    /// The fixture the fallback tests want: an action nobody has classified,
    /// asked for on purpose. Same verdict in both lanes.
    #[test]
    fn the_unclassified_fixture_falls_back_to_a_send() {
        for args in [
            composio_unclassified_args(),
            composio_unclassified_args_numbered(0),
            composio_unclassified_args_numbered(7),
        ] {
            let verdict = consequence_of(COMPOSIO_EXECUTE, &args);
            assert_eq!(verdict.group, EffectGroup::Send, "{args}");
            assert_eq!(verdict.standing, Standing::PerCall, "{args}");
        }
    }

    /// The one that #470 was really about: with the catalogue linked in, the
    /// read fixture is classified as a read — by the lookup, not by luck.
    ///
    /// This is the assertion the old `tool_slug` fixtures could not have made.
    /// Break the catalogue lookup and this fails, which is the property the
    /// negative control in #559 is built on.
    #[test]
    #[cfg(feature = "openhuman")]
    fn the_read_fixture_classifies_as_a_read() {
        let verdict = consequence_of(COMPOSIO_EXECUTE, &composio_read_args());
        assert_eq!(verdict.group, EffectGroup::Other);
        assert_eq!(verdict.standing, Standing::Grantable);
        // Issue #559 moved the read off `Consequence`, which is what parks. The
        // two assertions above are the ones that did not move.
        assert_eq!(verdict.reach, Reach::ExternalRead);
    }

    /// And the other lane, stated rather than left to be discovered: without
    /// the harness feature the catalogue is not linked in, so the read fixture
    /// classifies as a send. A test that wants the read verdict must be gated.
    #[test]
    #[cfg(not(feature = "openhuman"))]
    fn without_the_catalogue_the_read_fixture_is_a_send() {
        let verdict = consequence_of(COMPOSIO_EXECUTE, &composio_read_args());
        assert_eq!(verdict.group, EffectGroup::Send);
        assert_eq!(verdict.standing, Standing::PerCall);
    }
}

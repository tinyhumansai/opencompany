//! [`ModelSlug`] — the closed vocabulary a metered sample names its model in.
//!
//! # Why this is not a `String`
//!
//! The model that reaches an inference endpoint is **operator-authored free
//! text**. A company on `openai_compatible` or `ollama` points at a server it
//! runs and calls the model whatever it likes, and `[inference].models` maps a
//! workload tier onto that name verbatim — [`model_for_tier`] deliberately
//! honours an operator's entry without rewriting it. So the raw name can be a
//! customer's name, an internal project code, or anything else a person typed.
//!
//! Two consequences follow, and they are the whole reason this type exists
//! rather than a `model: String` field on [`UsageSample`]:
//!
//! * **Telemetry.** The product-analytics design (#1739, landing in #1751 as
//!   `docs/spec/runtime/analytics.md`) requires every textual property to be a
//!   `&'static str` written in this repository, precisely so a runtime string
//!   cannot become a payload. A `String` on the sample would be one
//!   `sample.model.clone()` away from an outbound body, and the metered event
//!   that PR adds reads exactly this sample.
//! * **Cardinality.** Samples are retained for
//!   [`RETENTION_DAYS`](crate::ports::usage::RETENTION_DAYS) — 90 days on every
//!   backend. A free-text column on every sample is unbounded-cardinality data
//!   an aggregation cannot group by and a store cannot index.
//!
//! This mirrors what `provider` already does
//! ([`provider_slug`](crate::company::inference::provider_slug), documented as
//! "the stable telemetry slug"): fold onto a fixed list, and send anything
//! unrecognised to a fallback. The dangerous direction is a value nobody
//! anticipated, so that is the direction it fails in.
//!
//! # The raw model name is deliberately **not** stored
//!
//! A design where the sample keeps the operator's own model name alongside the
//! slug was considered and rejected. It is defensible — it is the operator's
//! own data on the operator's own instance, and a Usage view showing them the
//! name they typed is a nicer view. It was rejected because the guarantee would
//! then be a *rule call sites obey* ("never put `raw_model` on an outbound
//! payload") rather than a *type that cannot hold runtime text*, and it would
//! have to be re-obeyed at every seam that leaves the box: the analytics
//! envelope, a hosted support bundle, an error report, a future export. One of
//! them eventually forgets.
//!
//! The operator's own model name is not lost by this: it is in
//! `[inference].models` and on the console's Inference card, which is where a
//! *current* configuration belongs. Copying it onto 90 days of accounting rows
//! is a different thing, and a worse one.
//!
//! # The vocabulary, and how it is extended
//!
//! A slug is `<vendor>` or `<vendor>-<line>`, plus this repo's own workload
//! tiers, plus [`ModelSlug::OTHER`]. A line is broken out only where the vendor
//! prices that line separately — which is exactly when an operator asks "what
//! is Sonnet costing us versus Haiku?" and a vendor-level answer cannot reply.
//!
//! **Add a slug when one of these becomes true, and not otherwise:**
//!
//! 1. this repo starts shipping the model as a default — a new entry in
//!    [`DEFAULT_TIER_MODELS`] or a new tier in `harness::build::model_for_tier`
//!    (the `openhuman`-gated tier mapper). The test
//!    `every_shipped_default_is_named` fails when this is forgotten, which is
//!    what stops the list rotting silently as the defaults move;
//! 2. a vendor is common enough among BYOK tenants that `other` has stopped
//!    being a useful answer for them. That is a judgement call, and it is meant
//!    to be: the alternative — adding a slug for every model that exists — is
//!    not a closed vocabulary, it is a copy of the world's model catalogue that
//!    goes stale weekly.
//!
//! A model outside the list reports [`ModelSlug::OTHER`]. That is the honest
//! answer and it is a *bounded* one; growing the list to avoid ever seeing it
//! would defeat the point of the type.
//!
//! [`model_for_tier`]: crate::company::inference::model_for_tier
//! [`DEFAULT_TIER_MODELS`]: crate::company::inference::DEFAULT_TIER_MODELS
//! [`UsageSample`]: crate::ports::usage::UsageSample

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A model identity folded onto the closed vocabulary described in the
/// [module docs](self).
///
/// The inner value is a private `&'static str`, so a `ModelSlug` **cannot** be
/// constructed from runtime data except through [`ModelSlug::classify`], which
/// only ever returns a compiled-in literal. That is what makes "the raw model
/// name never leaves the harness" a property of the type rather than a
/// convention a call site has to keep remembering — and it is the same
/// construction argument the analytics payload vocabulary in #1751 makes, so
/// [`as_str`](ModelSlug::as_str) drops straight into a `&'static str` property
/// with no second classifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModelSlug(&'static str);

impl ModelSlug {
    /// A model ran, but it is not one this build can name — a BYOK or
    /// self-hosted model, or a vendor the vocabulary has not been extended to.
    ///
    /// Deliberately not "unknown": nothing is unknown here. The endpoint was
    /// asked for a specific model and answered; what is missing is a *name for
    /// it that is safe to keep*, which is a different fact.
    pub const OTHER: Self = Self("other");

    /// The literal recorded on a sample and read by an aggregation.
    pub fn as_str(&self) -> &'static str {
        self.0
    }

    /// Folds a raw model string onto the vocabulary.
    ///
    /// Matching is case-insensitive and ignores an OpenRouter-style
    /// `author/` prefix, so `deepseek/deepseek-v4-pro`, `DeepSeek-V4-Pro` and
    /// `deepseek-v4-pro` all land on the same slug.
    ///
    /// A marker match is a **substring** test within the vendor's line, not an
    /// exact one, because vendors version their ids (`claude-sonnet-4-6`,
    /// `gpt-5.2-mini`) and pinning the exact id would send every point release
    /// to `OTHER`. The cost is that an operator who names a self-hosted model
    /// `our-sonnet-clone` is classified as `anthropic-sonnet`: a
    /// misclassification, not a leak, and the leak is the thing this type is
    /// defending against.
    pub fn classify(raw: &str) -> Self {
        let lower = raw.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return Self::OTHER;
        }

        // This repo's own workload tiers. On the subscription-proxied path the
        // tier *is* what goes on the wire — the platform's registry resolves it
        // upstream — so the tier is the most specific true answer available
        // here, and substituting the model it resolves to by default would be
        // a guess printed as a fact.
        //
        // Matched on the whole string, before the `author/` split, because a
        // tier has no author. The sentinels round-trip to themselves so that
        // `Deserialize` (which re-classifies) is idempotent.
        for slug in EXACT {
            if lower == *slug {
                return Self(slug);
            }
        }

        // OpenRouter and most catalogues namespace as `author/slug`. Keep both
        // halves: the author is the strongest vendor signal, and the slug is
        // where the line marker lives.
        let (author, line) = match lower.split_once('/') {
            Some((author, line)) => (author, line),
            None => ("", lower.as_str()),
        };

        for vendor in VENDORS {
            let matched = vendor
                .authors
                .iter()
                .any(|candidate| author == *candidate || line.starts_with(candidate));
            if !matched {
                continue;
            }
            for (marker, slug) in vendor.lines {
                if line.contains(marker) {
                    return Self(slug);
                }
            }
            return Self(vendor.slug);
        }

        Self::OTHER
    }
}

/// Slugs matched on the whole model string, before any `author/` split.
///
/// The four workload tiers this repo defines (`model_for_tier`), plus
/// [`ModelSlug::OTHER`]'s own literal so that re-classifying an already-folded
/// value is a fixed point.
const EXACT: &[&str] = &[
    "chat-v1",
    "reasoning-v1",
    "agentic-v1",
    "vision-v1",
    "other",
];

/// One vendor's entry in the vocabulary.
struct Vendor {
    /// The slug reported when the vendor matches but no line marker does.
    slug: &'static str,
    /// Author namespaces and id prefixes that identify this vendor.
    authors: &'static [&'static str],
    /// `(marker, slug)` pairs, tried in order, matched as a substring of the
    /// model id. Present only where the vendor prices its lines separately.
    lines: &'static [(&'static str, &'static str)],
}

/// The vendor table. See the [module docs](self) for the rule that governs
/// what may be added to it.
const VENDORS: &[Vendor] = &[
    // DeepSeek's flash and pro lines are priced separately, so retain the line
    // split for operators who select either from the OpenRouter catalog.
    Vendor {
        slug: "deepseek",
        authors: &["deepseek"],
        lines: &[
            ("v4-flash", "deepseek-v4-flash"),
            ("v4-pro", "deepseek-v4-pro"),
        ],
    },
    // `DEFAULT_TIER_MODELS` binds `vision-v1` to `qwen/qwen3.8-max`. The dot in
    // the upstream id is not carried into the slug — a telemetry value is read
    // by people and grouped by machines, and a point release must not mint a
    // new one.
    Vendor {
        slug: "qwen",
        authors: &["qwen"],
        lines: &[
            ("3.8-max", "qwen3-max"),
            ("3-max", "qwen3-max"),
            ("3.7-plus", "qwen3-plus"),
            ("3-plus", "qwen3-plus"),
        ],
    },
    // The shipped chat and agentic defaults are Anthropic Sonnet and Opus.
    // Anthropic prices opus, sonnet and haiku separately and by an order of
    // magnitude; "what is Sonnet costing us versus Haiku?" is the question
    // issue #1749 is named after, and a vendor-level slug cannot answer it.
    Vendor {
        slug: "anthropic",
        authors: &["anthropic", "claude"],
        lines: &[
            ("opus", "anthropic-opus"),
            ("sonnet", "anthropic-sonnet"),
            ("haiku", "anthropic-haiku"),
        ],
    },
    // The shipped reasoning default is an OpenAI GPT model. OpenAI's reasoning
    // (`o`-series) and chat (`gpt`) families are priced on different rate cards,
    // which is the same split as above.
    Vendor {
        slug: "openai",
        authors: &["openai", "gpt", "o1", "o3", "o4"],
        lines: &[("gpt", "openai-gpt")],
    },
    // Google prices Gemini Pro and Gemini Flash separately.
    Vendor {
        slug: "google",
        authors: &["google", "gemini"],
        lines: &[
            ("pro", "google-gemini-pro"),
            ("flash", "google-gemini-flash"),
        ],
    },
    // Open-weight vendors a self-hosted `ollama` or `openai_compatible` tenant
    // most often runs. One slug each: nobody is billed per line for a model
    // they host themselves, so there is nothing for a line split to answer.
    Vendor {
        slug: "meta-llama",
        authors: &["meta-llama", "llama"],
        lines: &[],
    },
    Vendor {
        slug: "mistral",
        authors: &["mistralai", "mistral"],
        lines: &[],
    },
];

impl fmt::Display for ModelSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Serialize for ModelSlug {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for ModelSlug {
    /// Re-classifies on the way in.
    ///
    /// A stored row is not trusted to already hold a vocabulary member: a
    /// hand-edited document, a row written by an older or a forked build, or a
    /// shared-single-DB tenant's collection can all present arbitrary text
    /// here. Re-folding it means a raw model name **cannot** survive a
    /// round-trip through a store and reach a reader — the classifier is on
    /// both boundaries, not just the write one.
    ///
    /// Serialising a vocabulary member and reading it back is a fixed point:
    /// every slug the classifier can emit classifies to itself.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::classify(&raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::company::inference::DEFAULT_TIER_MODELS;

    /// Every literal the classifier is capable of emitting.
    fn every_slug() -> Vec<&'static str> {
        let mut slugs: Vec<&'static str> = EXACT.to_vec();
        for vendor in VENDORS {
            slugs.push(vendor.slug);
            slugs.extend(vendor.lines.iter().map(|(_, slug)| *slug));
        }
        slugs.sort_unstable();
        slugs.dedup();
        slugs
    }

    #[test]
    fn a_shipped_default_model_is_named_rather_than_other() {
        // The ratchet that stops the vocabulary rotting: the moment
        // `DEFAULT_TIER_MODELS` gains a model this table has never heard of,
        // every self-serve tenant's spend starts reporting `other` and this
        // test says so.
        for (tier, model) in DEFAULT_TIER_MODELS {
            assert_ne!(
                ModelSlug::classify(model),
                ModelSlug::OTHER,
                "the shipped default for `{tier}` (`{model}`) classifies to `other`; \
                 add it to the vendor table in src/metering/model.rs"
            );
            assert_ne!(
                ModelSlug::classify(tier),
                ModelSlug::OTHER,
                "the workload tier `{tier}` classifies to `other`; add it to EXACT"
            );
        }
    }

    #[test]
    fn classifying_a_slug_again_returns_the_same_slug() {
        // What makes `Deserialize` (which re-classifies) safe to apply to a
        // value this crate wrote.
        for slug in every_slug() {
            assert_eq!(
                ModelSlug::classify(slug).as_str(),
                slug,
                "`{slug}` is not a fixed point of the classifier"
            );
        }
    }

    #[test]
    fn an_operator_named_model_never_reaches_the_slug() {
        // The BYOK leak this type exists to stop: a self-hosted endpoint can
        // name a model after the customer it was built for.
        for raw in [
            "acme-corp-internal-v3",
            "northwind-legal-review",
            "ollama/my-finetune-2026-01",
            "hr-screening-model",
            "",
            "   ",
        ] {
            let slug = ModelSlug::classify(raw);
            assert_eq!(slug, ModelSlug::OTHER, "`{raw}` should classify to other");
            assert!(
                !raw.to_ascii_lowercase().contains(slug.as_str()),
                "`{raw}` leaked into the slug `{slug}`"
            );
        }
    }

    #[test]
    fn a_known_vendor_line_is_named_at_the_granularity_it_is_priced_at() {
        for (raw, expected) in [
            ("anthropic/claude-sonnet-4-6", "anthropic-sonnet"),
            ("Claude-3-Haiku", "anthropic-haiku"),
            ("anthropic/claude-opus-4-1", "anthropic-opus"),
            ("anthropic/claude-next", "anthropic"),
            ("openai/gpt-5.2", "openai-gpt"),
            ("o3-mini", "openai"),
            ("google/gemini-2.5-pro", "google-gemini-pro"),
            ("google/gemini-2.5-flash", "google-gemini-flash"),
            ("deepseek/deepseek-v4-flash", "deepseek-v4-flash"),
            ("deepseek/deepseek-v4-pro", "deepseek-v4-pro"),
            ("deepseek/deepseek-r1", "deepseek"),
            ("qwen/qwen3.8-max", "qwen3-max"),
            ("qwen/qwen3.7-plus", "qwen3-plus"),
            ("meta-llama/llama-4-70b", "meta-llama"),
            ("mistralai/mistral-large", "mistral"),
            ("chat-v1", "chat-v1"),
            ("AGENTIC-V1", "agentic-v1"),
        ] {
            assert_eq!(
                ModelSlug::classify(raw).as_str(),
                expected,
                "classifying `{raw}`"
            );
        }
    }

    #[test]
    fn a_slug_serializes_as_its_literal_and_re_folds_on_the_way_back() {
        let slug = ModelSlug::classify("anthropic/claude-sonnet-4-6");
        let json = serde_json::to_string(&slug).unwrap();
        assert_eq!(json, "\"anthropic-sonnet\"");
        assert_eq!(serde_json::from_str::<ModelSlug>(&json).unwrap(), slug);

        // A store row that somehow holds raw operator text — a hand-edited
        // document, a forked build, a foreign writer — is folded on read, so
        // the raw name cannot reach a reader through the store.
        let smuggled: ModelSlug = serde_json::from_str("\"acme-corp-internal-v3\"").unwrap();
        assert_eq!(smuggled, ModelSlug::OTHER);
    }
}

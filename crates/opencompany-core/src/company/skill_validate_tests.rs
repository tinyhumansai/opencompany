use super::*;

use std::path::{Path, PathBuf};

/// `companies/`, the repo's bundle root, from this crate's manifest dir.
fn companies_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../companies")
}

fn skill_md(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n# Body\n")
}

#[test]
fn a_slug_at_the_cap_passes_and_one_over_it_is_refused() {
    let at_cap = "a".repeat(MAX_SLUG_CHARS);
    assert!(
        validate_slug(&at_cap).is_ok(),
        "{MAX_SLUG_CHARS} is allowed"
    );

    let over = "a".repeat(MAX_SLUG_CHARS + 1);
    let problem = validate_slug(&over).expect_err("one over the cap is refused");
    assert!(
        problem.contains(&format!("{} characters", MAX_SLUG_CHARS + 1)),
        "{problem}"
    );
    assert!(problem.contains(&MAX_SLUG_CHARS.to_string()), "{problem}");
}

#[test]
fn the_slug_charset_rule_still_applies_and_names_itself() {
    for slug in ["A", "-leading", "under_score", "a/b", "..", ""] {
        let problem = validate_slug(slug).expect_err("{slug} is refused");
        assert!(
            problem.contains("not a valid skill slug"),
            "{slug:?}: {problem}"
        );
    }
}

#[test]
fn a_description_over_the_cap_is_refused_and_one_at_it_passes() {
    let at_cap = "d".repeat(MAX_DESCRIPTION_CHARS);
    validate_skill_md("demo", &skill_md("demo", &at_cap)).expect("the cap itself is allowed");

    let over = "d".repeat(MAX_DESCRIPTION_CHARS + 1);
    let problems = validate_skill_md("demo", &skill_md("demo", &over))
        .expect_err("one character over the cap is refused");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains(&format!("{} characters", MAX_DESCRIPTION_CHARS + 1)),
        "{problems:?}"
    );
}

/// The cap counts characters, not bytes — a description of multi-byte
/// characters within the limit must not be refused for its encoded length.
#[test]
fn the_description_cap_counts_characters_not_bytes() {
    let multibyte = "é".repeat(MAX_DESCRIPTION_CHARS);
    assert!(
        multibyte.len() > MAX_DESCRIPTION_CHARS,
        "sanity: 2 bytes each"
    );
    validate_skill_md("demo", &skill_md("demo", &multibyte))
        .expect("a description at the character cap passes whatever it encodes to");
}

#[test]
fn a_frontmatter_block_over_the_cap_is_refused() {
    let padding = "x".repeat(MAX_FRONTMATTER_BYTES);
    let src = format!("---\nname: demo\ndescription: short\npadding: {padding}\n---\n# Body\n");

    let problems = validate_skill_md("demo", &src).expect_err("an oversized block is refused");
    assert!(
        problems.iter().any(|p| p.contains("frontmatter block")),
        "{problems:?}"
    );
}

/// The cap bounds the block even when the body is small — unknown keys are
/// tolerated by the parser, so without it a document could carry any amount of
/// metadata the console would then render.
#[test]
fn a_frontmatter_block_under_the_cap_passes_with_unknown_keys() {
    let padding = "x".repeat(MAX_FRONTMATTER_BYTES / 2);
    let src = format!("---\nname: demo\ndescription: short\npadding: {padding}\n---\n# Body\n");
    validate_skill_md("demo", &src).expect("a block within the cap is accepted");
}

#[test]
fn a_name_that_differs_from_the_slug_warns_rather_than_refusing() {
    let valid = validate_skill_md(
        "web-research",
        &skill_md("Web Research", "Answers questions."),
    )
    .expect("a display name does not refuse the document");
    assert_eq!(
        valid.deltas,
        vec![SpecDelta::NameIsNotSlug {
            name: "Web Research".to_string(),
            slug: "web-research".to_string(),
        }]
    );
    let message = valid.deltas[0].message();
    assert!(message.contains("web-research"), "{message}");
    assert!(message.contains("Web Research"), "{message}");
}

#[test]
fn a_name_equal_to_the_slug_records_no_delta() {
    let valid = validate_skill_md("demo", &skill_md("demo", "Does a thing.")).expect("valid");
    assert!(valid.deltas.is_empty(), "{:?}", valid.deltas);
    assert_eq!(valid.doc.slug, "demo");
}

/// Every problem at once: the caller should not have to fix one and resubmit to
/// discover the next.
#[test]
fn every_problem_is_reported_in_one_pass() {
    let slug = "A".repeat(MAX_SLUG_CHARS + 1);
    let over = "d".repeat(MAX_DESCRIPTION_CHARS + 1);
    let problems =
        validate_skill_md(&slug, &skill_md("demo", &over)).expect_err("both rules are broken");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("not a valid skill slug")),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("description is")),
        "{problems:?}"
    );
}

#[test]
fn a_document_that_does_not_parse_is_refused_with_the_parser_s_own_problems() {
    let problems = validate_skill_md("demo", "---\ncategory: research\n---\nbody\n")
        .expect_err("missing required keys");
    assert!(
        problems.iter().any(|p| p.contains("`name`")),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("`description`")),
        "{problems:?}"
    );
}

/// The phase's risk gate, as a test rather than a manual check.
///
/// The caps are new rules applied to documents that already exist. If one of
/// the skills this repo ships fails them, the cap is wrong — a company running
/// today's build would find a baseline skill it can no longer reinstall.
#[test]
fn every_shipped_global_skill_still_parses_and_validates() {
    let globals = companies_dir().join("_globals/skills");
    let docs = crate::company::load_dir_skills(&globals).expect("the global baseline parses");
    assert!(!docs.is_empty(), "sanity: {} has skills", globals.display());

    for doc in &docs {
        let src = std::fs::read_to_string(globals.join(&doc.slug).join("SKILL.md")).expect("read");
        let valid = validate_skill_md(&doc.slug, &src).unwrap_or_else(|problems| {
            panic!("global `{}` fails validation: {problems:?}", doc.slug)
        });
        assert_eq!(valid.doc.slug, doc.slug);
    }
}

/// The same gate over every bundle in the repo, not only the baseline: the
/// registry `install` serves is the union of all of them, so a vertical's skill
/// the caps rejected would be un-installable on every host that ships it.
#[test]
fn every_shipped_bundle_skill_still_parses_and_validates() {
    let companies = companies_dir();
    let docs = crate::company::load_catalog_skills(&companies).expect("the bundles parse");
    assert!(docs.len() >= 14, "sanity: the catalog is populated");

    for doc in &docs {
        let src = crate::company::render_skill_md(doc);
        validate_skill_md(&doc.slug, &src)
            .unwrap_or_else(|problems| panic!("`{}` fails validation: {problems:?}", doc.slug));
    }
}

/// The console counts a typed description against this limit while the operator
/// types. A counter that disagrees with the host either stops them short of a
/// description the host would have taken, or lets them fill a field that is
/// refused on save with the count still reading green.
///
/// Read out of the console's own source rather than restated here, so the two
/// numbers cannot drift without this failing.
#[test]
fn the_console_counts_against_the_same_description_limit() {
    const CONSOLE_LIB: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../frontend/src/lib/skills.ts"
    ));
    let declared = format!("export const SKILL_DESCRIPTION_MAX_CHARS = {MAX_DESCRIPTION_CHARS};");
    assert!(
        CONSOLE_LIB.contains(&declared),
        "frontend/src/lib/skills.ts no longer declares `{declared}` — the console's live \
         character count has drifted from the limit this validator enforces. Change both \
         together."
    );
}

/// Every caller of `slugify`, not only `create_custom`, gets a slug off the
/// reserved list.
#[test]
fn slugify_steps_off_every_reserved_slug() {
    for name in ["Draft", "Upload", "Registry"] {
        let slug = slugify(name);
        assert!(
            !RESERVED_SLUGS.contains(&slug.as_str()),
            "slugify({name:?}) returned the reserved slug {slug:?}"
        );
    }
}

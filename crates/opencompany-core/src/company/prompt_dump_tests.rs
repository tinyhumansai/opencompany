use super::*;

fn manifest(body: &str) -> CompanyManifest {
    toml::from_str(body).expect("test manifest parses")
}

#[test]
fn the_persona_section_is_always_present_and_names_the_company() {
    let manifest = manifest(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
    );
    let dumped = dump(&manifest);
    assert_eq!(dumped.len(), 1);
    assert_eq!(dumped[0].sections[0].title, "Persona");
    assert!(
        dumped[0].sections[0]
            .body
            .contains("Product Manager at Acme"),
        "{}",
        dumped[0].sections[0].body
    );
}

/// The first agent is the orchestrator when nobody is tagged, and the dump
/// must report that rather than leaving it to be inferred from an absent
/// `tier` — the orchestrator brief is a real section of its prompt.
#[test]
fn the_untagged_first_agent_is_reported_as_the_orchestrator() {
    let manifest = manifest(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"first\"\nrole = \"First\"\n\n[[agent]]\nid = \"second\"\nrole = \"Second\"\n",
    );
    let dumped = dump(&manifest);
    assert!(dumped[0].orchestrator);
    assert!(!dumped[1].orchestrator);
}

/// The sandbox section is the one this surface most needs to get right: an
/// agent that is never told it holds `file_write` records a task about
/// writing instead of writing, and an operator reading a dump that omits
/// the section has no way to see why. A default belt (`[tools].allow`
/// defaults to `*`) must therefore produce it, naming all three clauses.
///
/// `harness_sections` — the only place that ever adds or defers a "Your
/// sandbox" section — is itself `#[cfg(feature = "openhuman")]`; a
/// default build folds it into the single "Tool briefs (...)" deferred
/// line instead (see the `#[cfg(not(feature = "openhuman"))]` branch
/// above). This test needs the same feature gate its subject does.
#[test]
#[cfg(feature = "openhuman")]
fn a_default_belt_is_told_about_its_sandbox_and_its_shell() {
    let manifest = manifest(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
    );
    let dumped = dump(&manifest);
    let section = dumped[0]
        .sections
        .iter()
        .find(|s| s.title == "Your sandbox")
        .expect("a `*` belt covers files, shell and code");
    for tool in ["file_write", "shell", "apply_patch"] {
        assert!(section.body.contains(tool), "{}", section.body);
    }
}

/// The inverse, and the reason the section is gated at all: a belt that
/// reaches none of the three namespaces must report the absence rather than
/// describe tools this agent cannot call.
///
/// Same feature gate as above — without `openhuman`, this manifest's
/// absence gets folded into the generic "Tool briefs (...)" deferred
/// line rather than a "Your sandbox" one.
#[test]
#[cfg(feature = "openhuman")]
fn a_belt_with_no_sandbox_namespace_defers_the_section() {
    let manifest = manifest(
        "[company]\nname = \"Acme\"\n\n[tools]\nallow = [\"workspace\"]\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\ntools = [\"workspace\"]\n",
    );
    let dumped = dump(&manifest);
    assert!(
        !dumped[0].sections.iter().any(|s| s.title == "Your sandbox"),
        "{:?}",
        dumped[0]
            .sections
            .iter()
            .map(|s| &s.title)
            .collect::<Vec<_>>()
    );
    assert!(
        dumped[0]
            .deferred
            .iter()
            .any(|entry| entry.title == "Your sandbox"),
        "{:?}",
        dumped[0].deferred
    );
}

/// An agent with no `prompt_files` has no brief, and that is reported as a
/// deferred line rather than silently producing a shorter prompt — the
/// whole point of the surface is to explain what an agent is missing.
#[test]
fn an_agent_without_prompt_files_reports_the_absent_brief() {
    let manifest = manifest(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
    );
    let dumped = dump(&manifest);
    assert!(
        dumped[0]
            .deferred
            .iter()
            .any(|entry| entry.title == "Your brief"),
        "{:?}",
        dumped[0].deferred
    );
}

/// `body()` must be the concatenation the harness performs, with nothing
/// inserted: a separator here would be bytes the agent never sees, and the
/// dump would stop being usable as a diff against a provider trace.
#[test]
fn the_body_is_the_sections_concatenated_with_nothing_between_them() {
    let manifest = manifest(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"PM\"\nprompt = \"Be brief.\"\n",
    );
    let dumped = dump(&manifest);
    let expected: String = dumped[0]
        .sections
        .iter()
        .map(|section| section.body.clone())
        .collect();
    assert_eq!(dumped[0].body(), expected);
    assert!(dumped[0].body().contains("Be brief."));
}

/// A brief containing a fenced block must not close the fence the report
/// wraps it in, or half the prompt renders as prose in the dump.
#[test]
fn a_body_containing_a_fence_is_wrapped_in_a_longer_one() {
    let fenced = fence("before\n```rust\nlet x = 1;\n```\nafter");
    assert!(fenced.starts_with("````text\n"), "{fenced}");
    assert!(fenced.trim_end().ends_with("````"), "{fenced}");
}

/// PR #1780 review: `build_agent`'s call site for the connected-integrations
/// brief is compiled only under `#[cfg(feature = "composio")]`
/// (`harness/built_in/build.rs`), so in a binary built without that
/// feature — the standard `scripts/dump-prompt.sh` invocation enables only
/// `openhuman` — the brief can never be appended, no matter what the grant
/// or credential state is at runtime. The deferred reason must say the
/// feature is missing, not describe compile-time absence as something that
/// "may appear once the grant and credential resolve".
///
/// Same feature gate as the sandbox tests above: `harness_sections` under
/// `#[cfg(feature = "openhuman")]` is the only place that pushes this
/// entry, and this test needs `composio` compiled *out* to reach the
/// branch it is checking.
#[test]
#[cfg(all(feature = "openhuman", not(feature = "composio")))]
fn without_composio_the_connected_integrations_brief_names_the_missing_feature() {
    let manifest = manifest(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
    );
    let dumped = dump(&manifest);
    let entry = dumped[0]
        .deferred
        .iter()
        .find(|d| d.title == "Connected integrations brief")
        .expect("always deferred outside a live build");
    assert!(
        entry.reason.contains("--features composio"),
        "a binary without `composio` can never compile in the call site that appends \
         this brief, so the reason must name the missing feature: {}",
        entry.reason
    );
    assert!(
        !entry.reason.contains("credential resolves"),
        "must not describe this as runtime-deferred when a `composio`-less binary can \
         never include it regardless of grant or credential state: {}",
        entry.reason
    );
}

/// The inverse of the test above: once `composio` IS compiled in, the
/// section really is runtime-deferred (an explicit grant plus a resolved
/// credential decide it), so the reason must keep describing that instead
/// of claiming the feature is missing.
#[test]
#[cfg(feature = "composio")]
fn with_composio_the_connected_integrations_brief_stays_runtime_deferred() {
    let manifest = manifest(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
    );
    let dumped = dump(&manifest);
    let entry = dumped[0]
        .deferred
        .iter()
        .find(|d| d.title == "Connected integrations brief")
        .expect("always deferred outside a live build");
    assert!(
        entry.reason.contains("credential resolves"),
        "a `composio`-enabled binary can still include this brief once the grant and \
         credential resolve, so the reason must keep saying so: {}",
        entry.reason
    );
    assert!(
        !entry.reason.contains("--features composio"),
        "must not claim the feature is missing when it is compiled in: {}",
        entry.reason
    );
}

/// The server-family brief must describe a missing `mcp` feature as the
/// compile-time absence it is, not as a section waiting on live server state.
///
/// Same shape as the Composio test above, and for the same reason: the call site
/// that appends this brief is `#[cfg(feature = "mcp")]`, so a binary without it
/// can never include the section whatever the company has configured.
#[test]
#[cfg(all(feature = "openhuman", not(feature = "mcp")))]
fn without_mcp_the_server_family_brief_names_the_missing_feature() {
    let manifest = manifest(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
    );
    let dumped = dump(&manifest);
    let entry = dumped[0]
        .deferred
        .iter()
        .find(|d| d.title == "MCP server-family brief")
        .expect("always reported, so the dump never looks complete while missing a section");
    assert!(
        entry.reason.contains("--features mcp"),
        "a binary without `mcp` wires neither dispatch tool, so the reason must name the \
         missing feature: {}",
        entry.reason
    );
    assert!(
        !entry.reason.contains("live server"),
        "must not describe this as runtime-deferred when an `mcp`-less binary can never \
         include it: {}",
        entry.reason
    );
}

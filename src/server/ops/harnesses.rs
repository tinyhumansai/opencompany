//! `GET {scope}/harnesses` (issue #1245's harness-picker follow-up): the
//! company's declared `[[harness]]` set, read-only.
//!
//! Before this route existed, nothing outside the process could see what
//! harnesses a company had declared — `AgentDetailDto`/`EditAgentInput` could
//! carry a `harness` binding (a manifest agent already could; an overlay
//! teammate gained the same field alongside this route), but the console had
//! no way to know *what it could bind to*, or which one is the default a
//! blank binding falls back to. Settings' Harnesses page and the per-agent
//! Harness picker both read this one list, so the two cannot disagree about
//! what the company has declared.
//!
//! Read-only, and stays that way: a harness is declared in the
//! version-controlled `company.toml`, the same blueprint
//! [`team_agent`](super::team_agent)'s own module docs describe the console as
//! never rewriting. Defining a *new* harness from the console is a
//! meaningfully bigger, separate piece of work — it would need an overlay
//! storage mechanism harnesses do not have today, the way
//! [`OverlayAgent`](crate::ports::types::OverlayAgent) gives a teammate one.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde::Serialize;

use crate::AppState;
use crate::company::{ACP_AGENTS, Harness};
use crate::error::OpenCompanyError;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the harnesses read route fragment.
pub fn router() -> Router<AppState> {
    scoped("/harnesses", get(list_harnesses))
}

/// One declared `[[harness]]`, as the console renders it in a picker.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HarnessDto {
    id: String,
    /// One of `HARNESS_KINDS` — `built_in` (managed) or `acp` (external).
    kind: String,
    /// Whether an agent naming no harness runs here. Exactly one entry in the
    /// list sets this.
    default: bool,
    /// `acp` harnesses only: which CLI (`claude`/`codex`), when
    /// `transport = "local"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    /// `acp` harnesses only: `local` (spawned on this machine) or `runner` (a
    /// registered remote).
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<String>,
    /// Whether this entry is **declared** in `company.toml` or merely
    /// **detected** — a coding CLI this build can drive that is bindable
    /// without any `[[harness]]` naming it
    /// ([`Harness::implicit_local`](crate::company::Harness::implicit_local)).
    ///
    /// The distinction is the whole contract with the console. A declared
    /// harness is a property of the *company* and is the same wherever the
    /// manifest is opened; a detected one is a property of the *machine*, and
    /// whether it can actually run is answered by that machine's own
    /// `acp::discovery` survey — which this host cannot see and deliberately
    /// does not guess at. The console joins the two on `id`.
    detected: bool,
    /// Whether **this host** can spawn this harness's transport.
    ///
    /// The console cannot work this out for itself, and the attempts to infer
    /// it were wrong in a way nobody sees. A desktop connected to a *remote*
    /// company still runs its own `acp::discovery` survey, so a declared
    /// `transport = "local"` harness was probed against the operator's laptop
    /// — reporting `Ready`, and offering to install an adapter — while turns
    /// for that company spawn on the remote host, where the CLI may be absent
    /// and no `AcpAgentFactory` may exist at all.
    ///
    /// So the host answers instead of the client guessing:
    ///
    /// - `built_in` — always true; it is this host's own engine.
    /// - `acp` + `local` — true only where this host wired an
    ///   `AcpAgentFactory`, which is the embedded desktop and nothing else.
    /// - `acp` + `runner` — false; it runs on a registered remote machine.
    ///
    /// False means "do not probe this against the machine you are on", not
    /// "broken": a hosted company's declared local harness is perfectly valid
    /// on the host that serves it.
    runs_here: bool,
}

/// `GET {scope}/harnesses` — every harness an agent here can be bound to.
///
/// Declared `[[harness]]` entries first, then every coding CLI this build
/// knows how to drive that the manifest does not already declare. A declared
/// entry always wins on id collision, so a company that pinned a model on its
/// own `claude` harness is never shadowed by the bare detected one.
///
/// Carries **no readiness**: whether a CLI is installed and signed in is a
/// fact about the operator's machine, not about this company, and this route
/// answers for any client on any machine. See [`HarnessDto::detected`].
async fn list_harnesses(
    State(state): State<AppState>,
    company: ScopedCompany,
) -> Result<Json<Vec<HarnessDto>>, ApiError> {
    let record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;

    let declared = record.manifest.effective_harnesses();

    // A coding CLI is only bindable where **this host** can spawn one, and
    // that is a property of the host rather than of the company: only the
    // embedded desktop wires an `AcpAgentFactory`, and `opencompany serve`
    // builds `AppState` without one. Advertising `claude` and `codex` from a
    // hosted host let an admin bind a teammate to a CLI on somebody else's
    // machine — accepted by `PATCH`, then dead on the next rebuild, because
    // the server has nothing to launch. `lanes.rs` already marks such a lane
    // unavailable; this stops the binding being offered in the first place.
    //
    // Declared entries are untouched: a manifest that names a `[[harness]]`
    // is stating what that deployment has, and this route does not get to
    // second-guess it.
    //
    // Issue #1814: `can_run_local_acp()`, not `acp_agents().is_some()`. The
    // desktop wires a factory on every build, so the bare accessor said yes on
    // a desktop compiled without `acp` — the one host where this route's whole
    // purpose applied and it silently did nothing.
    let can_run_local = state.can_run_local_acp();
    let runs_here = |harness: &Harness| match harness.kind.as_str() {
        "built_in" => true,
        "acp" => {
            can_run_local && harness.acp.as_ref().map(|a| a.transport.as_str()) == Some("local")
        }
        _ => false,
    };
    let detected = ACP_AGENTS
        .iter()
        .filter(|_| can_run_local)
        .filter(|id| !declared.iter().any(|h| h.id == **id))
        .map(|id| Harness::implicit_local(id));

    Ok(Json(
        declared
            .iter()
            .cloned()
            .map(|harness| {
                let here = runs_here(&harness);
                dto(harness, false, here)
            })
            .chain(detected.map(|harness| {
                let here = runs_here(&harness);
                dto(harness, true, here)
            }))
            .collect(),
    ))
}

fn dto(harness: Harness, detected: bool, runs_here: bool) -> HarnessDto {
    HarnessDto {
        id: harness.id,
        kind: harness.kind,
        default: harness.default,
        agent: harness.acp.as_ref().and_then(|acp| acp.agent.clone()),
        transport: harness.acp.map(|acp| acp.transport),
        detected,
        runs_here,
    }
}

#[cfg(test)]
mod test {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::CompanyStore;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-harnesses-")
            .tempdir()
            .expect("tempdir")
    }

    async fn state_with_manifest(home: &std::path::Path, manifest_toml: &str) -> AppState {
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                // Added on `main` by #1530's override layer while this test was
                // on `feat/external-acp`; empty is the right seed for a fixture
                // that declares its roster in the manifest.
                overlay_agent_edits: Vec::new(),
                overlay_retired_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn get(state: &AppState, path: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, body)
    }

    const BASE: &str = "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n";

    /// A **declared** local harness still says whether *this* host runs it.
    ///
    /// This is the half the detected-row filter does not cover, and it is the
    /// one that misled the console: a hosted company may legitimately declare
    /// `transport = "local"` — local to the host serving it — and a desktop
    /// opening that company probed the binding against its own laptop,
    /// reporting readiness and offering an install for a machine that never
    /// runs those turns. The row stays listed either way, because the binding
    /// is valid; only the "probe it here" claim changes.
    #[tokio::test]
    async fn a_declared_local_harness_says_whether_this_host_runs_it() {
        const WITH_LOCAL: &str = r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "CEO"

[[harness]]
id = "laptop"
kind = "acp"
default = true

[harness.acp]
transport = "local"
agent = "claude"
"#;
        let hosted_home = home();
        let hosted = state_with_manifest(hosted_home.path(), WITH_LOCAL).await;
        let (status, body) = get(&hosted, "/api/v1/company/harnesses").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let row = body
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == "laptop")
            .expect("a declared harness is listed whoever is serving it");
        assert_eq!(
            row["runsHere"], false,
            "a host with no ACP factory cannot spawn it: {body}"
        );
    }

    #[tokio::test]
    async fn a_company_with_no_harness_block_lists_the_implicit_default() {
        let home = home();
        let state = state_with_manifest(home.path(), BASE).await;

        let (status, body) = get(&state, "/api/v1/company/harnesses").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let list = body.as_array().unwrap();
        assert_eq!(list[0]["kind"], "built_in");
        assert_eq!(list[0]["default"], true);
        assert_eq!(list[0]["detected"], false, "declared, not detected");
    }

    /// A host that can spawn a coding CLI offers them; one that cannot does
    /// not.
    ///
    /// Issue #1245's detected-harness follow-up made every CLI this build
    /// drives bindable without a `[[harness]]`. That is true only where the
    /// host can actually run one. Offering them anyway let an admin bind a
    /// hosted teammate to a CLI on somebody else's machine: accepted by
    /// `PATCH`, then dead on the next rebuild.
    ///
    /// Issue #1814 corrects what "can run one" means. It is NOT "an
    /// `AcpAgentFactory` was wired" — the desktop shell wires one on every
    /// build, including one compiled without `acp`, where the runtime then
    /// forces `acp_agents = None` and every such lane resolves `unavailable`.
    /// So the wired-factory half below asserts the CLIs are offered only under
    /// `acp`, and asserts the opposite without it: this test used to encode
    /// the bug, passing in the default lane precisely because it agreed with
    /// it.
    #[tokio::test]
    async fn a_coding_cli_is_offered_only_where_this_host_can_run_one() {
        // Two homes, not one: `state_with_manifest` seeds a fixed admin, and
        // seeding the same address twice into one store is a conflict.
        let hosted_home = home();
        let desktop_home = home();

        // Without a factory — the hosted shape.
        let hosted = state_with_manifest(hosted_home.path(), BASE).await;
        let (status, body) = get(&hosted, "/api/v1/company/harnesses").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        for id in ["claude", "codex"] {
            assert!(
                !body.as_array().unwrap().iter().any(|h| h["id"] == id),
                "a host with nothing to spawn must not offer `{id}`: {body}"
            );
        }

        // With one — the desktop shape.
        struct StubFactory;
        impl crate::ports::acp::AcpAgentFactory for StubFactory {
            fn build(
                &self,
                _agent: &str,
                _model: Option<&str>,
                _agent_models: &std::collections::HashMap<String, String>,
                _workspace_root: &std::path::Path,
            ) -> crate::Result<std::sync::Arc<dyn crate::ports::acp::AcpAgent>> {
                unreachable!("this route never builds an agent")
            }
        }
        let desktop = state_with_manifest(desktop_home.path(), BASE)
            .await
            .with_acp_agents(std::sync::Arc::new(StubFactory));

        let (status, body) = get(&desktop, "/api/v1/company/harnesses").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let list = body.as_array().unwrap();
        for id in ["claude", "codex"] {
            let found = list.iter().find(|h| h["id"] == id);
            if cfg!(feature = "acp") {
                let found = found.expect("cli listed");
                assert_eq!(found["kind"], "acp", "{id}");
                assert_eq!(found["transport"], "local", "{id}");
                assert_eq!(found["agent"], id);
                assert_eq!(found["detected"], true, "{id}");
                assert_eq!(
                    found["default"], false,
                    "a detected CLI must never become the company default"
                );
            } else {
                // Issue #1814. A factory alone is not enough: without `acp`
                // this build cannot turn it into an engine, so offering `{id}`
                // here would be the desktop reproducing the very trap this
                // route exists to close.
                assert!(
                    found.is_none(),
                    "a build that cannot run `{id}` must not offer it, \
                     even with a factory wired: {body}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_declared_acp_harness_carries_its_agent_and_transport() {
        let home = home();
        let toml = format!(
            "{BASE}\n[[harness]]\nid = \"laptop\"\nkind = \"acp\"\ndefault = true\n\n\
             [harness.acp]\ntransport = \"local\"\nagent = \"claude\"\n"
        );
        let state = state_with_manifest(home.path(), &toml).await;

        let (status, body) = get(&state, "/api/v1/company/harnesses").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let laptop = body
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == "laptop")
            .expect("laptop harness listed");
        assert_eq!(laptop["kind"], "acp");
        assert_eq!(laptop["agent"], "claude");
        assert_eq!(laptop["transport"], "local");
        assert_eq!(laptop["default"], true);
    }

    #[tokio::test]
    async fn several_harnesses_are_all_listed() {
        let home = home();
        let toml = format!(
            "{BASE}\n[[harness]]\nid = \"main\"\nkind = \"built_in\"\ndefault = true\n\n\
             [[harness]]\nid = \"laptop\"\nkind = \"acp\"\n\n\
             [harness.acp]\ntransport = \"local\"\nagent = \"codex\"\n"
        );
        let state = state_with_manifest(home.path(), &toml).await;

        let (status, body) = get(&state, "/api/v1/company/harnesses").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ids: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            &ids[..2],
            &["main", "laptop"],
            "declared entries come first, in manifest order: {ids:?}"
        );
    }

    /// A declared harness is never shadowed by the detected entry of the same
    /// id — otherwise a company that pinned a model on its own `claude`
    /// harness would read back the bare synthesized one instead.
    #[tokio::test]
    async fn a_declared_harness_wins_over_the_detected_entry_of_the_same_id() {
        let home = home();
        let toml = format!(
            "{BASE}\n[[harness]]\nid = \"main\"\nkind = \"built_in\"\ndefault = true\n\n\
             [[harness]]\nid = \"claude\"\nkind = \"acp\"\n\n\
             [harness.acp]\ntransport = \"local\"\nagent = \"claude\"\nmodel = \"opus-4-5\"\n"
        );
        let state = state_with_manifest(home.path(), &toml).await;

        let (status, body) = get(&state, "/api/v1/company/harnesses").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claude: Vec<_> = body
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| h["id"] == "claude")
            .collect();
        assert_eq!(claude.len(), 1, "listed once, not twice: {body}");
        assert_eq!(claude[0]["detected"], false, "the declared one");
    }
}

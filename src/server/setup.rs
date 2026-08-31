//! The first-run setup surface: one flow that configures an instance.
//!
//! Everything an operator must decide to get a spun-up harness running is
//! otherwise spread across four places that never meet — host settings in a
//! hand-edited `config.toml`, the company template in a `serve --company` flag,
//! per-company settings behind six console sub-pages, and nothing at all
//! recording that any of it happened. This module is the single place that
//! reads all of it back and writes it in one transaction.
//!
//! ## Two routes
//!
//! - `GET /api/v1/setup` — everything the wizard needs to draw itself: the
//!   effective value of each field **with the layer that set it**, the template
//!   catalog, the sign-in modes this host will accept, and which optional
//!   surfaces are compiled into this build.
//! - `POST /api/v1/setup` — applies a completed wizard: writes `config.toml`,
//!   seeds the chosen company template, and stamps `setup_completed_at`.
//!
//! ## Why every field carries its layer
//!
//! Config resolution is `env ⟵ config.toml ⟵ manifest ⟵ default`
//! (`docs/spec/runtime/config.md`), and this flow can only write the *second*
//! layer. A hosted tenant has `OPENCOMPANY_BIND`, `OPENCOMPANY_DATA_DIR` and
//! friends injected by the control plane, so a wizard that cheerfully accepted
//! an edit to `bind` there would write a file, report success, and change
//! nothing — the env layer still wins at the next boot. So each field reports
//! its [`ConfigLayer`] and an `editable` flag, an env-owned field is rendered
//! read-only, and [`apply`] **refuses** a write to one rather than pretending.
//! That refusal is the point: silently ignored configuration is the failure
//! mode this surface exists to prevent.
//!
//! ## Applied, or only staged
//!
//! Host-level fields are read once, at boot: `bind` binds a socket, `[workspace]`
//! decides the data-dir lifecycle. Writing those is a *staged* change, and each
//! says so via `requires_restart` — the same honesty `InferenceStatusDto`
//! practices with its own `restart_required` flag.
//!
//! `auth_mode` is deliberately **not** in that category, even though it is also
//! resolved at build and cached on the runtime. Picking a sign-in mode and then
//! being shown a sign-in form is the single most confusing thing this flow could
//! do, and "restart the host yourself" is not an answer on a first run. So the
//! apply makes the mode live on the [`AppState`] *before* it builds anything,
//! then rebuilds the companies that were already registered
//! ([`rebuild_company`](crate::runtime::rebuild_company)). A host with no
//! rebuilder wired is the only case that still needs a restart, and it is
//! reported per-company rather than assumed either way — `restart_required`
//! names what is genuinely still pending, never a guess.
//!
//! Per-company settings (inference, MCP servers, team) are not written here at
//! all: they go through the existing `ops` routes, which apply live.
//!
//! ## Who may call it
//!
//! Loopback-only throughout — nothing here is ever open to a routable host. On
//! top of that, the call is open without a session in exactly two situations,
//! both meaning "there is nobody who could authorize it": setup has never
//! completed, or the host has no companies and therefore no roster to hold an
//! admin.
//!
//! Both conditions being loopback-gated is the point. Openness on a routable
//! host would let whoever reached a fresh deployment first configure it;
//! openness on a *configured* laptop would let any page in the browser rewrite
//! its settings. The no-companies case is not a nicety either: setup can
//! complete without seeding a company, and gating that host behind an admin
//! check would leave it with no company to sign in to and no way back into
//! setup to make one — the dead end this flow exists to remove, one step later.
//!
//! Otherwise the ordinary admin check applies. This mirrors how the login routes
//! already treat loopback (`is_local_only` gates echoing a login code).

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::app::config::{
    AuthMode, ConfigFile, ConfigLayer, ConfigValue, EnvSource, ProcessEnv, resolve,
    write_config_toml,
};
use crate::error::OpenCompanyError;
use crate::server::error::ApiError;
use crate::server::users::admin::require_admin;

/// Builds the setup route fragment.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/setup", get(read).post(apply))
        .route("/api/v1/setup/roster", post(propose_roster))
        .route("/api/v1/setup/inference/test", post(test_inference))
}

// ---------------------------------------------------------------------------
// Response envelopes
// ---------------------------------------------------------------------------

/// One configurable field, with the layer that currently owns it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct FieldDto {
    /// The dotted `config.toml` key (`bind`, `workspace.max_blob_mb`).
    pub key: &'static str,
    /// The value currently held in `config.toml`, or `null` when the file does
    /// not set it. Always `null` for a field marked [`secret`](Self::secret) —
    /// a credential's status is reportable, its bytes are not.
    pub value: Option<String>,
    /// Which layer supplied it: `env`, `config.toml`, `manifest`, `default`.
    pub layer: &'static str,
    /// Whether the wizard may write it. `false` when `env` owns the field,
    /// because `config.toml` cannot outrank an environment variable.
    pub editable: bool,
    /// Whether a change takes effect only after the host restarts.
    pub requires_restart: bool,
    /// Set when the field holds a credential: the wizard shows "configured"
    /// rather than the value, and `value` is `None`.
    pub secret: bool,
}

/// A company template an instance can be seeded from.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TemplateDto {
    /// The stable preset slug, e.g. `agentic_marketing_agency`.
    pub id: &'static str,
    /// The human-readable name.
    pub name: &'static str,
    /// How many agents the template's roster declares, so the wizard can say
    /// what the operator is about to get without parsing the manifest itself.
    pub agent_count: usize,
    /// What a company built from this template produces (the manifest's
    /// `[company].output`), so a template card can say what the operator is
    /// choosing in the product's own words rather than a restated slug.
    pub output: Option<String>,
}

/// Which optional surfaces are compiled into this build.
///
/// These are **cargo features**, not settings: nothing the wizard writes can
/// turn one on. Reported so the flow can say "ACP is not in this build" instead
/// of offering a switch that does nothing. Mirrors the `*_in_build` flags
/// `ops::capabilities` already publishes.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct BuildDto {
    /// The Agent Client Protocol module (`acp`).
    pub acp_in_build: bool,
    /// Whether the ACP JSON-RPC transport is actually mounted. Distinct from
    /// `acp_in_build`: this tree compiles the session and permission model but
    /// mounts no `/acp` handler, so a client would get the reserved-path 404
    /// even in a build with the feature on. Saying so is the difference between
    /// "not available" and "misconfigured".
    pub acp_transport_mounted: bool,
    /// MCP tool-server management (`mcp`).
    pub mcp_in_build: bool,
    /// The embedded OpenHuman agent harness (`openhuman`).
    pub harness_in_build: bool,
    /// Third-party OAuth connection writes (`oauth`).
    pub oauth_in_build: bool,
}

/// The wizard's whole bootstrap payload.
#[derive(Clone, Debug, Serialize)]
pub struct SetupDto {
    /// Whether setup has already been completed on this instance.
    pub complete: bool,
    /// Absolute path of the `config.toml` a write would land in, so the flow can
    /// tell the operator where its output goes.
    pub config_path: String,
    /// Every configurable field, in a stable order.
    pub fields: Vec<FieldDto>,
    /// The company templates this build ships.
    pub templates: Vec<TemplateDto>,
    /// The sign-in modes this host will accept. `none` is absent on a routable
    /// bind, where it would mean an unauthenticated admin console.
    ///
    /// Which modes are *legal*, not which are convenient: `email` is listed on
    /// a host with no mail transport too, because hub OAuth and passwords sign
    /// people in there perfectly well. Read [`mail`](Self::mail) for what the
    /// magic-link path specifically can do today.
    pub auth_modes: Vec<&'static str>,
    /// Which optional surfaces this build has.
    pub build: BuildDto,
    /// Company ids already registered on this host. A non-empty list means the
    /// seed step should be skipped — the instance already has a company.
    pub companies: Vec<String>,
    /// What this host can already reach without the operator supplying anything.
    pub inference: InferenceReadyDto,
    /// What this host can do with a mailbox.
    pub mail: MailReadyDto,
}

/// What this host can do with a mailbox, so the wizard never offers a
/// sign-in that arrives nowhere.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct MailReadyDto {
    /// A transport *and* credentials are wired (`OPENCOMPANY_MAIL_*`). Not
    /// "a send will succeed" — the same predicate the login and invite
    /// routes branch on, so all three keep one answer.
    pub wired: bool,
    /// A minted code comes back in the response instead of going to a
    /// mailbox: loopback bind, no `public_url`, no transport. The laptop
    /// case, where the honest hand-off is a link rather than an inbox.
    pub echoes_code: bool,
}

/// The credential this host already holds, for the wizard's first step.
///
/// A hosted tenant has one injected by the control plane, and its operator has
/// no key of their own and no way to get one. Reporting this is what lets the
/// step arrive already answered — pre-filled and testable — instead of demanding
/// something unobtainable.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct InferenceReadyDto {
    /// Whether a credential is already resolvable, so the design pass would run
    /// with the operator typing nothing.
    ///
    /// Answered by constructing the pass itself rather than by re-reading the
    /// environment — see [`house_credential`]. A console that decided this
    /// differently would pre-fill a step on a host that then silently shipped a
    /// keyword-matched template.
    pub ready: bool,
    /// The provider slug behind it, for the picker's initial value. Always
    /// `managed` today: the injected path is the platform's own endpoint.
    pub provider: Option<&'static str>,
    /// The endpoint it resolves to. Shown, not secret — it is a URL, and seeing
    /// which one a test is about to hit is the difference between a green tick
    /// and a green tick you can trust.
    pub base_url: Option<String>,
}

/// Whether the host already holds a usable credential, and where it points.
///
/// Deliberately implemented by asking the harness for the very config the design
/// pass would run on, not by re-reading `OPENCOMPANY_INFERENCE_KEY` and friends.
/// That resolution is already subtle — a projected token file ahead of a static
/// key, an explicit URL ahead of the default — and a second copy of it in the
/// console layer would eventually disagree with the one that matters.
///
/// The credential itself never leaves this function.
#[cfg(feature = "openhuman")]
fn house_credential(env: &dyn EnvSource) -> Option<String> {
    crate::harness::provider::harness_inference_from_env(env).map(|(config, _)| config.base_url)
}

/// Without the harness there is no inference path at all, so the host holds
/// nothing and the step asks for everything.
#[cfg(not(feature = "openhuman"))]
fn house_credential(_env: &dyn EnvSource) -> Option<String> {
    None
}

/// What `POST /api/v1/setup` accepts.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SetupRequest {
    /// Field writes, as dotted key → value. A `null` value clears the key,
    /// letting the next precedence layer supply it.
    pub fields: std::collections::BTreeMap<String, Option<String>>,
    /// The template to seed the first company from. Ignored when the host
    /// already has a company — setup must never hand an operator a second
    /// starter company on a re-run.
    pub template: Option<String>,
    /// What to call the company this setup creates.
    ///
    /// Absent means "derive it", which is what every company made here used to
    /// get with no way to say otherwise: [`company_name`] takes the first
    /// clause of the *industry* answer, so a field labelled "what kind of
    /// company are you setting up?" silently named the company — and the id is
    /// minted from that name and then fixed
    /// ([`company_id_from_name`](crate::runtime::company_id_from_name)), with
    /// no rename anywhere in the product. Sent by the review step, which is the
    /// last screen before that becomes permanent.
    ///
    /// Applies to both paths: a designed company and a seeded template. Blank
    /// or whitespace is treated as absent rather than as a name, since an empty
    /// company name slugs to the literal id `company`.
    ///
    /// [`company_name`]: crate::company::setup::manifest_from_setup
    pub name: Option<String>,
    /// The address that will be able to sign in, for the template path.
    ///
    /// The designed path carries its own inside [`SetupCompany`], and that is
    /// where this used to live exclusively — which was fine while a designed
    /// company was the only thing the console ever sent. It is not any more: a
    /// picked template is now seeded as itself, and no shipped product template
    /// names an admin, so an operator who chose email sign-in and a template
    /// would finish setup into a company they cannot administer.
    ///
    /// Ignored when the seeded manifest asks nobody to sign in — see
    /// [`SeedOverrides::admin_email`](crate::desktop::SeedOverrides::admin_email).
    pub admin_email: Option<String>,
    /// A company the wizard **designed**, from the operator's answers and the
    /// roster they reviewed.
    ///
    /// Takes precedence over [`Self::template`] when both are present: an
    /// operator who answered three questions and edited a roster has expressed
    /// a preference that a template slug cannot override. The template path
    /// stays for `desktop::bootstrap_companies` and for any caller that still
    /// wants a preset.
    pub company: Option<SetupCompany>,
}

/// The company the wizard designed, as it arrives from the review step.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SetupCompany {
    industry: String,
    team_hint: String,
    automate: String,
    /// The roster **as reviewed** — renamed, trimmed and reordered by the
    /// operator. Sent back rather than regenerated, so what they approved is
    /// exactly what gets built; a second pass could return something else.
    agents: Vec<SetupCompanyAgent>,
    /// The address that will be able to sign in. Written into `[users].admins`,
    /// which is the only reason a laptop operator who chose email sign-in is
    /// not locked out of the company they just made.
    admin_email: Option<String>,
    /// The provider that passed the setup probe. Persisted with the company so
    /// the first agent turn uses the same endpoint the operator tested.
    inference: Option<SetupInference>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SetupInference {
    provider: String,
    base_url: Option<String>,
    model: Option<String>,
    /// Write-only credential. Never appears in a response or manifest.
    key: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SetupCompanyAgent {
    name: String,
    role: String,
    description: String,
    /// The job shape the design pass assigned, round-tripped through the review
    /// screen untouched — it is what decides this teammate's tool belt
    /// (`crate::company::setup::AgentFocus`).
    ///
    /// A `String` here rather than the enum, resolved through `from_wire`: this
    /// is the one field the console never shows and never edits, so an older or
    /// unknown spelling should cost that teammate its narrowing rather than
    /// reject the whole apply an operator has just confirmed.
    focus: Option<String>,
}

/// What the apply returns.
#[derive(Clone, Debug, Serialize)]
pub struct AppliedDto {
    /// Always true — a partial apply is an error, not a result.
    pub complete: bool,
    /// The file written.
    pub config_path: String,
    /// Keys whose new value only takes effect after a restart, so the console
    /// can say which ones are still pending rather than implying all of it is
    /// live.
    pub restart_required: Vec<String>,
    /// The company seeded by this call, if any.
    pub seeded_company: Option<String>,
}

// ---------------------------------------------------------------------------
// The field table
// ---------------------------------------------------------------------------

/// One row of the configurable-field table.
struct FieldSpec {
    /// The dotted `config.toml` key.
    key: &'static str,
    /// The [`ConfigProvenance`](crate::app::config::ConfigProvenance) field name
    /// this key resolves under. `None` for keys that have no resolution pass of
    /// their own (the `[workspace]` section, read straight off the file).
    prov: Option<&'static str>,
    /// Whether a change needs a restart to take effect.
    requires_restart: bool,
    /// Whether the value is a credential and must never be echoed.
    secret: bool,
}

/// Every field the setup flow owns, in the order the wizard shows them.
///
/// Deliberately not "every key in `ConfigFile`": `data_dir` is excluded because
/// a running host has already opened and locked its data root, so writing a new
/// one into the very file that lives *inside* that root would leave a config
/// nothing reads. Moving a data root is a relocation, not a setting.
const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "bind",
        prov: Some("bind"),
        requires_restart: true,
        secret: false,
    },
    FieldSpec {
        key: "auth_mode",
        prov: Some("auth_mode"),
        requires_restart: true,
        secret: false,
    },
    FieldSpec {
        key: "brain_mode",
        prov: Some("brain_mode"),
        requires_restart: true,
        secret: false,
    },
    FieldSpec {
        key: "api_url",
        prov: Some("api_url"),
        requires_restart: true,
        secret: false,
    },
    FieldSpec {
        key: "tinyhumans_api_key",
        prov: Some("tinyhumans_credential"),
        requires_restart: true,
        secret: true,
    },
    FieldSpec {
        key: "openhuman_url",
        prov: Some("openhuman_url"),
        requires_restart: true,
        secret: false,
    },
    FieldSpec {
        key: "public_url",
        prov: Some("public_url"),
        requires_restart: true,
        secret: false,
    },
    FieldSpec {
        key: "tinyplace_api_url",
        prov: Some("tinyplace_api_url"),
        requires_restart: true,
        secret: false,
    },
    FieldSpec {
        key: "github_token",
        prov: Some("github_token"),
        requires_restart: true,
        secret: true,
    },
    FieldSpec {
        key: "workspace.clear_tmp_on_startup",
        prov: None,
        requires_restart: true,
        secret: false,
    },
    FieldSpec {
        key: "workspace.max_blob_mb",
        prov: None,
        requires_restart: true,
        secret: false,
    },
    FieldSpec {
        key: "workspace.storage_quota_gb",
        prov: None,
        requires_restart: true,
        secret: false,
    },
    FieldSpec {
        key: "workspace.tree_quota_gb",
        prov: None,
        requires_restart: true,
        secret: false,
    },
];

/// Whether `key` is one this surface will write at all. An unknown key is
/// refused rather than passed through to `config.toml`, so a typo cannot write
/// a dead entry that reads as configuration.
fn spec_for(key: &str) -> Option<&'static FieldSpec> {
    FIELDS.iter().find(|f| f.key == key)
}

// ---------------------------------------------------------------------------
// Access control
// ---------------------------------------------------------------------------

/// Authorizes a setup call.
///
/// Loopback-only is necessary throughout: nothing here is ever open to a
/// routable host. On top of that, the call is open without a session in exactly
/// two situations, both of which are "there is nobody who could authorize it":
///
///   - setup has never completed, or
///   - the host has no companies, so there is no roster to hold an admin.
///
/// The second is not a nicety. Setup can complete without seeding a company —
/// an operator who only changes host settings — and gating that host behind an
/// admin check would leave it with no company to sign in to and no way back
/// into setup to create one. That is precisely the dead end this flow exists to
/// remove, reintroduced one step later.
///
/// `state.config().is_local_only()` is a statement about the *configured*
/// bind and `public_url`, not about this particular request — so it alone
/// cannot refuse a request that reaches a loopback-bound listener through an
/// undeclared reverse proxy. `request_looks_local` closes that gap: it checks
/// the request's actual TCP peer and rejects any proxy-forwarding header, the
/// same two-gate check `none`-mode login uses for the same reason (see
/// [`crate::server::graphql::auth::local_owner`]).
///
/// Otherwise the ordinary admin check applies, resolved through the sole
/// company: setup is host-level but authority is per company, and a host
/// serving several has no single roster that could speak for the instance.
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> Result<(), crate::server::Rejection> {
    if state.config().is_local_only()
        && (!state.setup_complete() || state.registry().is_empty())
        && crate::server::graphql::auth::request_looks_local(peer, headers)
    {
        return Ok(());
    }
    let runtime = state.registry().sole().ok_or_else(|| {
        // `sole` is `None` for an empty registry too, but that case was handled
        // above for a loopback host — so reaching here means either several
        // companies, or none on a routable host. Say what is true of both
        // rather than naming a count that might be wrong.
        ApiError::from(OpenCompanyError::Conflict(
            "No single company on this host can authorize a host-level change. Edit \
             config.toml directly and restart."
                .to_string(),
        ))
        .into_response()
    })?;
    require_admin(headers, state, &runtime, peer).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// GET
// ---------------------------------------------------------------------------

async fn read(
    State(state): State<AppState>,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    headers: HeaderMap,
) -> Result<Json<SetupDto>, crate::server::Rejection> {
    authorize(&state, &headers, peer).await?;
    snapshot(&state, &ProcessEnv)
        .map(Json)
        .map_err(|e| ApiError::from(e).into_response().into())
}

/// The manifest the resolution pass runs against.
///
/// Setup is a host-level surface and has no single company to read a manifest
/// from, so it uses the same synthetic stand-in `opencompany doctor` does for
/// the same reason (`src/bin/opencompany.rs`). Its `[brain].mode` and
/// `[users].mode` take their serde defaults — `hosted` and `email` — which are
/// precisely the values resolution would otherwise fall through to, so the
/// manifest layer reports exactly what an unconfigured host would resolve.
fn synthetic_manifest() -> crate::company::CompanyManifest {
    toml::from_str("[company]\nname = \"opencompany\"\n").expect("synthetic manifest is valid")
}

/// Builds the wizard payload from the live configuration.
fn snapshot(state: &AppState, env: &dyn EnvSource) -> Result<SetupDto, OpenCompanyError> {
    // `config_root`, not `home`: this is where startup resolves
    // `setup_completed_at` and every other `config.toml` key from, and the two
    // can diverge on a deployment with an explicit `--home` — see
    // `AppState::config_root`'s doc.
    let dir = state.config_root();
    let file = ConfigFile::load(dir)?;

    // Resolution needs a manifest for the `[brain]`/`[users]` layer, and this is
    // a host-level surface with no single company to read one from. A default
    // manifest is what `opencompany doctor` uses for the same reason, and its
    // `[brain].mode`/`[users].mode` defaults are exactly the values resolution
    // would fall through to anyway.
    let manifest = synthetic_manifest();
    let (_, prov) = resolve(env, file.as_ref(), &manifest)?;

    let fields = FIELDS
        .iter()
        .map(|spec| {
            let layer = spec
                .prov
                .and_then(|name| prov.layer(name))
                // A `[workspace]` key has no resolution pass: it is either in
                // the file or it is a built-in default.
                .unwrap_or(if workspace_key_present(file.as_ref(), spec.key) {
                    ConfigLayer::ConfigToml
                } else {
                    ConfigLayer::Default
                });
            FieldDto {
                key: spec.key,
                value: if spec.secret {
                    None
                } else {
                    effective_value(file.as_ref(), spec.key)
                },
                layer: layer.label(),
                editable: layer != ConfigLayer::Env,
                requires_restart: spec.requires_restart,
                secret: spec.secret,
            }
        })
        .collect();

    Ok(SetupDto {
        complete: state.setup_complete(),
        config_path: dir
            .join(crate::app::config::CONFIG_FILE)
            .display()
            .to_string(),
        fields,
        templates: templates(),
        auth_modes: auth_modes(state),
        // Asked through the login route's own predicates rather than re-read
        // from the environment here: a second spelling of "can this host mail"
        // is exactly how the wizard's copy and the route's behaviour drift into
        // contradicting each other.
        mail: MailReadyDto {
            wired: crate::server::users::routes::mail_transport_wired(state),
            echoes_code: crate::server::users::routes::echoes_code_in_response(state),
        },
        build: build_flags(),
        companies: state
            .registry()
            .list()
            .into_iter()
            .map(|id| id.as_ref().to_string())
            .collect(),
        inference: {
            let base_url = house_credential(env);
            InferenceReadyDto {
                ready: base_url.is_some(),
                provider: base_url.is_some().then_some("managed"),
                base_url,
            }
        },
    })
}

/// The sign-in modes this host will accept.
///
/// `none` is withheld on a routable bind: a company with no sign-in served to
/// anything but loopback is an unauthenticated admin console, and the runtime
/// already refuses that combination at boot, at provisioning, and in the
/// desktop loader. Offering it in the wizard would only produce a choice that
/// fails on the next restart.
fn auth_modes(state: &AppState) -> Vec<&'static str> {
    let mut modes = vec![AuthMode::Email.as_str(), AuthMode::Wallet.as_str()];
    if state.config().is_local_only() {
        modes.push(AuthMode::None.as_str());
    }
    modes
}

/// Which optional surfaces this build carries.
fn build_flags() -> BuildDto {
    BuildDto {
        acp_in_build: cfg!(feature = "acp"),
        acp_transport_mounted: cfg!(feature = "acp"),
        mcp_in_build: cfg!(feature = "mcp"),
        harness_in_build: cfg!(feature = "openhuman"),
        oauth_in_build: cfg!(feature = "oauth"),
    }
}

/// The shipped company templates.
fn templates() -> Vec<TemplateDto> {
    crate::desktop::PRESETS
        .iter()
        .map(|preset| {
            // A preset that will not parse is a packaging bug, but it must not
            // take the whole wizard down with it: report the entry with an
            // unknown roster rather than failing the request.
            let parsed = preset.manifest_parsed().ok();
            TemplateDto {
                id: preset.id,
                name: preset.name,
                agent_count: parsed.as_ref().map(|m| m.agents.len()).unwrap_or(0),
                output: parsed.and_then(|m| m.company.output.clone()),
            }
        })
        .collect()
}

/// The current value of `key` as a display string, read from the file layer.
fn effective_value(file: Option<&ConfigFile>, key: &str) -> Option<String> {
    let file = file?;
    match key {
        "bind" => file.bind.clone(),
        "auth_mode" => file.auth_mode.clone(),
        "brain_mode" => file.brain_mode.clone(),
        "api_url" => file.api_url.clone(),
        "openhuman_url" => file.openhuman_url.clone(),
        "public_url" => file.public_url.clone(),
        "tinyplace_api_url" => file.tinyplace_api_url.clone(),
        "workspace.clear_tmp_on_startup" => {
            file.workspace.clear_tmp_on_startup.map(|v| v.to_string())
        }
        "workspace.max_blob_mb" => file.workspace.max_blob_mb.map(|v| v.to_string()),
        "workspace.storage_quota_gb" => file.workspace.storage_quota_gb.map(|v| v.to_string()),
        "workspace.tree_quota_gb" => file.workspace.tree_quota_gb.map(|v| v.to_string()),
        _ => None,
    }
}

/// Whether a `[workspace]` key is set in the file, which is the only way those
/// keys can be owned by anything but the built-in default.
fn workspace_key_present(file: Option<&ConfigFile>, key: &str) -> bool {
    effective_value(file, key).is_some()
}

// ---------------------------------------------------------------------------
// POST
// ---------------------------------------------------------------------------

async fn apply(
    State(state): State<AppState>,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    headers: HeaderMap,
    Json(req): Json<SetupRequest>,
) -> Result<Json<AppliedDto>, crate::server::Rejection> {
    authorize(&state, &headers, peer).await?;
    apply_inner(&state, req, &ProcessEnv)
        .await
        .map(Json)
        .map_err(|e| ApiError::from(e).into_response().into())
}

/// `+ Sync` so the returned future is `Send`, which axum requires of a handler:
/// a `&T` is `Send` only when `T` is `Sync`, and the bare trait object is not.
async fn apply_inner(
    state: &AppState,
    req: SetupRequest,
    env: &(dyn EnvSource + Sync),
) -> Result<AppliedDto, OpenCompanyError> {
    // See `snapshot`'s comment: this must be the same root startup reads
    // `config.toml` from, which is not always `state.home()`.
    let dir = state.config_root().to_path_buf();
    let file = ConfigFile::load(&dir)?;
    let manifest = synthetic_manifest();
    let (_, prov) = resolve(env, file.as_ref(), &manifest)?;

    // Validate everything before writing anything: a config half-applied is
    // worse than one refused, because the operator has no way to tell which
    // half landed.
    let mut edits: Vec<(&'static str, ConfigValue)> = Vec::new();
    let mut restart_required = Vec::new();
    for (key, value) in &req.fields {
        let spec = spec_for(key).ok_or_else(|| {
            OpenCompanyError::InvalidRequest(format!("`{key}` is not a setting this flow writes."))
        })?;

        // The refusal that gives this surface its point: `config.toml` cannot
        // outrank an environment variable, so accepting the edit would write a
        // file, report success, and change nothing at the next boot.
        if spec.prov.and_then(|n| prov.layer(n)) == Some(ConfigLayer::Env) {
            return Err(OpenCompanyError::Conflict(format!(
                "`{key}` is set by an environment variable on this host, which outranks \
                 config.toml. Writing it here would have no effect — change the environment \
                 instead."
            )));
        }

        let parsed = parse_value(spec, value.as_deref())?;
        // `bind` gets one more check `parse_value` cannot do on its own: it
        // needs to resolve, the same way the boot path's own bind attempt
        // will. Checked here, not at the next boot, for the same reason the
        // `auth_mode` parse above runs before the write — an unresolvable
        // `bind` aborts `TcpListener::bind` at startup, which would turn a
        // typo here into a host that will not come back up.
        if spec.key == "bind"
            && let ConfigValue::Str(addr) = &parsed
        {
            validate_bind(addr).await?;
        }
        edits.push((spec.key, parsed));
        if spec.requires_restart {
            restart_required.push(spec.key.to_string());
        }
    }

    // Validate the mode before it is written, not at the next boot: an
    // unparseable `auth_mode` aborts startup, which would turn a typo here into
    // a host that will not come back up.
    let chosen_auth_mode = match req.fields.get("auth_mode") {
        // Present and non-blank: the operator picked one.
        Some(Some(mode)) if !mode.trim().is_empty() => {
            // Re-tagged as a bad *request* rather than a bad *config*:
            // `AuthMode::from_str` raises `Config`, which maps to 500 — right
            // for a malformed file found at boot, wrong for a value just typed.
            let parsed: AuthMode = mode
                .trim()
                .parse()
                .map_err(|e: OpenCompanyError| OpenCompanyError::InvalidRequest(e.to_string()))?;
            if !parsed.has_login() && !state.config().is_local_only() {
                return Err(OpenCompanyError::Conflict(
                    "`none` has no sign-in, and this host binds a routable address — it would \
                     serve an unauthenticated admin console to anyone who can reach it."
                        .to_string(),
                ));
            }
            Some(Some(parsed))
        }
        // Present and cleared: back to each manifest's own `[users].mode`.
        Some(None) => Some(None),
        // Absent: not this operator's business, leave the host-wide mode alone.
        _ => None,
    };

    // Validate the model choice before writing setup state. The endpoint that
    // passed the probe is normalized once more at this trust boundary, then
    // stored on the company rather than forgotten after the green tick.
    let designed_inference = req
        .company
        .as_ref()
        .and_then(|company| company.inference.as_ref())
        .map(|input| {
            let mut models = std::collections::BTreeMap::new();
            if let Some(model) = input
                .model
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
            {
                for tier in crate::company::INFERENCE_TIERS {
                    models.insert((*tier).to_string(), model.to_string());
                }
            }
            let config = crate::company::inference::RuntimeInference {
                provider: input.provider.trim().to_string(),
                base_url: crate::company::inference::normalize_setup_base_url(
                    &input.provider,
                    input.base_url.as_deref(),
                ),
                models,
            };
            let problems = crate::company::inference::validate_runtime(&config);
            if problems.is_empty() {
                Ok(config)
            } else {
                Err(OpenCompanyError::InvalidRequest(problems.join(" ")))
            }
        })
        .transpose()?;

    // Persist before anything live is mutated. The module doc promises "writes
    // it in one transaction" and `AppliedDto::complete` promises a partial
    // apply is an error, not a result — both break if a later step (auth
    // override, seeding, rebuild) runs before the write, because a failed
    // write would then return an error while the live host already reflects
    // the change. Persisting first means a write failure leaves the process
    // exactly as it was: nothing below has run yet.
    edits.push((
        "setup_completed_at",
        ConfigValue::Int(crate::ports::now_millis() as i64),
    ));
    let path = write_config_toml(&dir, &edits)?;
    state.mark_setup_complete();

    // Make the chosen mode live **before** anything is built with it.
    //
    // The mode is resolved once, at build, and cached on the runtime. Writing it
    // to `config.toml` alone therefore only takes effect at the next boot —
    // which is why picking "no sign-in" used to leave the operator staring at a
    // login form on a host that had just been told not to have one. Setting it
    // here means the company seeded below is built with it, and the rebuild
    // further down applies it to anything already registered.
    if let Some(mode) = chosen_auth_mode {
        state.set_auth_mode_override(mode);
    }

    // Seed the template only when the host has no company. A re-run must never
    // hand the operator a second starter company, which is the same reason
    // `desktop::bootstrap_companies` seeds only as a fallback.
    // Blank is not a name. `company_id_from_name` slugs an empty string to the
    // literal id `company`, so an operator who cleared the field would get a
    // company called nothing at an id naming nothing — deriving is the better
    // answer to "I typed no name" than obeying it is.
    let chosen_name: Option<String> = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        // Bounded at the boundary, by the same 60 characters `company_name`
        // clamps its derivation to. Not cosmetic: `company_id_from_name` keeps
        // every alphanumeric character it is given, and that id becomes one
        // directory component under the store — so a pasted paragraph makes the
        // apply fail while writing the bundle, on most filesystems at 255
        // bytes. Truncated rather than refused, because a name is not a
        // credential and the operator can see what they typed.
        .map(|name| {
            name.chars()
                .take(crate::company::setup::MAX_COMPANY_NAME)
                .collect::<String>()
        })
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    let chosen_name = chosen_name.as_deref();

    let seeded = match (&req.company, &req.template, state.registry().is_empty()) {
        // A designed company wins over a template slug: the operator answered
        // three questions and edited the roster, which a preset cannot override.
        (Some(designed), _, true) => {
            let answers = crate::company::setup::SetupAnswers {
                industry: designed.industry.clone(),
                team_hint: designed.team_hint.clone(),
                automate: designed.automate.clone(),
            };
            let agents: Vec<crate::company::setup::ProposedAgent> = designed
                .agents
                .iter()
                .map(|a| crate::company::setup::ProposedAgent {
                    name: a.name.clone(),
                    role: a.role.clone(),
                    description: a.description.clone(),
                    focus: a
                        .focus
                        .as_deref()
                        .and_then(crate::company::setup::AgentFocus::from_wire),
                })
                .collect();
            // Validated again on the way in, for the same reason the roster pass
            // validates the model's answer: this arrived over the wire and the
            // operator edited it, so neither the bounds nor the de-duplication
            // can be assumed to have survived.
            let agents = crate::company::setup::validate_roster(agents);
            let mut manifest = crate::company::setup::manifest_from_setup(
                &answers,
                &agents,
                designed.admin_email.as_deref(),
            );
            // Overrides the derived name, and only the name: everything else
            // `manifest_from_setup` decided is still what a provisioned company
            // gets. Set before `seed_generated_company`, because the id is
            // minted from this field there and never again.
            if let Some(name) = chosen_name {
                manifest.company.name = name.to_string();
            }
            if let Some(inference) = &designed_inference {
                manifest.inference.provider = Some(inference.provider.clone());
                manifest.inference.base_url = inference.base_url.clone();
                manifest.inference.models = inference.models.clone();
            }
            let id = crate::desktop::seed_generated_company(state, manifest, Some(answers)).await?;
            if let Some(key) = designed
                .inference
                .as_ref()
                .and_then(|inference| inference.key.as_deref())
                .map(str::trim)
                .filter(|key| !key.is_empty())
                && let Some(runtime) = state.registry().get(&id)
            {
                crate::company::inference::store_key(&id, runtime.secrets().as_ref(), key).await?;
            }
            Some(id.as_ref().to_string())
        }
        (None, Some(template), true) => {
            // The template path, and now a reachable one: the console sends a
            // slug rather than a designed company when the operator picked a
            // template and no model tailored it, so what gets seeded is that
            // template — its roster, its tool belt, its prompts — instead of an
            // approximation rebuilt from a roster screen.
            let id = crate::desktop::seed_company_with(
                state,
                template,
                crate::desktop::SeedOverrides {
                    name: chosen_name,
                    admin_email: req
                        .admin_email
                        .as_deref()
                        .map(str::trim)
                        .filter(|email| !email.is_empty()),
                },
            )
            .await?;
            Some(id.as_ref().to_string())
        }
        _ => None,
    };

    // Companies that already existed still hold the old mode on their cached
    // runtime, so rebuild them in place. `seeded` is excluded — it was just
    // built with the new mode and rebuilding it would be pure work.
    //
    // A host with no rebuilder wired cannot do this, and that is the only case
    // where `auth_mode` genuinely still needs a process restart. Reporting it
    // per-company rather than assuming either answer is what keeps the response
    // honest about what is actually in force.
    let mut needs_restart_for_auth = false;
    if chosen_auth_mode.is_some() {
        for id in state.registry().list() {
            if seeded.as_deref() == Some(id.as_ref()) {
                continue;
            }
            match crate::runtime::rebuild_company(state, &id).await {
                Ok(_) => {
                    tracing::info!(company = %id, "rebuilt for the new sign-in mode");
                }
                Err(error) => {
                    tracing::warn!(
                        company = %id, %error,
                        "could not rebuild for the new sign-in mode; it needs a restart",
                    );
                    needs_restart_for_auth = true;
                }
            }
        }
    }
    // Applied live, so telling the operator to restart for it would be a lie —
    // and the restart notice is the one part of this screen they act on.
    if !needs_restart_for_auth {
        restart_required.retain(|key| key != "auth_mode");
    }

    Ok(AppliedDto {
        complete: true,
        config_path: path.display().to_string(),
        restart_required,
        seeded_company: seeded,
    })
}

/// Validates a submitted `bind` value the same way the boot path resolves one,
/// before it is ever written.
///
/// Not a bare `SocketAddr::parse`: the boot path (`server::routes::serve_on`,
/// via `TcpListener::bind`) resolves through `ToSocketAddrs`, which accepts a
/// hostname like `localhost:8080` alongside a literal IP — and a stricter
/// check here would refuse a value the host would actually have bound.
/// `tokio::net::lookup_host` is that same resolution, run without holding a
/// listener open. A value that resolves to zero addresses, or that fails to
/// resolve at all (a malformed port, most commonly — `127.0.0.1:notaport`),
/// is refused as a bad request rather than left to abort the next restart.
async fn validate_bind(addr: &str) -> Result<(), OpenCompanyError> {
    let mut resolved = tokio::net::lookup_host(addr).await.map_err(|e| {
        OpenCompanyError::InvalidRequest(format!("`bind` must be a valid address:port — {e}"))
    })?;
    if resolved.next().is_none() {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "`bind` resolved to no address at all — you sent `{addr}`."
        )));
    }
    Ok(())
}

/// Converts a submitted string into the typed value its key expects.
fn parse_value(spec: &FieldSpec, raw: Option<&str>) -> Result<ConfigValue, OpenCompanyError> {
    // `null` — and a blank string, which is what an emptied form field sends —
    // both mean "clear this". Writing `""` instead would be a set-but-empty
    // value that shadows the layer below rather than deferring to it.
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(ConfigValue::Unset);
    };
    let invalid = |what: &str| {
        OpenCompanyError::InvalidRequest(format!(
            "`{}` must be {what} — you sent `{raw}`.",
            spec.key
        ))
    };
    Ok(match spec.key {
        "workspace.clear_tmp_on_startup" => {
            ConfigValue::Bool(raw.parse().map_err(|_| invalid("true or false"))?)
        }
        "workspace.max_blob_mb" | "workspace.storage_quota_gb" | "workspace.tree_quota_gb" => {
            ConfigValue::Float(raw.parse().map_err(|_| invalid("a number"))?)
        }
        _ => ConfigValue::Str(raw.to_string()),
    })
}

#[cfg(test)]
mod test;

// ---------------------------------------------------------------------------
// The roster proposal, before any company exists
// ---------------------------------------------------------------------------

/// What `POST /api/v1/setup/roster` accepts.
///
/// The same three answers the company-scoped route takes, plus the credential
/// the operator is typing into this very wizard. The credential is **used and
/// discarded**: it is not written anywhere by this route, so the apply that
/// writes `config.toml` stays one atomic step rather than a write-then-generate
/// sequence that can half-land.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SetupRosterRequest {
    industry: String,
    team_hint: String,
    automate: String,
    /// A shipped preset chosen explicitly in the wizard.
    template: Option<String>,
    /// The inference credential from the wizard's own field, when the operator
    /// has just supplied one. Absent falls back to whatever the host already
    /// has; neither yielding one ships the curated team.
    inference_key: Option<String>,
    inference_provider: Option<String>,
    inference_base_url: Option<String>,
    inference_model: Option<String>,
}

/// One proposed teammate, shaped for the wizard's review step.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupAgentDto {
    name: String,
    role: String,
    description: String,
    /// The job shape that decides this teammate's tool belt. Sent so the review
    /// step can hand it straight back on apply — the console never shows or
    /// edits it, it only carries it.
    focus: Option<String>,
}

/// The proposal.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupRosterDto {
    agents: Vec<SetupAgentDto>,
    /// Which curated roster framed the proposal, e.g. `ecommerce`.
    template: String,
    /// `model` or `fallback` — the review step says which, in a sentence.
    source: String,
    /// The jobs the operator named, as the host split them. Echoed back so the
    /// list the roster was judged against is the list they can see — a bad split
    /// is then visible to the person who typed it.
    jobs: Vec<String>,
    /// The jobs no teammate owns. Non-empty only on the `model` path; a curated
    /// team makes no coverage claim about a list it never read.
    uncovered: Vec<String>,
    /// Why this is the curated team: `no_model`, `model_unreachable` or
    /// `not_designable`. Absent on the `model` path. The review screen needs it
    /// because the operator's next move differs — "add a key" versus "try
    /// again" versus "tell us more".
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

/// What `POST /api/v1/setup/inference/test` accepts.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InferenceTestRequest {
    /// One of `crate::company::INFERENCE_PROVIDERS`.
    provider: String,
    /// The key the operator just typed. Blank means "use whatever this host
    /// already has", which is the hosted case and the keyless-Ollama case.
    ///
    /// Used and discarded. Nothing here is written: testing a credential and
    /// committing to it are separate acts, and an operator must be able to find
    /// out a key is wrong without having already stored it.
    key: Option<String>,
    /// Endpoint override. Required for `openai_compatible`, defaulted otherwise.
    base_url: Option<String>,
}

/// What the test answers. Never carries the credential, and never the raw
/// provider error — see [`test_inference`].
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceTestDto {
    ok: bool,
    /// The endpoint that was actually reached, so a green tick is checkable.
    /// An operator who mistypes a base URL and gets a tick from the *default*
    /// endpoint has been told the wrong thing.
    base_url: String,
    /// Concrete model discovered from the endpoint catalog, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Present only on failure, in the operator's language.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `POST /api/v1/setup/inference/test` — a live one-turn probe of a credential
/// the operator has just typed, before anything is written.
///
/// The company-scoped `POST {scope}/inference/test` cannot serve the wizard for
/// the same reason the roster route could not: it resolves a `CompanyRuntime`
/// and reads that company's secret store, and during first-run setup there is
/// no company and no store. Same [`probe`](crate::harness::provider::probe)
/// underneath.
///
/// ## Why this exists as its own step
///
/// The design pass is silent about credentials by design: it falls back to a
/// curated team on any failure, so a wrong key produces a *plausible* company
/// rather than an error. That is right for the pass and wrong for setup — an
/// operator who mistypes a key would be shown a keyword-matched roster and told
/// only that a model could not be reached, several screens after the mistake.
/// One explicit test, before the questions, is where a bad credential is cheap
/// to discover.
///
/// ## The error is summarised, not forwarded
///
/// A provider's failure body can echo request material back. The probe already
/// scrubs the credential, and this narrows further to the shape of the failure —
/// enough to act on, without a wire from an upstream error message into a
/// browser on an unauthenticated first-run host.
async fn test_inference(
    State(state): State<AppState>,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    headers: HeaderMap,
    Json(req): Json<InferenceTestRequest>,
) -> Result<Json<InferenceTestDto>, crate::server::Rejection> {
    authorize(&state, &headers, peer).await?;
    Ok(Json(probe_inference(&req, &ProcessEnv).await))
}

/// Runs the probe against the resolved config.
#[cfg(feature = "openhuman")]
async fn probe_inference<E: EnvSource + Sync>(
    req: &InferenceTestRequest,
    env: &E,
) -> InferenceTestDto {
    let env_default =
        crate::harness::provider::harness_inference_from_env(env).map(|(config, _)| {
            crate::company::inference::EnvDefault {
                base_url: config.base_url,
                credential: config.credential,
            }
        });
    let normalized_base_url =
        crate::company::inference::normalize_setup_base_url(&req.provider, req.base_url.as_deref());
    let mut decl = crate::company::inference::decl_for_probe(
        &req.provider,
        normalized_base_url.as_deref(),
        req.key.as_deref(),
        env_default.as_ref(),
    );
    let model = if matches!(
        crate::company::inference::normalize_provider(&req.provider),
        "ollama" | "openai_compatible"
    ) {
        let bearer = decl.bearer().await.ok().flatten();
        crate::server::inference_models::discover_models(&decl.base_url, bearer.as_deref())
            .await
            .ok()
            .and_then(|models| models.into_iter().next())
            .map(|model| model.id)
    } else {
        None
    };
    if let Some(model) = &model {
        for tier in crate::company::INFERENCE_TIERS {
            decl.models.insert((*tier).to_string(), model.clone());
        }
    }
    let base_url = decl.base_url.clone();

    // `openai_compatible` has no default endpoint, so a blank URL resolves to
    // an empty string. Reported here rather than left to produce a confusing
    // transport error from a request to nowhere.
    if base_url.trim().is_empty() {
        return InferenceTestDto {
            ok: false,
            base_url,
            model: None,
            error: Some("This provider needs an endpoint URL.".to_string()),
        };
    }

    // No company exists yet at this step, so there is no harness to name — the
    // repair hint on failure falls back to the company-level phrasing (there is
    // no company-level config to name either, but nothing here has one to
    // offer instead).
    match crate::harness::provider::probe(&decl, None).await {
        Ok(()) => InferenceTestDto {
            ok: true,
            base_url,
            model,
            error: None,
        },
        Err(err) => {
            // Logged in full for whoever runs the host; summarised for the page.
            tracing::info!(
                provider = %req.provider,
                base_url = %base_url,
                error = %err,
                "[setup] the inference test could not reach the provider"
            );
            InferenceTestDto {
                ok: false,
                base_url,
                model,
                error: Some(summarise_probe_failure(&err.to_string())),
            }
        }
    }
}

/// Without the harness there is nothing to probe with.
#[cfg(not(feature = "openhuman"))]
async fn probe_inference<E: EnvSource + Sync>(
    req: &InferenceTestRequest,
    _env: &E,
) -> InferenceTestDto {
    InferenceTestDto {
        ok: false,
        base_url: crate::company::inference::effective_base_url(
            &req.provider,
            req.base_url.as_deref(),
        ),
        model: None,
        error: Some(
            "This build cannot reach a model — the agent harness is not compiled in.".to_string(),
        ),
    }
}

/// Turns a provider failure into one line an operator can act on.
///
/// Three outcomes are worth telling apart because each has a different fix: the
/// key is wrong, the endpoint is wrong, or the provider said no for its own
/// reasons. Everything else is reported as unreachable rather than guessed at.
#[cfg(feature = "openhuman")]
fn summarise_probe_failure(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("401") || lower.contains("unauthorized") || lower.contains("invalid api key")
    {
        "That key was rejected by the provider.".to_string()
    } else if lower.contains("403") || lower.contains("forbidden") {
        "That key was accepted but is not allowed to use this model.".to_string()
    } else if lower.contains("404") {
        "Reached the host, but there is no chat endpoint at that URL.".to_string()
    } else if lower.contains("429") || lower.contains("rate limit") {
        "The provider is rate-limiting this key right now.".to_string()
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "The provider did not answer in time.".to_string()
    } else if lower.contains("dns") || lower.contains("connect") || lower.contains("resolve") {
        "Could not reach that address.".to_string()
    } else {
        "Could not get a reply from the provider.".to_string()
    }
}

/// `POST /api/v1/setup/roster` — propose a starting team for a company that
/// does not exist yet.
///
/// The company-scoped `POST {scope}/setup/roster` cannot serve the wizard: it
/// resolves a `CompanyRuntime`, and during first-run setup there is none. Same
/// pass underneath, same validation, same fallback — only the scope differs.
///
/// Authorized by the same [`authorize`] the rest of this flow uses, so an
/// unconfigured loopback host can reach it before anyone can sign in, and a
/// configured one demands an admin.
/// The roster a bundled template declares, as review-shaped teammates.
///
/// `None` when the template carries no roster at all, which no shipped one does
/// (`every_preset_seeds_a_non_empty_roster` in `crate::desktop`) but which a
/// caller must still be able to survive rather than present an empty team.
///
/// A manifest `[[agent]]` has no display name — `Agent::name` is an in-memory
/// carrier for operator-added teammates and is `#[serde(skip)]` — so the role
/// stands in for both, which is what the console already renders for these
/// teammates once the company exists.
fn preset_roster(
    id: &str,
) -> Result<Option<Vec<crate::company::setup::ProposedAgent>>, crate::server::Rejection> {
    let Some(preset) = crate::desktop::preset(id) else {
        return Ok(None);
    };
    let manifest = preset.manifest_parsed()?;
    let agents: Vec<crate::company::setup::ProposedAgent> = manifest
        .agents
        .iter()
        .map(|agent| crate::company::setup::ProposedAgent {
            name: agent.role.clone(),
            role: agent.role.clone(),
            description: agent.description.clone().unwrap_or_default(),
            // The template's own `[tools]` decides its belt, and this roster is
            // shown rather than built from — see the apply, which seeds the
            // template itself. Claiming a focus here would be inventing one.
            focus: None,
        })
        .collect();
    Ok((!agents.is_empty()).then_some(agents))
}

async fn propose_roster(
    State(state): State<AppState>,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    headers: HeaderMap,
    Json(req): Json<SetupRosterRequest>,
) -> Result<Json<SetupRosterDto>, crate::server::Rejection> {
    authorize(&state, &headers, peer).await?;

    let template_name = req
        .template
        .as_deref()
        .map(|id| {
            crate::desktop::preset(id)
                .map(|preset| preset.name)
                .ok_or_else(|| {
                    OpenCompanyError::InvalidRequest(format!("unknown company template `{id}`"))
                })
        })
        .transpose()?;
    let answers = crate::company::setup::SetupAnswers {
        industry: match template_name {
            Some(name) if req.industry.trim().is_empty() => name.to_string(),
            Some(name) if req.industry.trim().eq_ignore_ascii_case(name) => name.to_string(),
            Some(name) => format!("{name} — {}", req.industry.trim()),
            None => req.industry,
        },
        team_hint: req.team_hint,
        automate: req.automate,
    };
    let proposal = propose_for_setup(
        &answers,
        req.inference_provider.as_deref(),
        req.inference_base_url.as_deref(),
        req.inference_key.as_deref(),
        req.inference_model.as_deref(),
    )
    .await;

    // A picked template outranks the matched one when no model designed
    // anything.
    //
    // Both are "the curated answer", and they are not the same roster.
    // `template_proposal` matches a reference team from what the operator
    // *wrote*; the template list is what they *chose*. So picking "Agentic
    // Marketing Agency" and skipping the model step returned the five-person
    // curated marketing team under a heading naming a template that ships
    // eight — a roster nobody selected, presented as the one they did.
    //
    // Only on the fallback paths. A model that designed a team read the
    // template as one input among the operator's answers and produced
    // something for *them*; replacing that with the shipped roster would throw
    // away the entire point of the design pass.
    let proposal = match (&req.template, proposal.source) {
        (Some(id), crate::company::setup::RosterSource::Fallback) => {
            match preset_roster(id)? {
                // `preset` is the same lookup `template_name` above already
                // validated, so an unknown id has been rejected before here;
                // an empty roster is the only remaining reason to keep what we
                // have, and `every_preset_seeds_a_non_empty_roster` makes that
                // unreachable for a bundled template.
                Some(agents) => crate::company::setup::preset_proposal(
                    &answers,
                    crate::desktop::preset(id)
                        .map(|preset| preset.id)
                        .unwrap_or(""),
                    agents,
                    proposal
                        .reason
                        .unwrap_or(crate::company::setup::FallbackReason::NoModel),
                ),
                None => proposal,
            }
        }
        _ => proposal,
    };

    tracing::info!(
        template = proposal.template_key,
        source = proposal.source.as_str(),
        agents = proposal.agents.len(),
        "[setup] proposed a starting roster for a company that does not exist yet"
    );

    Ok(Json(SetupRosterDto {
        agents: proposal
            .agents
            .into_iter()
            .map(|a| SetupAgentDto {
                name: a.name,
                role: a.role,
                description: a.description,
                focus: a.focus.map(|f| f.as_str().to_string()),
            })
            .collect(),
        template: proposal.template_key.to_string(),
        source: proposal.source.as_str().to_string(),
        jobs: proposal.jobs,
        uncovered: proposal.uncovered,
        reason: proposal.reason.map(|r| r.as_str()),
    }))
}

/// Designs the roster when a model is reachable, and hands back the curated
/// team when one is not.
#[cfg(feature = "openhuman")]
async fn propose_for_setup(
    answers: &crate::company::setup::SetupAnswers,
    provider: Option<&str>,
    base_url: Option<&str>,
    credential: Option<&str>,
    model: Option<&str>,
) -> crate::company::setup::RosterProposal {
    match crate::harness::roster_build::RosterBuilder::for_setup(
        &ProcessEnv,
        provider,
        base_url,
        credential,
        model,
    ) {
        // Unmetered on purpose — there is no company to charge yet. See
        // `RosterBuilder::for_setup`.
        Some(builder) => builder.propose(answers).await.0,
        // No builder means no credential was reachable at all.
        None => crate::company::setup::template_proposal(
            answers,
            crate::company::setup::FallbackReason::NoModel,
        ),
    }
}

/// The default build links no harness, so the curated team is the whole answer —
/// and it is a real one.
#[cfg(not(feature = "openhuman"))]
async fn propose_for_setup(
    answers: &crate::company::setup::SetupAnswers,
    _provider: Option<&str>,
    _base_url: Option<&str>,
    _credential: Option<&str>,
    _model: Option<&str>,
) -> crate::company::setup::RosterProposal {
    crate::company::setup::template_proposal(
        answers,
        crate::company::setup::FallbackReason::NoModel,
    )
}

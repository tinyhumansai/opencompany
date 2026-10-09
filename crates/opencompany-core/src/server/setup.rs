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
//! The call is open without a session only on a verified loopback request when
//! setup has never completed or the host has no companies. A routable empty
//! host instead requires the short-lived SSO bootstrap session for its
//! platform-designated owner; every other request needs a company admin.
//!
//! The unauthenticated cases being loopback-gated is the point. Openness on a
//! routable host would let whoever reached a fresh deployment first configure
//! it; openness on a *configured* laptop would let any page in the browser
//! rewrite its settings. The no-companies case is not a nicety either: setup can
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
        .route("/api/v1/setup/inference/probe", post(probe_inference_draft))
        .route(
            "/api/v1/setup/composio/api-key/test",
            post(test_composio_key),
        )
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
    /// The stable preset slug, e.g. `marketing_agency`.
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
    /// a host with no mail transport too, because a password signs people in
    /// there perfectly well. Read [`mail`](Self::mail) for what the magic-link
    /// path specifically can do today.
    pub auth_modes: Vec<&'static str>,
    /// The mode the wizard should preselect when `config.toml` names none.
    ///
    /// `none` on the packaged desktop app, which boots with that mode already
    /// in force as a host-wide override on a loopback bind
    /// (`crates/opencompany-app/src/embedded.rs`): one machine, one person, no
    /// mailbox, so the sign-in question is already answered and asking it
    /// again — and then asking for an address to go with the wrong answer — is
    /// the first-run confusion this field removes. Reported by the host rather
    /// than sniffed from the webview so the same console opened in a browser
    /// tab against a desktop host gets the same default. Absent everywhere
    /// else, where `email` stays the default it always was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_auth_mode: Option<&'static str>,
    /// Which optional surfaces this build has.
    pub build: BuildDto,
    /// Company ids already registered on this host. A non-empty list means the
    /// seed step should be skipped — the instance already has a company.
    pub companies: Vec<String>,
    /// What this host can already reach without the operator supplying anything.
    pub inference: InferenceReadyDto,
    /// What this host can do with a mailbox.
    pub mail: MailReadyDto,
    /// Whether this GET was authorized by the platform SSO bootstrap session —
    /// i.e. the console arrived via the dashboard's one-click SSO. When true the
    /// wizard skips the sign-in step: `apply` signs that owner straight in (see
    /// [`apply_response`]), so a password they would never use is not asked for.
    pub sso_bootstrap: bool,
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
    /// The endpoint it resolves to, with any embedded credential redacted.
    /// Seeing which endpoint a test is about to hit is the difference between a
    /// green tick and a green tick you can trust — but a URL is not
    /// automatically safe to show, because it can carry userinfo. See
    /// [`redact_endpoint`](crate::company::inference::catalogue::redact_endpoint).
    pub base_url: Option<String>,
    /// Where an operator mints a TinyHumans key by hand — the API-keys tab of
    /// the dashboard belonging to the platform **this host is on**. The wizard
    /// runs before any company exists, so the one-click grant (which the host
    /// scopes to a company) cannot; a link is what is left, and it has to be a
    /// link to the right hub. The console used to hard-code production here,
    /// so a host on staging sent its operator to mint a key staging would
    /// never accept. `None` when `api_url` follows no convention the site
    /// derivation knows (`hub_account::site_for_api`), and the wizard then
    /// shows no link rather than a guess.
    pub keys_url: Option<String>,
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
fn house_credential(env: &dyn EnvSource, api_url: &str) -> Option<String> {
    crate::harness::provider::harness_inference_from_env_at(env, Some(api_url))
        // Redacted, because "it is a URL" is not the same as "it is not a
        // secret": `OPENCOMPANY_INFERENCE_URL` can carry userinfo, and this
        // one is the deployer's own endpoint rather than a tenant's, so no
        // input rule this workload holds can have kept it out.
        .map(|(config, _)| crate::company::inference::catalogue::redact_endpoint(&config.base_url))
}

/// Without the harness there is no inference path at all, so the host holds
/// nothing and the step asks for everything.
#[cfg(not(feature = "openhuman"))]
fn house_credential(_env: &dyn EnvSource, _api_url: &str) -> Option<String> {
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
    /// The first admin's password, set on the account the moment the company
    /// exists.
    ///
    /// Without it the wizard finished into a company whose only admin was
    /// *eligible* and could not get in: the magic link needs a mail transport
    /// the laptop it was just run on rarely has, and the fallback — the host
    /// echoing the code back into the wizard — was a link the operator did not
    /// know to expect. A password chosen (or generated) on the "You" step is a
    /// credential the wizard can sign them in with the instant setup applies,
    /// and the same one they use tomorrow.
    ///
    /// Applies to whichever admin address the apply seeds — the template
    /// path's [`Self::admin_email`] or the designed company's own. Ignored when
    /// the host has no sign-in. Write-only, like every other secret here.
    pub admin_password: Option<String>,
    /// A company the wizard **designed**, from the operator's answers and the
    /// roster they reviewed.
    ///
    /// Takes precedence over [`Self::template`] when both are present: an
    /// operator who answered three questions and edited a roster has expressed
    /// a preference that a template slug cannot override. The template path
    /// stays for `desktop::bootstrap_companies` and for any caller that still
    /// wants a preset.
    pub company: Option<SetupCompany>,
    /// The TinyHumans account key the wizard's managed branch collected, to be
    /// stored against the company this call seeds.
    ///
    /// Write-only, and never a `config.toml` write: this is the **company's**
    /// credential ([`company_key::KEY_KEY`](crate::company::company_key::KEY_KEY)),
    /// the one the Connections Account page sets, not the instance-wide
    /// `tinyhumans_api_key` field. Sent here rather than written by the console
    /// itself because `PUT …/credential` is admin-scoped to an existing
    /// company, and during first run there is neither: the company is created
    /// by this very call, and nobody has signed in yet to be its admin.
    ///
    /// Applies to both seed paths — a designed company and a seeded template —
    /// which is why it sits at the top level rather than inside
    /// [`SetupCompany`]: an operator who took the managed branch and kept an
    /// untouched preset roster is sent back as a template slug, with no
    /// designed company to carry anything.
    pub tinyhumans_key: Option<String>,
    /// The model to finish the `tinyhumans` row with, as the setup probe
    /// discovered it.
    ///
    /// The fan-out creates no row without one, so a probe that named no model
    /// leaves the row unmade and says so through
    /// [`AppliedDto::credential_note`] rather than silently reporting success.
    pub tinyhumans_model: Option<String>,
    /// The provider the wizard's self-managed branch connected, to be added to
    /// the company this call seeds.
    ///
    /// The **same body** `POST …/inference/providers` accepts, deserialized by
    /// the same type and applied by the same function
    /// ([`add_provider_inner`](crate::server::ops::inference::providers::add_provider_inner)).
    /// Deliberately not a shape of its own: the add carries a slot guard, a
    /// first-provider default, a credential-then-record rollback pair and a
    /// probe-class rollback, and a wizard-only flush would have reproduced the
    /// row without any of them.
    ///
    /// Sent here rather than written by the console because that route is
    /// admin-scoped to an existing company, and first run has neither — the
    /// same reason [`Self::tinyhumans_key`] travels this way.
    ///
    /// Top level for the same reason too: it belongs to whichever company comes
    /// out of the seed, designed or templated.
    pub(crate) provider_draft: Option<crate::server::ops::inference::providers::AddProvider>,
    /// The Composio credential the wizard's self-managed branch collected.
    ///
    /// Two shapes, one field, because the Connections dialog is one form with
    /// two routes: a company's own Composio API key (which also selects the
    /// BYOK mode), or a token for the TinyHumans-managed route.
    ///
    /// Travels on the apply for the same reason the others do — both writes are
    /// per company, and the company is what this call creates.
    pub composio_draft: Option<ComposioDraft>,
}

/// The Composio credential the wizard collected, and which of the two it is.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposioDraft {
    /// Which write this performs — the same two values the console's
    /// `ComposioForm.credential` carries.
    pub credential: ComposioCredential,
    /// The secret. Write-only: no route returns it.
    pub value: String,
}

/// Which Composio credential a draft is.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum ComposioCredential {
    /// This company's own Composio account key, which also selects BYOK.
    #[serde(rename = "composio-api-key")]
    ApiKey,
    /// A token for the TinyHumans-managed route.
    #[serde(rename = "composio-token")]
    Token,
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
    /// What the account-key fan-out actually did, in the host's own words
    /// ([`fan_out_note`](crate::company::company_key::fan_out_note)) — the same
    /// sentence the Account page's save toast carries.
    ///
    /// `None` when no key was sent or no company was seeded. Reported rather
    /// than assumed because the fan-out honestly skips slots it must not
    /// touch, and a model it was never given leaves the `tinyhumans` row
    /// unmade: "you're set up" alone would paper over both.
    pub credential_note: Option<String>,
    /// What connecting the self-managed branch's provider did, in the host's
    /// own words — the same sentence the LLM page's add toast carries.
    ///
    /// `None` when no provider was drafted or no company was seeded. It also
    /// carries the **refusal** when the add was refused: the company is built
    /// by the time this runs, and an endpoint that stopped answering between
    /// the wizard's probe and the apply is a reason to say so, not a reason to
    /// fail a setup that otherwise succeeded.
    pub provider_note: Option<String>,
    /// What the Composio credential the wizard collected did.
    ///
    /// `None` when none was sent or no company was seeded.
    pub composio_note: Option<String>,
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
/// A verified loopback request is open without a session in exactly two
/// situations, both of which are "there is nobody who could authorize it":
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
/// On a routable empty host, the platform's short-lived SSO bootstrap session
/// authorizes only the owner named by the per-tenant token. Otherwise the
/// ordinary admin check applies, resolved through the sole
/// company: setup is host-level but authority is per company, and a host
/// serving several has no single roster that could speak for the instance.
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> Result<(), crate::server::Rejection> {
    if state.registry().is_empty()
        && let Some((company, token)) = crate::server::users::cookie::session_from_header(headers)
        && crate::server::sso::bootstrap_session_is_valid(state, &company, &token).await?
    {
        return Ok(());
    }
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
    // Surface whether this request is on an SSO bootstrap session so the wizard
    // can skip the sign-in step (the owner is signed in by `apply`, not a
    // password). Read before the snapshot so the flag rides out with it.
    let sso_bootstrap = bootstrap_setup_subject(&state, &headers).await?.is_some();
    let mut dto = snapshot(&state, &ProcessEnv)?;
    dto.sso_bootstrap = sso_bootstrap;
    Ok(Json(dto))
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
        // `read` overrides this when the request carries a valid SSO bootstrap
        // session; `snapshot` has no request headers to tell on its own.
        sso_bootstrap: false,
        complete: state.setup_complete(),
        config_path: dir
            .join(crate::app::config::CONFIG_FILE)
            .display()
            .to_string(),
        fields,
        templates: templates(),
        auth_modes: auth_modes(state),
        default_auth_mode: default_auth_mode(state),
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
            let base_url = house_credential(env, &state.config().api_url);
            InferenceReadyDto {
                ready: base_url.is_some(),
                provider: base_url.is_some().then_some("managed"),
                base_url,
                keys_url: state
                    .config()
                    .hub_site()
                    .map(|site| crate::server::hub_account::manage_keys_url(&site)),
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

/// The mode a fresh wizard preselects — see [`SetupDto::default_auth_mode`].
///
/// `none` only where it is both in force and legal: the live override says
/// this host already runs without a sign-in, and the bind is loopback so
/// `auth_modes` offers it. A routable host with the override set could not
/// have booted (`is_local_only` gates it), so the second check is belt and
/// braces against a config nobody should be able to reach.
fn default_auth_mode(state: &AppState) -> Option<&'static str> {
    (state.auth_mode_override() == Some(AuthMode::None) && state.config().is_local_only())
        .then_some(AuthMode::None.as_str())
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
) -> Result<axum::response::Response, crate::server::Rejection> {
    authorize(&state, &headers, peer).await?;
    // Capture the SSO bootstrap owner BEFORE seeding: a bootstrap session only
    // validates on an empty host, so once `apply_inner` registers the company it
    // stops validating — this is the one chance to read who it proved.
    let bootstrap_owner = bootstrap_setup_subject(&state, &headers).await?;
    let applied = apply_inner(&state, req, &ProcessEnv).await?;
    apply_response(&state, &headers, bootstrap_owner, applied).await
}

/// The SSO-bootstrap owner this request proves, or `None` when it did not arrive
/// on a valid bootstrap session.
///
/// Read BEFORE seeding: [`crate::server::sso::bootstrap_session_is_valid`]
/// requires an empty registry, so a bootstrap session stops validating the
/// moment setup seeds a company. A valid one proves the per-tenant token's
/// subject is the deployment's bootstrap admin, so that is who setup signs in.
async fn bootstrap_setup_subject(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<String>, crate::server::Rejection> {
    if !state.registry().is_empty() {
        return Ok(None);
    }
    let Some((company, token)) = crate::server::users::cookie::session_from_header(headers) else {
        return Ok(None);
    };
    if !crate::server::sso::bootstrap_session_is_valid(state, &company, &token).await? {
        return Ok(None);
    }
    Ok(state
        .config()
        .bootstrap_admin()
        .map(|admin| crate::ports::users::normalize_email(&admin)))
}

/// Builds the apply response, signing the SSO-bootstrap owner straight into the
/// company setup just seeded so the console lands authenticated — no second
/// sign-in with a password they never needed.
///
/// Falls through to the plain `AppliedDto` when there is no bootstrap owner, no
/// company was seeded, or the owner is not a standing admin of the seeded
/// company (the same eligibility gate the SSO redeem applies, so setup cannot
/// mint a session for an address the company would not itself admit). The owner
/// is signed in by a `Set-Cookie`: the SSO bootstrap link only ever lands in the
/// host's own same-origin console, so the cookie is the carrier and there is no
/// cross-origin setup apply to hand a body session to (see
/// [`crate::server::users::routes::mint_session_cookie`]).
async fn apply_response(
    state: &AppState,
    headers: &HeaderMap,
    bootstrap_owner: Option<String>,
    applied: AppliedDto,
) -> Result<axum::response::Response, crate::server::Rejection> {
    let Some(owner) = bootstrap_owner else {
        return Ok(Json(applied).into_response());
    };
    let Some(company) = applied.seeded_company.clone() else {
        return Ok(Json(applied).into_response());
    };
    let Some(runtime) = state
        .registry()
        .get(&crate::ports::types::CompanyId::new(company.as_str()))
    else {
        return Ok(Json(applied).into_response());
    };
    let standing = crate::server::users::routes::bootstrap_admins(state.config(), &runtime).await?;
    if !standing.iter().any(|admin| admin == &owner) {
        return Ok(Json(applied).into_response());
    }
    let now = crate::ports::now_millis();
    let user = crate::server::users::routes::upsert_from_eligibility(
        &runtime,
        &owner,
        crate::ports::users::UserRole::Admin,
        now,
    )
    .await?;
    let set =
        crate::server::users::routes::mint_session_cookie(state, &runtime, &user, headers).await?;
    Ok(([(axum::http::header::SET_COOKIE, set)], Json(applied)).into_response())
}

/// Serializes the whole first-run apply, process-wide.
///
/// Without this, two `POST /api/v1/setup` requests that both land before
/// either has inserted into the registry can both read
/// `state.registry().is_empty()` as `true` and both seed a starter company —
/// exactly the "a re-run must never hand the operator a second starter
/// company" case this module documents, just reached by two concurrent
/// first runs instead of one re-run. Held for the whole of [`apply_inner`],
/// not just the seed check, so a second caller only ever starts once the
/// first has fully landed (or failed) — including the `config.toml` write,
/// which is not otherwise safe against a torn concurrent write either.
static APPLY_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// `+ Sync` so the returned future is `Send`, which axum requires of a handler:
/// a `&T` is `Send` only when `T` is `Sync`, and the bare trait object is not.
async fn apply_inner(
    state: &AppState,
    req: SetupRequest,
    env: &(dyn EnvSource + Sync),
) -> Result<AppliedDto, OpenCompanyError> {
    let _apply_guard = APPLY_LOCK.lock().await;
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

    // The admin address the apply is about to make eligible, on either seed
    // path, and the password that makes it usable. Validated here, before
    // anything is written, for the same reason everything above is: a company
    // seeded and a password refused afterwards is a half-applied setup.
    let admin_email: Option<String> = req
        .company
        .as_ref()
        .and_then(|company| company.admin_email.as_deref())
        .or(req.admin_email.as_deref())
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(crate::ports::normalize_email);
    let admin_password = req
        .admin_password
        .as_deref()
        .filter(|password| !password.is_empty());
    if let (Some(email), Some(password)) = (admin_email.as_deref(), admin_password) {
        crate::server::users::password::validate(password, email)?;
    }

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

    // Make the admin real, not merely eligible. Seeding wrote the address into
    // `[users].admins`, which is a standing invite; this turns it into an
    // account with a password, so the console can sign the operator in with
    // what they just typed rather than handing them a form they cannot pass.
    // Skipped on a host with no sign-in — there is nobody to distinguish — and
    // where no password was given, which keeps the older link hand-off working.
    if let (Some(id), Some(email), Some(password)) =
        (seeded.as_deref(), admin_email.as_deref(), admin_password)
        && let Some(runtime) = state
            .registry()
            .get(&crate::ports::types::CompanyId::new(id))
        && runtime.auth_mode().uses_email()
    {
        let standing =
            crate::server::users::routes::bootstrap_admins(state.config(), &runtime).await?;
        match crate::server::users::bootstrap::claim_first_admin(
            runtime.users(),
            runtime.id(),
            &standing,
            email,
            password,
        )
        .await?
        {
            Ok(_) => tracing::info!(company = %runtime.id(), "first admin created by setup"),
            // A company the wizard just seeded has no users, and the address
            // was written into its manifest a moment ago, so neither refusal
            // can happen on this path; if it somehow does, the standing invite
            // is still there and the sign-in screen's own claim takes over.
            Err(refusal) => tracing::warn!(
                company = %runtime.id(),
                ?refusal,
                "setup could not create the first admin; the address stays eligible",
            ),
        }
    }

    let credential_note = store_account_key(state, seeded.as_deref(), &req).await?;
    let provider_note = connect_drafted_provider(state, seeded.as_deref(), &req).await?;
    let composio_note = store_composio_credential(state, seeded.as_deref(), &req).await?;

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
        credential_note,
        provider_note,
        composio_note,
    })
}

/// Stores the Composio credential the self-managed branch collected against the
/// seeded company.
///
/// [`store_api_key`](crate::company::composio::store_api_key) and
/// [`store_token`](crate::company::composio::store_token) are the same two
/// functions `PUT …/composio/api-key` and `PUT …/composio/token` call, and they
/// take a company id and a secret store rather than a request, so they are
/// reached directly rather than through a seam.
///
/// The routes' extra machinery is all about a **transition**: the account-key
/// slot guard, the `load_mode` re-read and its `Conflict`, the `switching` /
/// `used_by` / `confirm_in_use` warning about providers stranded in the account
/// this company is leaving. A company created milliseconds ago has no previous
/// mode, no connected providers and no concurrent admin — and `apply_inner`
/// holds `APPLY_LOCK` — so there is no transition for any of it to describe.
///
/// [`evict_catalog_cache`](crate::server::ops::composio::evict_catalog_cache)
/// runs anyway. There is nothing cached for a company this new, and a cache
/// drop that costs nothing is not worth reasoning about being right.
///
/// Deliberately **not** journalled, the same call 4a made for the account key:
/// the journal attributes a credential change to the admin who made it, and a
/// first run has no signed-in admin to name. The apply is what records it.
///
/// A blank value is dropped rather than written.
/// [`store_api_key`](crate::company::composio::store_api_key) reads an empty
/// key as "clear this company back to managed", which on a company that was
/// never on BYOK is a mode write nobody asked for.
async fn store_composio_credential(
    state: &AppState,
    seeded: Option<&str>,
    req: &SetupRequest,
) -> Result<Option<String>, OpenCompanyError> {
    let Some(draft) = req.composio_draft.as_ref() else {
        return Ok(None);
    };
    let value = draft.value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    // Only ever onto a company this call created — same rule as the account key
    // and the provider draft, for the same reason.
    let Some(id) = seeded.map(crate::ports::types::CompanyId::new) else {
        return Ok(None);
    };
    let Some(runtime) = state.registry().get(&id) else {
        return Ok(None);
    };
    let secrets = runtime.secrets();

    let note = match draft.credential {
        ComposioCredential::ApiKey => {
            match crate::company::composio::store_api_key(&id, secrets.as_ref(), value).await {
                Ok(_) => {
                    "This company reaches its tools through its own Composio account.".to_string()
                }
                Err(err) => format!("The Composio credential could not be stored: {err}"),
            }
        }
        ComposioCredential::Token => {
            match crate::company::composio::store_token(&id, secrets.as_ref(), value).await {
                Ok(()) => {
                    "A Composio token is stored for the TinyHumans-managed route.".to_string()
                }
                Err(err) => format!("The Composio credential could not be stored: {err}"),
            }
        }
    };
    crate::server::ops::composio::evict_catalog_cache(runtime.as_ref());
    Ok(Some(note))
}

/// Adds the provider the self-managed branch connected to the seeded company,
/// through the same function `POST …/inference/providers` runs.
///
/// Reuse, not a parallel path. Writing the row here with `store::put_provider`
/// and a secret set would look like the same outcome and would not be: the add
/// carries the `tinyhumans` slot guard, decision X1's first-provider default
/// (with its re-validation under the index lock), the model check, the
/// credential-then-record rollback pair, the probe-class rollback, and the
/// sole-provider auto-route. Every one of those is the difference between a
/// company whose provider answers and a row that merely exists.
///
/// The company has already been seeded by the time this runs, so every
/// failure — a refusal or a store that cannot be written — is reported
/// through the returned note rather than raised.
async fn connect_drafted_provider(
    state: &AppState,
    seeded: Option<&str>,
    req: &SetupRequest,
) -> Result<Option<String>, OpenCompanyError> {
    let Some(draft) = req.provider_draft.clone() else {
        return Ok(None);
    };
    let Some(id) = seeded.map(crate::ports::types::CompanyId::new) else {
        return Ok(None);
    };
    let Some(runtime) = state.registry().get(&id) else {
        return Ok(None);
    };

    match crate::server::ops::inference::providers::add_provider_inner(
        state,
        runtime.as_ref(),
        draft,
    )
    .await
    {
        Ok(mutation) => {
            rebuild_after_provider(state, &runtime).await;
            Ok(Some(mutation.note))
        }
        Err(ApiError(OpenCompanyError::InvalidRequest(message))) => Ok(Some(message)),
        Err(ApiError(err)) => Ok(Some(format!("The provider could not be connected: {err}"))),
    }
}

/// Swaps the seeded company's echo brain for the one its new provider affords.
async fn rebuild_after_provider(
    state: &AppState,
    runtime: &std::sync::Arc<crate::company::runtime::CompanyRuntime>,
) {
    if !crate::server::ops::company_key::restart_required_for(runtime.as_ref()).await {
        return;
    }
    if let Err(err) = crate::runtime::rebuild_company(state, runtime.id()).await {
        tracing::warn!(
            company = %runtime.id(),
            error = %err,
            "provider connected but the runtime could not be rebuilt; a restart is still required",
        );
    }
}

/// Stores the wizard's TinyHumans key as the seeded company's own credential,
/// through the same fan-out `PUT …/credential` runs.
///
/// Reuse, not a parallel path: [`fan_out_and_evict`] and [`rebuild_if_pending`]
/// are the two halves of what the Account page's save does either side of its
/// journal line, so the wizard's key lands in every slot that page's key lands
/// in — the Composio copy, the LLM copy, the `tinyhumans` row, the default —
/// and the company that just booted without inference is rebuilt onto it
/// instead of being left on the echo brain behind a "restart required" notice.
///
/// Deliberately **not** journalled. The journal attributes a credential change
/// to the admin who made it, and a first run has no signed-in admin to name —
/// the apply itself is what records that this happened. A rebuild failure is
/// already logged by [`rebuild_if_pending`] and leaves the honest
/// restart-required state behind.
async fn store_account_key(
    state: &AppState,
    seeded: Option<&str>,
    req: &SetupRequest,
) -> Result<Option<String>, OpenCompanyError> {
    let Some(key) = req
        .tinyhumans_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    else {
        return Ok(None);
    };
    // Only ever onto a company this call created. A key with nowhere to go is
    // dropped rather than guessed at: on a host that already had companies
    // there is no one of them this wizard can claim the operator meant.
    let Some(id) = seeded.map(crate::ports::types::CompanyId::new) else {
        return Ok(None);
    };
    let Some(runtime) = state.registry().get(&id) else {
        return Ok(None);
    };

    let model = req
        .tinyhumans_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let report = crate::server::ops::company_key::fan_out_and_evict(
        state,
        runtime.as_ref(),
        crate::company::company_key::FanOutKey::Explicit(key),
        model,
        false,
    )
    .await?;
    crate::server::ops::company_key::rebuild_if_pending(state, &runtime, &report).await;
    Ok(Some(crate::company::company_key::fan_out_note(
        false, &report, model,
    )))
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
#[path = "setup/setup_test_group_1.rs"]
mod setup_test_group_1;
#[cfg(test)]
#[path = "setup/setup_test_group_2.rs"]
mod setup_test_group_2;
#[cfg(test)]
#[path = "setup/setup_test_group_3.rs"]
mod setup_test_group_3;
#[cfg(test)]
#[path = "setup/setup_test_group_4.rs"]
mod setup_test_group_4;
#[cfg(test)]
#[path = "setup/setup_test_group_5.rs"]
mod setup_test_group_5;
#[cfg(test)]
#[path = "setup/setup_test_group_6.rs"]
mod setup_test_group_6;
#[cfg(test)]
#[path = "setup/setup_test_group_7.rs"]
mod setup_test_group_7;
#[cfg(test)]
#[path = "setup/setup_test_group_8.rs"]
mod setup_test_group_8;
#[cfg(test)]
#[path = "setup/setup_test_support_1.rs"]
mod setup_test_support_1;

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
    /// The operator answered the model step with "no model", and means it.
    ///
    /// Distinct from *sending no credential*, which this route reads as "use
    /// whatever the host already has" — and a host usually has something:
    /// `RosterBuilder::for_setup` falls through to `harness_inference_from_env`,
    /// so a hosted tenant with an injected credential would design a roster
    /// with a model the operator had just declined. The screen promises a
    /// standard team for their industry; this is what makes that true rather
    /// than true-unless-the-host-happens-to-have-a-key.
    force_curated: bool,
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
    /// Why this is the curated team: `no_model`, `model_unreachable`,
    /// `not_designable` or `output_budget_exhausted`. Absent on the `model` path. The review screen needs it
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

#[cfg(feature = "openhuman")]
const MODEL_DISCOVERY_FAILURE: &str = "Could not list models from this provider.";

#[cfg(feature = "openhuman")]
const MODEL_PROBE_CANDIDATE_LIMIT: usize = 5;

/// The model a first company thinks with when its provider offers it.
///
/// Without this the default is whichever model the provider happens to list
/// first, which is a position in someone else's catalogue rather than a choice.
#[cfg(feature = "openhuman")]
const PREFERRED_SETUP_MODEL: &str = "deepseek/deepseek-v4-flash";

#[cfg(feature = "openhuman")]
fn probe_model_candidates(
    mut models: Vec<crate::server::inference_models::InferenceModel>,
) -> Vec<crate::server::inference_models::InferenceModel> {
    models.sort_by_key(|model| {
        let id = model.id.to_ascii_lowercase();
        let unusable = ["embed", "rerank", "moderation"]
            .iter()
            .any(|marker| id.contains(marker));
        let preferred = id == PREFERRED_SETUP_MODEL;
        (unusable, !preferred)
    });
    models
        .into_iter()
        .take(MODEL_PROBE_CANDIDATE_LIMIT)
        .collect()
}

/// `POST /api/v1/setup/inference/test` — a live probe of a credential the
/// operator has just typed, before anything is written.
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
    Ok(Json(
        probe_inference(&req, &ProcessEnv, &state.config().api_url).await,
    ))
}

/// `POST /api/v1/setup/inference/probe` — read a drafted endpoint's model
/// catalogue before there is a company to store it against.
///
/// The company-scoped `POST {scope}/inference/probe` is the same probe behind
/// an `AdminScopedCompany`, and first run has neither a company nor an admin —
/// so the wizard's self-managed branch reaches
/// [`probe_draft_inner`](crate::server::ops::inference::providers::probe_draft_inner)
/// through this gate instead. The probe itself is the same function, not a
/// second implementation of it.
///
/// ## What this widens, and what it does not
///
/// It puts one more outward dial behind [`authorize`]'s first-run gate. The
/// one already there is [`test_inference`], which takes the same
/// `{provider, key, baseUrl}`, applies the same `endpoint_has_credentials`
/// refusal, and dials the same address on the same terms — so this adds
/// another *caller* of a primitive this surface already exposes rather than a
/// new kind of exposure. Both are loopback-bound, both require a genuinely
/// local peer with no proxy-forwarding header, and both are reachable only
/// while setup is incomplete or the registry is empty.
///
/// Its own refusals are unchanged and unconditional: a URL carrying userinfo
/// is refused before the request, and `probe::check_endpoint` screens the URL
/// and every redirect target inside the probe.
///
/// ## Why the wizard needs it rather than reusing the test
///
/// `test_inference` answers with one `model`; this step has to *offer* the
/// endpoint's list, which is what the model step is. And its env-default
/// fallback means a blank key silently probes the **host's** own credential —
/// right for "does this host reach a model", wrong for "does the key I just
/// typed work".
async fn probe_inference_draft(
    State(state): State<AppState>,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    headers: HeaderMap,
    Json(body): Json<crate::server::ops::inference::providers::ProbeDraft>,
) -> Result<axum::response::Response, crate::server::Rejection> {
    authorize(&state, &headers, peer).await?;
    Ok(crate::server::ops::inference::providers::probe_draft_inner("setup", body).await)
}

/// What `POST /api/v1/setup/composio/api-key/test` is asked.
///
/// The company-scoped route takes **no body** on purpose: it reads the key from
/// the company's own store, and a body carrying one would turn it into "send
/// this credential to that host". This one has to take the key, because there
/// is no company to read it from — and it is the same primitive all the same,
/// because the destination is not in the body either way:
/// `composio_direct::probe_api_key` dials Composio's own compile-time URL.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposioKeyTestRequest {
    api_key: String,
}

/// `POST /api/v1/setup/composio/api-key/test` — check a Composio API key the
/// operator has just typed, before there is a company to store it against.
///
/// Its own SSRF footing is the reason this one is cheap to add: the endpoint is
/// fixed at compile time, so no caller — authenticated or not — can point it
/// anywhere. All this route can do is spend an outbound request on a key the
/// caller supplied and report one of three fixed sentences.
///
/// Without it an operator types a wrong Composio key in onboarding and learns
/// nothing until they open Connections and find an empty tool belt — which is
/// the same objection that rules out skipping the inference probe.
async fn test_composio_key(
    State(state): State<AppState>,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    headers: HeaderMap,
    Json(req): Json<ComposioKeyTestRequest>,
) -> Result<Json<crate::server::ops::composio::ApiKeyTestDto>, crate::server::Rejection> {
    authorize(&state, &headers, peer).await?;
    let key = req.api_key.trim();
    if key.is_empty() {
        return Err(ApiError::from(OpenCompanyError::InvalidRequest(
            "Paste a Composio API key to check it.".to_string(),
        ))
        .into());
    }
    Ok(Json(
        match crate::server::ops::composio::classify_key("setup", key).await {
            None => crate::server::ops::composio::ApiKeyTestDto {
                ok: true,
                probe_class: None,
                message: None,
            },
            // `describe_verdict`, not `describe`: this route stored nothing,
            // and the latter's copy opens by saying it did.
            Some(class) => crate::server::ops::composio::ApiKeyTestDto {
                ok: false,
                probe_class: Some(class),
                message: Some(crate::company::composio_probe::describe_verdict(class).to_string()),
            },
        },
    ))
}

/// Runs the probe against the resolved config.
#[cfg(feature = "openhuman")]
async fn probe_inference<E: EnvSource + Sync>(
    req: &InferenceTestRequest,
    env: &E,
    api_url: &str,
) -> InferenceTestDto {
    // The endpoint follows the host's `api_url` with or without an instance
    // credential — the same default the runtime builder attaches — so a key
    // typed into the wizard on a staging host is probed against staging.
    let (config, _) = crate::harness::provider::platform_inference_default_at(env, Some(api_url));
    let env_default = Some(crate::company::inference::EnvDefault {
        base_url: config.base_url,
        credential: config.credential,
    });
    let normalized_base_url =
        crate::company::inference::normalize_setup_base_url(&req.provider, req.base_url.as_deref());
    // **Refused before anything is sent.** The catalogue read below and the
    // probe after it would both put a URL's userinfo on the wire as basic auth,
    // this response echoes the endpoint to the browser, and a failure is
    // logged — so a credential typed into the URL would reach all three before
    // the setup apply's own validation could refuse it (Codex review on #2281).
    if let Some(typed) = normalized_base_url.as_deref()
        && crate::company::inference::catalogue::endpoint_has_credentials(typed)
    {
        return InferenceTestDto {
            ok: false,
            base_url: crate::company::inference::catalogue::redact_endpoint(typed),
            model: None,
            error: Some(
                crate::company::inference::catalogue::ENDPOINT_CREDENTIAL_REFUSAL.to_string(),
            ),
        };
    }
    let decl = crate::company::inference::decl_for_probe(
        &req.provider,
        normalized_base_url.as_deref(),
        req.key.as_deref(),
        env_default.as_ref(),
    );
    // What is *said* about the endpoint — in this response and in the log below.
    // Redacted, because an endpoint that reached here without being typed (an
    // `OPENCOMPANY_INFERENCE_URL` this host does not own) can still carry
    // userinfo. The probe itself goes to `decl.base_url`, untouched.
    let base_url = crate::company::inference::catalogue::redact_endpoint(&decl.base_url);

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

    let bearer = match decl.bearer().await {
        Ok(bearer) => bearer,
        Err(error) => {
            tracing::info!(
                provider = %req.provider,
                base_url = %base_url,
                error = %error,
                "[setup] the inference test could not list provider models"
            );
            return InferenceTestDto {
                ok: false,
                base_url,
                model: None,
                error: Some(MODEL_DISCOVERY_FAILURE.to_string()),
            };
        }
    };
    let auth = crate::company::inference::catalogue::auth_style_for(&req.provider);
    let shape =
        crate::company::inference::catalogue::catalog_shape_for(&req.provider, &decl.base_url);
    let models = match crate::server::inference_models::discover_models(
        &decl.base_url,
        bearer.as_deref(),
        auth,
        shape,
    )
    .await
    {
        Ok(models) => models,
        Err(error) => {
            tracing::info!(
                provider = %req.provider,
                base_url = %base_url,
                error = %error,
                "[setup] the inference test could not list provider models"
            );
            let message = match error.credential_status() {
                Some(401) => "That key was rejected by the provider.",
                Some(403) => "That key was accepted but is not allowed to list models.",
                _ => MODEL_DISCOVERY_FAILURE,
            };
            return InferenceTestDto {
                ok: false,
                base_url,
                model: None,
                error: Some(message.to_string()),
            };
        }
    };
    if models.is_empty() {
        return InferenceTestDto {
            ok: false,
            base_url,
            model: None,
            error: Some(MODEL_DISCOVERY_FAILURE.to_string()),
        };
    }

    let candidates = probe_model_candidates(models);

    // TinyHumans owns both proxy surfaces. Their model catalog is authenticated,
    // so a successful read already proves the key; a second chat request proves
    // only that the account has credit and consumes provider capacity. Let setup
    // accept the credential here. The first real turn will then report an
    // insufficient balance (or a genuine runtime rate limit) in its proper place.
    if matches!(req.provider.as_str(), "managed" | "tinyhumans") {
        return InferenceTestDto {
            ok: true,
            base_url,
            model: candidates.first().map(|model| model.id.clone()),
            error: None,
        };
    }

    let mut last_failure = None;
    for model in candidates.into_iter().map(|model| model.id) {
        let candidate = decl.clone().with_chosen_model(model.clone());
        match crate::harness::provider::probe(&candidate, &model, None).await {
            Ok(()) => {
                return InferenceTestDto {
                    ok: true,
                    base_url,
                    model: Some(model),
                    error: None,
                };
            }
            Err(err) => {
                tracing::info!(
                    provider = %req.provider,
                    base_url = %base_url,
                    model = %model,
                    error = %err,
                    "[setup] the inference test could not reach the provider"
                );
                let result = InferenceTestDto {
                    ok: false,
                    base_url: base_url.clone(),
                    model: Some(model),
                    error: Some(summarise_probe_failure(&err)),
                };
                if !probe_failure_may_be_model_specific(&err) {
                    return result;
                }
                last_failure = Some(result);
            }
        }
    }
    last_failure.expect("a non-empty model catalog produced at least one probe result")
}

/// Without the harness there is nothing to probe with.
#[cfg(not(feature = "openhuman"))]
async fn probe_inference<E: EnvSource + Sync>(
    req: &InferenceTestRequest,
    _env: &E,
    _api_url: &str,
) -> InferenceTestDto {
    InferenceTestDto {
        ok: false,
        base_url: crate::company::inference::catalogue::redact_endpoint(
            &crate::company::inference::effective_base_url(&req.provider, req.base_url.as_deref()),
        ),
        model: None,
        error: Some(
            "This build cannot reach a model — the agent harness is not compiled in.".to_string(),
        ),
    }
}

/// Turns a provider failure into one line an operator can act on.
///
/// Typed provider statuses and configuration errors each keep their own action.
/// Only untyped transport failures fall back to inspecting rendered text.
#[cfg(feature = "openhuman")]
fn summarise_probe_failure(err: &anyhow::Error) -> String {
    if let Some(error) = err.downcast_ref::<tinyinference::Error>() {
        let message = match error {
            tinyinference::Error::Provider(error) => match error.status {
                Some(401) => "That key was rejected by the provider.",
                Some(403) => "That key was accepted but is not allowed to use this model.",
                Some(404) if crate::harness::provider::is_model_unavailable_failure(error) => {
                    "That model is not available from this provider for your account."
                }
                Some(404) => "Reached the host, but there is no chat endpoint at that URL.",
                Some(429) => "The provider is rate-limiting this key right now.",
                _ => "Could not get a reply from the provider.",
            },
            tinyinference::Error::Model(_) | tinyinference::Error::Validation(_) => {
                "The model configuration for this connection is invalid."
            }
            _ => "Could not get a reply from the provider.",
        };
        return message.to_string();
    }

    let lower = err.to_string().to_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        "The provider did not answer in time.".to_string()
    } else if lower.contains("dns") || lower.contains("connect") || lower.contains("resolve") {
        "Could not reach that address.".to_string()
    } else {
        "Could not get a reply from the provider.".to_string()
    }
}

#[cfg(feature = "openhuman")]
fn probe_failure_may_be_model_specific(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<tinyinference::Error>() {
        Some(tinyinference::Error::Model(_)) => true,
        Some(tinyinference::Error::Provider(error)) => {
            matches!(error.status, Some(400 | 403 | 404 | 422))
        }
        _ => false,
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
    let proposal = if req.force_curated {
        // Asked for and answered: no model runs, whatever this host holds.
        crate::company::setup::template_proposal(
            &answers,
            crate::company::setup::FallbackReason::NoModel,
        )
    } else {
        propose_for_setup(
            &answers,
            &state.config().api_url,
            req.inference_provider.as_deref(),
            req.inference_base_url.as_deref(),
            req.inference_key.as_deref(),
            req.inference_model.as_deref(),
        )
        .await
    };

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
    api_url: &str,
    provider: Option<&str>,
    base_url: Option<&str>,
    credential: Option<&str>,
    model: Option<&str>,
) -> crate::company::setup::RosterProposal {
    match crate::harness::roster_build::RosterBuilder::for_setup(
        &ProcessEnv,
        Some(api_url),
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
    _api_url: &str,
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

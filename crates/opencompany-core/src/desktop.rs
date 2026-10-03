//! The local runtime behind the packaged OpenCompany desktop application.
//!
//! The desktop app is a thin Tauri shell around the existing React console and
//! Axum operator API.  It binds only loopback, persists under the app-data
//! directory supplied by Tauri, and starts from one of the shipped company
//! presets.  There is intentionally no OpenHuman local-AI preset bridge here:
//! OpenCompany owns its local company store and its company presets.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Serialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::company::CompanyManifest;
use crate::runtime::{RuntimeBuilder, company_id_from_name};
use crate::server::cors::CorsConfig;
use crate::{AppConfig, AppState, Result};

/// A company definition bundled into the desktop app.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct DesktopPreset {
    /// Stable identifier used by the desktop host.
    pub id: &'static str,
    /// Human-readable company template name.
    pub name: &'static str,
    manifest: &'static str,
}

macro_rules! preset {
    ($id:literal, $name:literal) => {
        DesktopPreset {
            id: $id,
            name: $name,
            manifest: include_str!(concat!("../../../companies/", $id, "/company.toml")),
        }
    };
}

/// The product company templates shipped with the desktop app.
pub const PRESETS: &[DesktopPreset] = &[
    preset!("accounting_firm", "Agentic Accounting Firm"),
    preset!("consultation_firm", "Agentic Consultation Firm"),
    preset!("customer_support", "Agentic Customer Support"),
    preset!("design_studio", "Agentic Design Studio"),
    preset!("enterprise_sales", "Agentic Enterprise Sales"),
    preset!("game_business", "Agentic Game Business"),
    preset!("game_studio", "Agentic Game Studio"),
    preset!("influencer_business", "Agentic Influencer Business"),
    preset!("law_firm", "Agentic Law Firm"),
    preset!("marketing_agency", "Agentic Marketing Agency"),
    preset!("media_company", "Agentic Media Company"),
    preset!("pharma_startup", "Agentic Pharma Startup"),
    preset!("realestate_company", "Agentic Real Estate Company"),
    preset!("recruiting_company", "Agentic Recruiting Company"),
    preset!("software_company", "Agentic Software Company"),
    preset!("venture_capital", "Agentic Venture Capital"),
    preset!("venture_studio", "Agentic Venture Studio"),
    preset!("signals_opportunity_studio", "Signals Opportunity Studio"),
    preset!("startup_accelerator", "Startup Accelerator"),
];

/// The preset a first-run desktop install uses.
pub const DEFAULT_PRESET_ID: &str = "marketing_agency";
/// The origin used by Tauri v2's desktop webview.
pub const TAURI_WEBVIEW_ORIGIN: &str = "http://tauri.localhost";

impl DesktopPreset {
    /// Parses this preset's bundled manifest.
    ///
    /// Exposed so the setup flow can describe a template — its roster size, its
    /// own description — without reading `companies/` off disk, which a packaged
    /// install does not carry.
    pub fn manifest_parsed(&self) -> Result<CompanyManifest> {
        let mut manifest: CompanyManifest = toml::from_str(self.manifest).map_err(|error| {
            crate::OpenCompanyError::Config(format!(
                "bundled preset `{}` is invalid: {error}",
                self.id
            ))
        })?;
        // The roster lives in `agents/*.toml`, which the embedded `company.toml`
        // does not carry — see `embedded_roster`. Empty keeps any inline
        // `[[agent]]` entries the manifest declared, matching the exclusivity the
        // disk path implements from the other side.
        let roster = embedded_roster(self.id)?;
        if !roster.is_empty() {
            manifest.agents = roster;
        }
        Ok(manifest)
    }
}

/// Finds a bundled preset by its stable id.
pub fn preset(id: &str) -> Option<&'static DesktopPreset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// Where the webview should point itself at its embedded runtime.
///
/// No credential of any kind, and that is the whole shape of the desktop now:
/// the host runs `none` mode, so the address and the company are all a caller
/// needs. It carried an operator mailbox until the shell stopped signing
/// anybody in.
#[derive(Clone, Debug, Serialize)]
pub struct DesktopConfig {
    pub api_url: String,
    pub company: String,
}

/// A running local OpenCompany API. Dropping it aborts the loopback server.
pub struct DesktopRuntime {
    config: DesktopConfig,
    server: JoinHandle<Result<()>>,
}

impl DesktopRuntime {
    pub fn config(&self) -> &DesktopConfig {
        &self.config
    }
}

impl Drop for DesktopRuntime {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The manifest a first-run install starts from: a bundled preset, unchanged.
///
/// It used to carry one edit — a synthetic `operator@opencompany.local` pushed
/// into `[users].admins`, because `eligibility` admits an address only if it is
/// already a user, is named as a bootstrap admin, or holds a redeemable invite,
/// and a manifest naming nobody satisfies none of the three (issue #632).
///
/// The desktop no longer asks that question. It runs
/// [`AuthMode::None`](crate::app::config::AuthMode::None) host-wide, where
/// `eligibility` consults **no** bootstrap list at all and the single local
/// owner is materialized from the request rather than admitted from a roster.
/// An entry here would therefore grant nothing to nobody — and worse, it would
/// be an entry `validate_users` flags the moment anything writes
/// `[users].mode = "none"` beside it.
///
/// That is also why the mode is set as a *host-wide override* on the desktop's
/// `AppConfig` rather than written into these manifests: the override leaves
/// `manifest.users.mode` at its serde default, so nothing is flagged, while
/// `RuntimeBuilder::with_auth_mode_override` still outranks it at build.
fn first_run_manifest(preset_id: &str) -> Result<CompanyManifest> {
    let preset = preset(preset_id).ok_or_else(|| {
        crate::OpenCompanyError::Config(format!("unknown desktop preset `{preset_id}`"))
    })?;
    let mut manifest: CompanyManifest = toml::from_str(preset.manifest).map_err(|error| {
        crate::OpenCompanyError::Config(format!(
            "bundled desktop preset `{preset_id}` is invalid: {error}"
        ))
    })?;
    // The roster lives in `agents/*.toml`, which the embedded `company.toml`
    // does not carry — see `embedded_roster`.
    let roster = embedded_roster(preset_id)?;
    if !roster.is_empty() {
        manifest.agents = roster;
    }
    Ok(manifest)
}

/// The roster `build.rs` embedded for `preset_id`.
///
/// A bundle authors its roster either inline in `company.toml` as `[[agent]]`
/// entries or as one file per teammate under `agents/`, and the two are
/// exclusive — `CompanyManifest::from_file_with_agents` refuses a bundle that
/// has both rather than picking a precedence. The on-disk path moved with the
/// bundles when every shipped company adopted the per-file form; the embedded
/// path could not follow, because a preset carries a single `include_str!`'d
/// `company.toml` and `include_str!` cannot glob a directory. So the desktop
/// parsed a manifest whose `[[agent]]` section no longer existed and seeded a
/// company with nobody in it.
///
/// Empty means "this bundle has no `agents/` directory", which leaves whatever
/// `[[agent]]` entries the manifest declared untouched — the same exclusivity
/// the disk path implements, from the other side.
fn embedded_roster(preset_id: &str) -> Result<Vec<crate::company::Agent>> {
    let Some((_, files)) = generated::EMBEDDED_AGENT_BUNDLES
        .iter()
        .find(|(id, _)| *id == preset_id)
    else {
        return Ok(Vec::new());
    };

    let names = crate::company::agent_file::embedded_roster_names(files);
    crate::company::agent_file::load_agents_from(
        // Only ever used to label a parse failure, and a packaged install has
        // no path to name — the bundle id is what identifies it to a reader.
        std::path::Path::new(preset_id),
        &names,
        &|rel| {
            files
                .iter()
                .find(|(name, _)| *name == rel)
                .map(|(_, body)| (*body).to_string())
                .ok_or(std::io::ErrorKind::NotFound)
        },
    )
}

/// The bundle rosters `build.rs` embedded, one entry per shipped company.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));
}

/// Registers every company this data root already holds, seeding the first-run
/// company when it holds none. Returns the registered ids, in listing order.
///
/// ## Why an embedded host has to do this itself
///
/// A company reaches the registry one of two ways: `serve --company <dir>`
/// names one on the command line, or the hosting control plane provisions one
/// over `POST /api/v1/companies`. A packaged desktop has neither — nobody types
/// a flag at a double-clicked application, and provisioning demands the
/// `platform` scope, which `PlatformScope` grants only against a configured
/// `platform_auth` that a prosumer host deliberately does not have.
///
/// So the desktop booted an empty registry, and an empty registry cannot be
/// signed into: sign-in is per-company (`/api/v1/companies/{id}/auth/login`,
/// or the sole-company alias), which leaves a fresh install with a login form
/// addressing a company that does not exist and no way to create one. That is
/// issue #632, and this function is the missing step.
///
/// ## Adoption before seeding
///
/// The persisted bundles are read first and the preset is a *fallback*, because
/// seeding unconditionally would hand the operator a second copy of the starter
/// company on every launch, and skipping adoption would make the one they
/// already have unreachable. The bundle is the only authority here: a desktop
/// company has no source directory to re-read, so what the store wrote at the
/// last shutdown — including console-created desks, agents and workflows, which
/// `RuntimeBuilder::build` carries forward from the persisted record — is what
/// comes back.
///
/// An `archived` company is skipped. Archiving removes a company from the
/// registry on purpose (`src/server/provision.rs`), and re-registering it at
/// the next launch would undo that quietly.
///
/// ## What the desktop does now
///
/// Not this. The application starts every host through `adopt_companies`
/// alone and lets an empty registry open the first-run wizard, because seeding
/// answered the wizard's questions silently and then hid it for good — the
/// reasoning is on `crate::desktop`'s caller side, in the tauri crate's
/// `local::start_at`. The seed half stayed reachable, since the wizard itself
/// ends by calling `seed_company` with the template the operator picked, and
/// this function stayed whole because the two halves belong together for any
/// caller that does want a company without a wizard.
pub async fn bootstrap_companies(
    state: &AppState,
    preset_id: &str,
) -> Result<Vec<crate::ports::types::CompanyId>> {
    let registered = adopt_companies(state).await?;
    if !registered.is_empty() {
        return Ok(registered.into_iter().map(|(id, _)| id).collect());
    }

    let id = seed_company(state, preset_id).await?;
    Ok(vec![id])
}

/// Registers every company this data root already holds, in listing order.
///
/// The adopt half of [`bootstrap_companies`], split out because `serve` needs it
/// **without** the seed half. A company can now reach a data root without ever
/// being named on a command line — the first-run setup flow
/// (`crate::server::setup`) puts one there — and `serve` previously only ever
/// registered what `--company` named. So an operator who completed setup, was
/// told to restart for their settings to apply, and did, came back to
/// "serving with no companies": the bundle was on disk and simply never read.
///
/// Adopting is not seeding: this registers what is already there and creates
/// nothing, so a `serve` host with an empty data root still starts empty rather
/// than inventing a starter company nobody asked for.
///
/// Each adopted manifest is returned with its id because `serve` needs it to
/// start that company's cron scheduler — schedules live on the manifest, not on
/// the built runtime, and this function has already read it.
///
/// An `archived` company is skipped. Archiving removes a company from the
/// registry on purpose (`src/server/provision.rs`), and re-registering it at the
/// next launch would undo that quietly.
pub async fn adopt_companies(
    state: &AppState,
) -> Result<Vec<(crate::ports::types::CompanyId, CompanyManifest)>> {
    let store: std::sync::Arc<dyn crate::ports::CompanyStore> = match state.stores() {
        Some(handles) => handles.company.clone(),
        None => std::sync::Arc::new(crate::store::FsCompanyStore::new(
            state.home().to_path_buf(),
        )),
    };

    let mut registered = Vec::new();
    for summary in store.list().await? {
        if summary.lifecycle == "archived" {
            continue;
        }
        // One unreadable bundle must not cost the operator every other company
        // on the machine, so this warns and moves on rather than failing the
        // boot — the load as much as the build below.
        let record = match store.load(&summary.id).await {
            Ok(Some(record)) => record,
            // Listed a moment ago and gone now: a bundle removed under us.
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(company = %summary.id, %error, "could not read a stored company");
                continue;
            }
        };
        let manifest = record.manifest.clone();
        match register(state, summary.id.clone(), record.manifest, None).await {
            Ok(()) => registered.push((summary.id, manifest)),
            Err(error) => {
                tracing::warn!(company = %summary.id, %error, "could not adopt a stored company");
            }
        }
    }
    Ok(registered)
}

/// Registers `preset_id` as a company on this host, and returns its id.
///
/// The seed half of [`bootstrap_companies`], split out so the first-run setup
/// flow (`crate::server::setup`) can seed the template the **operator** chose
/// rather than [`DEFAULT_PRESET_ID`]. The packaged desktop still reaches it
/// through `bootstrap_companies` with the default, so its behavior is unchanged.
///
/// Note what this does *not* do: check whether the host already holds companies.
/// That is `bootstrap_companies`' adopt-before-seed rule, and separately the
/// setup route's "only seed an empty registry" guard — both of which exist so a
/// re-run never hands an operator a second starter company.
pub async fn seed_company(
    state: &AppState,
    preset_id: &str,
) -> Result<crate::ports::types::CompanyId> {
    seed_company_with(state, preset_id, SeedOverrides::default()).await
}

/// What the operator decides about a company seeded from a template.
///
/// Everything else is the template's. These two are not, and both are fixed at
/// seed time rather than editable afterwards — the id is minted from the name
/// here, and a company with no admin cannot be signed into at all.
#[derive(Clone, Copy, Debug, Default)]
pub struct SeedOverrides<'a> {
    /// What to call the company. `None` keeps the template's own
    /// `[company].name`, which is what every seed before the first-run wizard
    /// asked used, and what [`bootstrap_companies`] still passes.
    pub name: Option<&'a str>,
    /// The address that may sign in, written into `[users].admins`.
    ///
    /// No shipped product template names an admin, so on a host that asks
    /// people to sign in, a seed without this produces a company nobody can
    /// administer — setup completes, email sign-in is on, and the address the
    /// operator just typed is ineligible. The designed path has always written
    /// it (`manifest_from_setup`); the template path reached the console only
    /// once a picked template began being seeded as itself, which is what makes
    /// this reachable now.
    ///
    /// Ignored under `[users].mode = "none"`, where `validate_users` flags an
    /// admin list as granting nothing and both seeding paths treat a flagged
    /// manifest as a hard error.
    pub admin_email: Option<&'a str>,
}

/// [`seed_company`], with the decisions the operator made about it.
///
/// Split from `seed_company` rather than folded into it so the no-decision
/// callers — first-run bootstrap, tests — keep an entry point that says they
/// made none.
pub async fn seed_company_with(
    state: &AppState,
    preset_id: &str,
    overrides: SeedOverrides<'_>,
) -> Result<crate::ports::types::CompanyId> {
    let mut manifest = first_run_manifest(preset_id)?;
    if let Some(name) = overrides
        .name
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        manifest.company.name = name.to_string();
    }
    if let Some(email) = overrides
        .admin_email
        .map(str::trim)
        .filter(|email| !email.is_empty())
        && manifest.users.mode != "none"
    {
        manifest.users.admins = vec![email.to_string()];
    }
    let id = company_id_from_name(&manifest.company.name);
    // Issue #85: record which template this install started from. Only the
    // slug, never a host path — there is no source directory on a packaged
    // install, and provenance is exposed verbatim on the API surfaces.
    let provenance = crate::ports::types::TemplateProvenance {
        source_id: preset_id.to_string(),
        version: None,
        path: None,
    };
    register(state, id.clone(), manifest, Some(provenance)).await?;
    tracing::info!(company = %id, preset = preset_id, "seeded the first-run company");
    Ok(id)
}

/// Registers a company the first-run wizard **designed**, rather than one copied
/// from a preset.
///
/// The sibling of [`seed_company`], and deliberately a separate entry point: a
/// generated company carries no [`TemplateProvenance`], because it did not come
/// from a template. Stamping the reference roster's slug there would claim a
/// lineage the company does not have, and provenance is exposed verbatim on the
/// API surfaces — an operator reading `ecommerce` would reasonably conclude
/// their company *is* that template and could be re-seeded from it.
///
/// The manifest is expected to have come from
/// [`manifest_from_setup`](crate::company::setup::manifest_from_setup), which is
/// what guarantees it validates.
pub async fn seed_generated_company(
    state: &AppState,
    manifest: CompanyManifest,
    answers: Option<crate::company::setup::SetupAnswers>,
) -> Result<crate::ports::types::CompanyId> {
    let problems = manifest.validate();
    if !problems.is_empty() {
        // Refused rather than registered: a company that fails validation would
        // boot into a state the operator cannot fix from the console, and they
        // typed nothing wrong to get here.
        return Err(crate::OpenCompanyError::Config(format!(
            "the company this setup designed is not valid: {}",
            problems.join("; ")
        )));
    }
    let id = company_id_from_name(&manifest.company.name);
    register(state, id.clone(), manifest, None).await?;

    // Record what the operator told us, on the company it produced.
    //
    // Phase 2 builds this company's workflows from these answers, and the whole
    // point of storing them is that it never has to ask again. The company-scoped
    // route already does this; without it here, a company created through the
    // wizard — the *default* path — would be the one that arrives without them.
    //
    // Logged and swallowed: the company is registered and usable, and losing the
    // answers costs a re-ask later rather than the company itself.
    if let Some(answers) = answers
        && let Some(runtime) = state.registry().get(&id)
    {
        let store = runtime.store();
        match store.load(&id).await {
            Ok(Some(mut record)) => {
                record.setup = Some(answers);
                if let Err(error) = store.save(&record).await {
                    tracing::warn!(company = %id, %error, "could not store the setup answers");
                }
            }
            Ok(None) => tracing::warn!(company = %id, "no record to store the setup answers on"),
            Err(error) => tracing::warn!(company = %id, %error, "could not read the record back"),
        }
    }

    tracing::info!(company = %id, "seeded a company designed by first-run setup");
    Ok(id)
}

/// Builds one company over the instance home and puts it in the registry.
/// The builder assembly every desktop company is built with.
///
/// Shared by [`register`] and [`DesktopRebuilder`] so a rebuilt company cannot
/// be wired differently from the one it replaces — the same reason the binary's
/// `BootRebuilder` reuses `company_builder`. A rebuild that quietly dropped the
/// ACP factory would take every local harness down with it, and the symptom
/// (turns falling back to the echo brain) points nowhere near the cause.
fn desktop_builder(
    state: &AppState,
    id: crate::ports::types::CompanyId,
    manifest: CompanyManifest,
) -> Result<RuntimeBuilder> {
    let mut builder = attach_tinyhumans_feedback(
        crate::app::attach_harness(
            RuntimeBuilder::new(state.home().to_path_buf(), manifest),
            state.config(),
        ),
        state.config(),
    )
    .with_id(id)
    // The host-wide sign-in mode, which outranks the manifest's own.
    .with_auth_mode_override(state.auth_mode_override())
    .with_default_mcp_servers(state.config().default_mcp_servers.clone())
    .with_workspace_quota(state.config().workspace_quota)
    .with_workspace_git_enabled(state.config().workspace_git_enabled)
    // The same tracker the host reports through (`bin/opencompany.rs`,
    // `server/provision.rs`). Without it every company built here — and every
    // one `DesktopRebuilder` rebuilds — meters into a `NullTracker`, so a
    // desktop that reports `instance_started` would never report a turn.
    .with_analytics(state.analytics())
    // Empty unless a `skills_root` is set, which a packaged install has no
    // checkout to supply — so this resolves to the honest "this host serves no
    // shared registry" rather than inventing a directory to point at.
    .with_skills_registry(state.shared_skill_registry()?);
    if let Some(stores) = state.stores() {
        builder = builder.with_stores(stores);
    }
    // Issue #1245: `with_acp_agents` only exists under `acp` — an
    // `openhuman`-only build (or one with no `AcpAgentFactory` wired on
    // `state`, e.g. every non-desktop embedder) leaves the builder's default
    // `None`, so a `local` acp harness resolves `unavailable` exactly as it
    // already does.
    #[cfg(feature = "acp")]
    if let Some(factory) = state.acp_agents() {
        builder = builder.with_acp_agents(factory);
    }
    Ok(builder)
}

/// Routes feedback through the hub when this build can reach it, so a report
/// from a host holding a credential is recorded against that credential's
/// owner rather than filed from nowhere.
#[cfg(not(feature = "tinyhumans"))]
fn attach_tinyhumans_feedback(builder: RuntimeBuilder, _config: &AppConfig) -> RuntimeBuilder {
    builder
}

#[cfg(feature = "tinyhumans")]
fn attach_tinyhumans_feedback(builder: RuntimeBuilder, config: &AppConfig) -> RuntimeBuilder {
    match &config.tinyhumans_credential {
        Some(credential) => builder.with_tinyhumans_feedback(std::sync::Arc::new(
            crate::feedback::HttpTinyHumansClient::new(config.api_url.clone(), credential.clone()),
        )),
        None => builder,
    }
}

/// Rebuilds a desktop company in place, with the wiring it booted with.
///
/// The desktop needs this more than `serve` does, not less: it is the only host
/// that runs local ACP harnesses, so it is the only host where changing a
/// teammate's harness or model has to take effect without a restart. Wiring it
/// nowhere meant `rebuild_company` failed on exactly that host — the handler
/// logged and returned 200, so the console reported success while turns kept
/// using the old lane until the app was restarted.
pub struct DesktopRebuilder;

#[async_trait::async_trait]
impl crate::runtime::RuntimeRebuilder for DesktopRebuilder {
    async fn rebuild(
        &self,
        state: &AppState,
        request: crate::runtime::RebuildRequest,
    ) -> Result<crate::CompanyRuntime> {
        desktop_builder(state, request.id.clone(), request.manifest)?
            // The successor adopts the live journal, approval gate, stores and
            // harness pool rather than constructing a second copy of any of
            // them. Not attaching this is a correctness bug, not a missed
            // optimisation — see `RuntimeHandover`.
            .with_handover(request.handover)
            .build()
            .await
    }
}

async fn register(
    state: &AppState,
    id: crate::ports::types::CompanyId,
    manifest: CompanyManifest,
    provenance: Option<crate::ports::types::TemplateProvenance>,
) -> Result<()> {
    // The embedded agent harness, on the same terms `serve` attaches it: the
    // pool unconditionally, plus whichever managed backends the environment
    // supplies. Without this a desktop company had no harness even in a build
    // that compiled one in, so every turn fell back to the echo brain and the
    // console reported that this build cannot reach a model.
    let mut builder = desktop_builder(state, id.clone(), manifest)?;
    if let Some(provenance) = provenance {
        builder = builder.with_template_provenance(provenance);
    }
    let runtime = builder.build().await?;
    // The same refusal `serve` applies at boot and provisioning: a `none`-mode
    // company on a routable bind is an unauthenticated admin console. The
    // desktop app always binds loopback (`start_local` below), so this is
    // unreachable today — kept anyway so a future change to that bind cannot
    // silently reintroduce the gap on this, the third company-registration
    // path.
    if !runtime.auth_mode().has_login() && !state.config().is_local_only() {
        return Err(crate::OpenCompanyError::Config(format!(
            "company `{}` is configured with `[users].mode = \"none\"`, which has no sign-in, \
             but this host binds `{}` and would serve it to anyone who can reach that address.",
            runtime.id().as_ref(),
            state.config().bind,
        )));
    }
    state.registry().insert(id, std::sync::Arc::new(runtime));
    Ok(())
}

/// Starts an offline, loopback-only runtime from a bundled preset.
///
/// `none` host-wide, like the packaged shell (`src-tauri/src/embedded.rs`),
/// and set the same way — as an override on the host's own config rather than
/// as `[users].mode` in the preset manifests, which would not survive
/// `validate_users`. The two entry points must agree about the mode: this one
/// is not what ships, but it is the one whose name reads like it is, and a
/// company enterable here but not there (or the reverse) is a difference
/// nothing would report.
pub async fn start_local(home: impl Into<PathBuf>, preset_id: &str) -> Result<DesktopRuntime> {
    let manifest = first_run_manifest(preset_id)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address: SocketAddr = listener.local_addr()?;
    let company_id = company_id_from_name(&manifest.company.name);
    let state = AppState::new(AppConfig {
        bind: address.to_string(),
        auth_mode_override: Some(crate::app::config::AuthMode::None),
        ..AppConfig::default()
    })
    .with_home(home.into())
    .with_cors(CorsConfig {
        allowed_origins: vec![TAURI_WEBVIEW_ORIGIN.to_string()],
    });
    let runtime = crate::app::attach_harness(
        RuntimeBuilder::new(state.home().to_path_buf(), manifest),
        state.config(),
    )
    .with_id(company_id.clone())
    .with_auth_mode_override(state.auth_mode_override())
    .build()
    .await?;
    state
        .registry()
        .insert(company_id.clone(), std::sync::Arc::new(runtime));

    // Through the one production serving path, not a bare `axum::serve` — this
    // is where the `none`-mode local-owner peer and proxy-header gates are
    // wired (see `serve_on`'s doc comment). No longer merely defensive: this
    // host *is* `none`-mode, so those two per-request gates are the ones
    // standing between its owner's company and anything that reached this
    // socket from somewhere else.
    let server = tokio::spawn(crate::server::serve_on(listener, state));
    Ok(DesktopRuntime {
        config: DesktopConfig {
            api_url: format!("http://{address}"),
            company: company_id.as_ref().to_string(),
        },
        server,
    })
}

#[cfg(test)]
#[path = "desktop_tests.rs"]
mod tests;

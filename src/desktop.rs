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
            manifest: include_str!(concat!("../companies/", $id, "/company.toml")),
        }
    };
}

/// The product company templates shipped with the desktop app.
pub const PRESETS: &[DesktopPreset] = &[
    preset!("agentic_accounting_firm", "Agentic Accounting Firm"),
    preset!("agentic_consultation_firm", "Agentic Consultation Firm"),
    preset!("agentic_customer_support", "Agentic Customer Support"),
    preset!("agentic_design_studio", "Agentic Design Studio"),
    preset!("agentic_enterprise_sales", "Agentic Enterprise Sales"),
    preset!("agentic_game_business", "Agentic Game Business"),
    preset!("agentic_game_studio", "Agentic Game Studio"),
    preset!("agentic_influencer_business", "Agentic Influencer Business"),
    preset!("agentic_law_firm", "Agentic Law Firm"),
    preset!("agentic_marketing_agency", "Agentic Marketing Agency"),
    preset!("agentic_media_company", "Agentic Media Company"),
    preset!("agentic_pharma_startup", "Agentic Pharma Startup"),
    preset!("agentic_realestate_company", "Agentic Real Estate Company"),
    preset!("agentic_recruiting_company", "Agentic Recruiting Company"),
    preset!("agentic_software_company", "Agentic Software Company"),
    preset!("agentic_venture_capital", "Agentic Venture Capital"),
    preset!("agentic_venture_studio", "Agentic Venture Studio"),
    preset!("signals_opportunity_studio", "Signals Opportunity Studio"),
    preset!("startup_accelerator", "Startup Accelerator"),
];

/// The preset a first-run desktop install uses.
pub const DEFAULT_PRESET_ID: &str = "agentic_marketing_agency";
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
) -> RuntimeBuilder {
    let mut builder =
        crate::app::attach_harness(RuntimeBuilder::new(state.home().to_path_buf(), manifest))
            .with_id(id)
            // The host-wide sign-in mode, which outranks the manifest's own.
            .with_auth_mode_override(state.auth_mode_override());
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
    builder
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
        desktop_builder(state, request.id.clone(), request.manifest)
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
    let mut builder = desktop_builder(state, id.clone(), manifest)
            // The host-wide sign-in mode (`OPENCOMPANY_AUTH_MODE` / `config.toml`
            // `auth_mode`), which outranks this manifest's own `[users].mode`.
            //
            // `serve`'s `--company` path has always applied it; this one did not,
            // so a company registered here silently kept its manifest's mode. That
            // was invisible while only the desktop app reached this code, and stops
            // being invisible the moment anything else does: the first-run setup
            // flow writes `auth_mode` and tells the operator to restart, and
            // without this the restart adopts the company and quietly ignores the
            // setting they were just told would take effect. `None` — the normal
            // case — leaves each manifest to name its own mode, as before.
        ;
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
    let runtime =
        crate::app::attach_harness(RuntimeBuilder::new(state.home().to_path_buf(), manifest))
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
mod tests {
    use super::*;

    use crate::ports::CompanyStore;

    #[test]
    fn ships_the_full_company_template_catalog() {
        assert_eq!(PRESETS.len(), 19);
        assert_eq!(
            preset(DEFAULT_PRESET_ID).unwrap().name,
            "Agentic Marketing Agency"
        );
        assert!(PRESETS.iter().all(|preset| !preset.manifest.is_empty()));
    }

    /// The embedded roster must equal the one the same bundle yields on disk.
    ///
    /// Two code paths now produce a roster — `agents/` read off the filesystem
    /// by `CompanyManifest::from_located`, and the table `build.rs` embeds — and
    /// a company must not depend on which one loaded it. They share a parser, so
    /// field-level drift is unlikely; **order** is the real risk, and it is
    /// load-bearing: `orchestrator_id` falls back to "the first agent declared"
    /// when nobody is tagged `tier = "orchestrator"`, so a different sort would
    /// silently hand the company to a different teammate.
    ///
    /// Compares ids in order, plus each agent's resolved prompt documents, which
    /// is where the embedded path does the most work that disk gets for free.
    #[test]
    fn the_embedded_roster_matches_the_bundle_on_disk() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("companies");

        for preset in PRESETS {
            let bundle = root.join(preset.id);
            if !bundle.join(crate::company::agent_file::AGENTS_DIR).is_dir() {
                continue;
            }

            let from_disk = crate::company::agent_file::load_agents(&bundle)
                .unwrap_or_else(|e| panic!("bundle `{}` must load from disk: {e}", preset.id));
            let embedded = embedded_roster(preset.id)
                .unwrap_or_else(|e| panic!("bundle `{}` must load embedded: {e}", preset.id));

            assert_eq!(
                embedded.iter().map(|a| &a.id).collect::<Vec<_>>(),
                from_disk.iter().map(|a| &a.id).collect::<Vec<_>>(),
                "bundle `{}` yields a different roster order embedded than on \
                 disk, which changes which teammate orchestrates",
                preset.id
            );
            for (embedded, disk) in embedded.iter().zip(from_disk.iter()) {
                assert_eq!(
                    embedded.prompt_files_resolved, disk.prompt_files_resolved,
                    "agent `{}` in bundle `{}` resolves different prompt \
                     documents embedded than on disk",
                    disk.id, preset.id
                );
                assert_eq!(embedded.role, disk.role);
                assert_eq!(embedded.tier, disk.tier);
                assert_eq!(embedded.tools, disk.tools);
            }
        }
    }

    /// Every bundled template must seed a company that has teammates.
    ///
    /// The roster moved out of `company.toml` and into `agents/*.toml`, and the
    /// disk path followed it — `CompanyManifest::from_located` reads the bundle
    /// when `agents/` is present. The embedded path did not: a preset carries
    /// only the `include_str!`'d `company.toml`, so `first_run_manifest` parsed
    /// a manifest whose `[[agent]]` entries no longer exist and seeded an empty
    /// roster. That reaches both desktop first-run and the setup flow, since
    /// `seed_company` builds from this manifest.
    ///
    /// Asserted for every preset rather than just the default, because the
    /// failure is per bundle and silent: a company with no teammates still
    /// boots and still serves, it simply never answers.
    #[test]
    fn every_preset_seeds_a_non_empty_roster() {
        for preset in PRESETS {
            let manifest = first_run_manifest(preset.id)
                .unwrap_or_else(|e| panic!("preset `{}` must parse: {e}", preset.id));

            assert!(
                !manifest.agents.is_empty(),
                "preset `{}` seeds a company with no teammates — its roster lives \
                 in `agents/*.toml`, which the embedded manifest does not carry",
                preset.id
            );
        }
    }

    /// No shipped template narrows Composio to a hand-written toolkit list.
    ///
    /// Absent means open mode: the host answers with the backend's live catalog,
    /// which is every toolkit it permits. A non-empty list is authoritative and
    /// offered verbatim — the catalog is not consulted and nothing may widen it —
    /// so declaring one here caps both the agent belt and the Connections tab at
    /// whatever was typed, for every operator who starts from that template.
    ///
    /// `agentic_software_company` briefly carried such a list, added to work
    /// around a console that rendered zero provider rows for an empty one
    /// (#397). That root cause is fixed, so a list added here now buys nothing
    /// and silently restores the cap. Narrowing a template is a legitimate
    /// choice — it is just one that has to be made deliberately, which is what
    /// this test forces (#550).
    #[test]
    fn no_shipped_template_caps_the_composio_toolkit_list() {
        for preset in PRESETS {
            let manifest: toml::Value = toml::from_str(preset.manifest)
                .unwrap_or_else(|e| panic!("{} manifest does not parse: {e}", preset.id));
            let declared = manifest
                .get("tools")
                .and_then(|tools| tools.get("composio"))
                .and_then(|composio| composio.get("toolkits"));
            assert!(
                declared.is_none(),
                "{} declares [tools.composio].toolkits = {:?}, which pins its \
                 Connections tab to that list instead of the backend's catalog. \
                 Remove it, or narrow this template on purpose and say why here.",
                preset.id,
                declared.unwrap(),
            );
        }
    }

    /// `manifest_parsed` is what `server::setup::templates()` calls to describe
    /// each shipped preset without touching disk — so a preset that fails to
    /// parse must report a specific, attributable error rather than panic or
    /// silently produce garbage.
    #[test]
    fn manifest_parsed_reads_a_real_preset() {
        let preset = preset(DEFAULT_PRESET_ID).expect("default preset is shipped");
        let manifest = preset.manifest_parsed().unwrap();
        assert_eq!(manifest.company.name, "Agentic Marketing Agency");
        assert!(
            !manifest.agents.is_empty(),
            "the default preset should ship at least one agent"
        );
    }

    #[test]
    fn manifest_parsed_reports_a_configuration_error_for_a_malformed_manifest() {
        let bad = DesktopPreset {
            id: "not-a-real-preset",
            name: "Not A Real Preset",
            manifest: "this is not valid toml >>>",
        };
        let err = bad.manifest_parsed().unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("not-a-real-preset"),
            "the error should name the offending preset id: {message}"
        );
    }

    #[tokio::test]
    async fn starts_a_loopback_runtime_from_the_default_preset() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = start_local(directory.path(), DEFAULT_PRESET_ID)
            .await
            .unwrap();
        assert!(runtime.config().api_url.starts_with("http://127.0.0.1:"));
        assert_eq!(runtime.config().company, "agentic-marketing-agency");
    }

    /// A desktop company must come up with an agent harness attached.
    ///
    /// The gap this pins was two-part and each half was silent on its own. The
    /// desktop shell shipped without the `openhuman` feature, so `src/harness/`
    /// was not compiled at all and the console's inference test answered "This
    /// build cannot reach a model — the agent harness is not compiled in"; and
    /// `register` — the only path a desktop company is built through — never
    /// called `with_harness`, so even a build that *did* compile one in would
    /// have handed every company the echo brain instead.
    ///
    /// Gated on the feature because there is nothing to attach without it, and
    /// asserted here rather than on `attach` itself because the seam that broke
    /// is this registration path, not the helper.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_seeded_company_has_a_harness() {
        let directory = tempfile::tempdir().unwrap();
        let state = state_over(directory.path());

        bootstrap_companies(&state, DEFAULT_PRESET_ID)
            .await
            .expect("a fresh root bootstraps");

        let runtime = state.registry().sole().expect("the company is registered");
        assert!(
            runtime.harness().is_some(),
            "a desktop company was built without a harness pool, so every turn \
             falls back to the echo brain",
        );
    }

    /// A state over `home`, as an embedded host builds one.
    fn state_over(home: &std::path::Path) -> AppState {
        AppState::new(AppConfig::default()).with_home(home.to_path_buf())
    }

    fn store_over(home: &std::path::Path) -> crate::store::FsCompanyStore {
        crate::store::FsCompanyStore::new(home.to_path_buf())
    }

    /// A state shaped like the packaged desktop: loopback, and `none` host-wide.
    ///
    /// The override is what the shipped shell sets (`src-tauri/src/embedded.rs`),
    /// and it is set *there* rather than in the preset manifests deliberately —
    /// `[users].mode = "none"` beside a `[users].admins` entry is a manifest
    /// `validate_users` flags, and both seeding paths treat a flagged manifest as
    /// a hard error. The override never rewrites the manifest, so nothing is
    /// flagged.
    fn desktop_state_over(home: &std::path::Path) -> AppState {
        AppState::new(AppConfig {
            auth_mode_override: Some(crate::app::config::AuthMode::None),
            ..AppConfig::default()
        })
        .with_home(home.to_path_buf())
    }

    /// Issue #632: a fresh install must end up somewhere its owner can work.
    ///
    /// The answer changed shape rather than going away. The seeded company used
    /// to name a synthetic mailbox in `[users].admins`, because `eligibility`
    /// admits nobody a manifest has not named and a company nobody is eligible
    /// for is the same dead end as no company at all. The desktop now runs
    /// [`AuthMode::None`](crate::app::config::AuthMode::None), where there is no
    /// eligibility question to answer: the person at the machine *is* the
    /// principal. A bootstrap roster would grant nothing to nobody.
    ///
    /// So the assertion moved rather than relaxed — an empty `[users].admins` is
    /// now the correct state, and the thing that must hold is the mode the
    /// company is actually built in.
    #[tokio::test]
    async fn a_first_run_seeds_a_company_its_owner_needs_no_account_for() {
        let directory = tempfile::tempdir().unwrap();
        let state = desktop_state_over(directory.path());

        let ids = bootstrap_companies(&state, DEFAULT_PRESET_ID)
            .await
            .expect("a fresh root bootstraps");

        assert_eq!(ids.len(), 1, "one starter company, not a fleet");
        let runtime = state
            .registry()
            .sole()
            .expect("the seeded company is registered, not merely written");
        assert_eq!(
            runtime.auth_mode(),
            crate::app::config::AuthMode::None,
            "the host-wide mode is what a desktop company is built in"
        );
        let record = runtime
            .store()
            .load(runtime.id())
            .await
            .unwrap()
            .expect("the seeded company persists");
        assert!(
            record.manifest.users.admins.is_empty(),
            "a `none`-mode company has no bootstrap roster to name, and naming \
             one here would put `[users].admins` under a mode that grants it \
             nothing: {:?}",
            record.manifest.users.admins
        );
        assert_eq!(
            record.template_provenance.map(|p| p.source_id),
            Some(DEFAULT_PRESET_ID.to_string()),
            "the install records which template it started from"
        );
    }

    /// The second launch is the one that would go wrong quietly: seeding again
    /// hands the operator a duplicate starter company, and every launch after
    /// that another.
    #[tokio::test]
    async fn a_later_launch_adopts_what_the_root_already_holds() {
        let directory = tempfile::tempdir().unwrap();
        let first = bootstrap_companies(&state_over(directory.path()), DEFAULT_PRESET_ID)
            .await
            .unwrap();

        // A wholly fresh state, as a relaunched application has.
        let relaunched = state_over(directory.path());
        let second = bootstrap_companies(&relaunched, DEFAULT_PRESET_ID)
            .await
            .unwrap();

        assert_eq!(first, second, "the same company comes back");
        assert_eq!(
            std::fs::read_dir(directory.path().join("companies"))
                .unwrap()
                .count(),
            1,
            "and no second bundle was written"
        );
        assert!(relaunched.registry().sole().is_some());
    }

    /// A damaged bundle costs its own company and nothing else.
    ///
    /// The failure this rules out is the whole-desktop one: a boot that gave up
    /// on the first unreadable bundle would leave an operator with no local host
    /// at all — not even the companies that are perfectly intact — and the
    /// console renders that as "no embedded host", which reads like the app is
    /// broken rather than like one company is.
    #[tokio::test]
    async fn a_damaged_bundle_does_not_take_the_healthy_one_with_it() {
        let directory = tempfile::tempdir().unwrap();
        let state = state_over(directory.path());
        let healthy = first_run_manifest(DEFAULT_PRESET_ID).unwrap();
        let healthy_id = company_id_from_name(&healthy.company.name);
        register(&state, healthy_id.clone(), healthy, None)
            .await
            .unwrap();

        let damaged = directory.path().join("companies").join("broken");
        std::fs::create_dir_all(&damaged).unwrap();
        std::fs::write(
            damaged.join("company.toml"),
            "this is not manifest = toml {{",
        )
        .unwrap();

        let relaunched = state_over(directory.path());
        let ids = bootstrap_companies(&relaunched, DEFAULT_PRESET_ID)
            .await
            .expect("a damaged bundle must not fail the boot");

        assert_eq!(ids, vec![healthy_id], "the intact company still comes up");
        assert!(
            relaunched.registry().sole().is_some(),
            "and it is the only one, rather than joined by a second starter"
        );
    }

    /// Archiving removes a company from the registry deliberately. A boot that
    /// re-registered every bundle on disk would undo that at the next launch,
    /// silently, and the operator would find it back in the picker.
    #[tokio::test]
    async fn an_archived_company_is_not_brought_back() {
        let directory = tempfile::tempdir().unwrap();
        let state = state_over(directory.path());
        bootstrap_companies(&state, DEFAULT_PRESET_ID)
            .await
            .unwrap();
        // A second company, so the archived one is skipped rather than merely
        // replaced by the first-run seed.
        let kept = first_run_manifest("agentic_law_firm").unwrap();
        let kept_id = company_id_from_name(&kept.company.name);
        register(&state, kept_id.clone(), kept, None).await.unwrap();

        let store = store_over(directory.path());
        let archived_id =
            company_id_from_name(&first_run_manifest(DEFAULT_PRESET_ID).unwrap().company.name);
        let mut record = store.load(&archived_id).await.unwrap().unwrap();
        record.lifecycle = "archived".to_string();
        store.save(&record).await.unwrap();

        let relaunched = state_over(directory.path());
        let ids = bootstrap_companies(&relaunched, DEFAULT_PRESET_ID)
            .await
            .unwrap();

        assert_eq!(ids, vec![kept_id]);
        assert!(
            relaunched.registry().get(&archived_id).is_none(),
            "an archived company must stay unaddressable"
        );
    }

    /// `adopt_companies` registers what is on disk and seeds nothing.
    ///
    /// This is the half `serve` calls when no `--company` was named. A company
    /// can now reach a data root without ever being named on a command line —
    /// the first-run setup flow puts one there — and before this existed, an
    /// operator who finished setup, was told to restart for their settings to
    /// apply, and did, came back to "serving with no companies" with their
    /// company sitting unread on disk.
    #[tokio::test]
    async fn adopting_registers_a_stored_company_without_seeding_one() {
        let directory = tempfile::tempdir().unwrap();

        // An empty root adopts nothing and — unlike `bootstrap_companies` —
        // must not invent a starter company. `serve` is not the desktop app.
        let empty = state_over(directory.path());
        assert!(adopt_companies(&empty).await.unwrap().is_empty());
        assert_eq!(empty.registry().len(), 0, "adopting must never seed");

        // Now put one there, the way setup does.
        let seeded = state_over(directory.path());
        let id = seed_company(&seeded, "agentic_law_firm").await.unwrap();

        // A later boot finds it.
        let relaunched = state_over(directory.path());
        let adopted = adopt_companies(&relaunched).await.unwrap();
        assert_eq!(
            adopted.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            vec![id.clone()],
        );
        assert!(relaunched.registry().get(&id).is_some());
        assert_eq!(
            adopted[0].1.company.name, "Agentic Law Firm",
            "the manifest comes back with the id, because serve needs its schedules"
        );
    }

    /// The host-wide sign-in mode reaches a company registered through this
    /// path, not just one named by `serve --company`.
    ///
    /// Without it the first-run setup flow is a lie: it writes `auth_mode` to
    /// `config.toml`, tells the operator the setting needs a restart, and the
    /// restart then adopts the company under its manifest's own mode instead —
    /// configuration silently ignored, which is the exact failure the setup
    /// surface exists to prevent.
    #[tokio::test]
    async fn the_host_wide_auth_mode_reaches_an_adopted_company() {
        let directory = tempfile::tempdir().unwrap();

        let seeding = state_over(directory.path());
        let id = seed_company(&seeding, "agentic_law_firm").await.unwrap();
        assert_eq!(
            seeding.registry().get(&id).unwrap().auth_mode(),
            crate::app::config::AuthMode::Email,
            "the shipped template's own mode, with no override in play"
        );

        // Loopback, so `none` is permitted — a routable host refuses it.
        let overridden = AppState::new(AppConfig {
            bind: "127.0.0.1:8080".to_string(),
            auth_mode_override: Some(crate::app::config::AuthMode::None),
            ..AppConfig::default()
        })
        .with_home(directory.path().to_path_buf());

        adopt_companies(&overridden).await.unwrap();

        assert_eq!(
            overridden.registry().get(&id).unwrap().auth_mode(),
            crate::app::config::AuthMode::None,
            "the host-wide mode must outrank the manifest's `[users].mode`"
        );
    }
}

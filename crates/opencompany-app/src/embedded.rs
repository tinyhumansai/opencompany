//! The host, running inside the desktop process.
//!
//! ## Why it still binds a real socket
//!
//! The obvious optimisation is to skip the network: hold the axum `Router` and
//! drive it in-process through `tower::Service`. It is rejected on purpose.
//!
//! With a real listener, the embedded path exercises the identical
//! serialisation, auth extractors (`ScopedCompany`, the session carrier), CORS
//! branch, error envelopes and event framing as a remote host. Every Playwright
//! spec, every future ACP conformance test, and the proxy's own tests are then
//! valid evidence about embedded mode too. Skipping the socket saves perhaps
//! 50µs per request and buys a *second code path* that will diverge — and
//! divergence in an auth extractor is precisely the class of bug that cannot be
//! afforded.
//!
//! ## Loopback and an ephemeral port
//!
//! `127.0.0.1:0`, never `0.0.0.0`: an embedded instance is this machine's, and
//! binding a routable address would quietly publish someone's company to their
//! network. Port `0` because a fixed 8080 collides with a dev server or a second
//! app — a support case that reads "it works unless I have a terminal open".
//! The OS picks; [`EmbeddedHost::address`] reports what it picked.

use std::path::PathBuf;

use opencompany::app::EmbeddedInstance;
use opencompany::{AppConfig, AppState};

/// A running in-process host.
pub struct EmbeddedHost {
    address: std::net::SocketAddr,
    instance_id: String,
    companies: Vec<String>,
    /// Holds the data root's exclusive lock for as long as the host runs.
    /// Dropping it would release the root while this process kept writing.
    _instance: EmbeddedInstance,
    server: tokio::task::JoinHandle<()>,
    sweeper: tokio::task::JoinHandle<()>,
}

impl EmbeddedHost {
    /// The loopback address the console should point at.
    pub fn address(&self) -> std::net::SocketAddr {
        self.address
    }

    /// The base URL for a connection record.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// This host's stable identity, from `instance-id` under the data root.
    ///
    /// The console needs this precisely *because* [`Self::base_url`] is not
    /// stable: the port is ephemeral by design (see above), so a client that
    /// recognises this host by its address recognises a new host on every
    /// launch and accumulates a dead connection per run. The address says where
    /// to knock; this says who answers.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// The companies registered at boot, in listing order.
    pub fn companies(&self) -> &[String] {
        &self.companies
    }
}

impl Drop for EmbeddedHost {
    fn drop(&mut self) {
        // The task owns the listener; aborting it closes the port. Without this
        // a restarted embedded host would leak a listener per restart.
        self.server.abort();
        self.sweeper.abort();
    }
}

/// What a host does about a data root that holds no company yet.
///
/// The two answers to the same question, and only one may be given per host.
/// A host that seeds is a host that is *already set up* — `AppSpec` reports
/// `setup_complete` as `stamp || !registry.is_empty()` — so seeding does not
/// merely add a company, it suppresses the first-run wizard the console would
/// otherwise open (`views/setup/SetupWizard.tsx`), permanently and with no way
/// back to it.
///
/// That last clause is why the packaged application now gives the same answer
/// for **every** instance it starts, including the one at the data root, which
/// used to seed: see `local::start_at`. What is left here is a knob for callers
/// that want a populated host without walking a wizard — the test suites, which
/// need a company to address, and any embedder in the same position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstRun {
    /// Register a starter company from the default preset when the root is
    /// empty, so the host is usable with no decisions at all
    /// ([issue #632](https://github.com/tinyhumansai/opencompany/issues/632)).
    ///
    /// No longer what a launched application does — #632's requirement is now
    /// met by the wizard being reachable rather than by the answer being
    /// assumed. Reached through [`start`], which the test suites use.
    SeedStarterCompany,
    /// Register only what the root already holds, leaving an empty root empty.
    ///
    /// What every instance the desktop starts does. An empty root then reports
    /// setup outstanding and the console opens the wizard against it; a root
    /// with companies in it — every install that has been used — adopts them
    /// and goes straight to the console, exactly as before.
    RunSetupWizard,
}

/// Boots a host over `data_dir`, seeding a starter company on an empty root.
///
/// The seeding entry point, kept for callers that want a host with a company in
/// it without completing setup first. The application itself starts its hosts
/// through [`start_with`] with [`FirstRun::RunSetupWizard`] — see
/// `local::start_at` for why.
///
/// `data_dir` is passed explicitly rather than resolved from the environment.
/// A desktop app knows its platform data directory and should say so — and the
/// crate's own fallback resolves a *relative* path when neither `HOME` nor
/// `USERPROFILE` is set, which for a double-clicked application is wherever the
/// launcher happened to put it.
pub async fn start(data_dir: PathBuf) -> opencompany::Result<EmbeddedHost> {
    start_with(data_dir, FirstRun::SeedStarterCompany).await
}

/// Boots a host over `data_dir`, deciding what an empty root means.
///
/// See [`FirstRun`]. Everything else — the lock, the migration, the journal
/// check, the loopback bind — is identical, because the two kinds of host
/// differ in exactly one decision and must not be allowed to drift in any
/// other.
pub async fn start_with(
    data_dir: PathBuf,
    first_run: FirstRun,
) -> opencompany::Result<EmbeddedHost> {
    // Resolve, lock, migrate, and prove the journal root is writable — the same
    // sequence `serve` runs, shared rather than copied so the two cannot drift.
    // The lock is what refuses a second instance over one data root, including
    // the very ordinary case of a terminal already running `opencompany serve`
    // against the same default.
    let instance = opencompany::app::prepare_instance(Some(data_dir)).await?;

    // The desktop intentionally does not initialize the embedded OpenHuman
    // harness while its API migration is in progress. Its workspace and
    // identity setup will return with that harness.

    // The host-wide layers — the process environment, then this root's
    // `config.toml` — resolved through the pass every host shares. What is
    // spelled out below is only what this host owns: the loopback bind and the
    // sign-in default.
    let config_file = opencompany::app::config::ConfigFile::load(instance.home())?;

    // Bound HERE, before the config exists, so the config can name the port the
    // OS actually chose. It used to be bound after, with `bind` left reading
    // the literal `127.0.0.1:0` it was asked for — and everything that derives
    // an address from `config().bind` then said port `0`: a TinyHumans key
    // grant sent the browser back to `http://127.0.0.1:0/…`, which Chrome
    // refuses outright (`ERR_UNSAFE_PORT`), and an MCP OAuth redirect URI was
    // registered the same way. `bind` is what `host_base_url()` reads, so it
    // has to be the truth, not the request.
    //
    // `127.0.0.1:0`, never `0.0.0.0`: an embedded instance is this machine's,
    // and a routable address would publish someone's company to their café.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| {
            opencompany::error::OpenCompanyError::Config(format!(
                "could not bind `127.0.0.1:0`: {error}"
            ))
        })?;
    let address = listener.local_addr().map_err(|error| {
        opencompany::error::OpenCompanyError::Config(format!(
            "could not read the embedded host's bound address: {error}"
        ))
    })?;

    let config = AppConfig {
        bind: address.to_string(),
        // No sign-in, for every company this host serves.
        //
        // A desktop install is one machine and one person: there is nobody to
        // invite, nobody to tell apart, and no mailbox to send a link to. What
        // the login screen actually bought was a synthetic
        // `operator@opencompany.local` the operator was told to accept, a magic
        // link the host echoed back into its own response because there was no
        // transport, and a cookie the Tauri proxy then discarded — it holds no
        // cookie store and strips `x-opencompany-session` as a reserved header.
        // The console got through on `is_local_only` regardless. `none` deletes
        // the ceremony and says what was already true.
        //
        // Set here, as a **host-wide override**, rather than as
        // `[users].mode = "none"` in the shipped preset manifests. Two reasons,
        // and both matter:
        //
        // - It reaches every company on this host — the starter preset, one the
        //   setup wizard designed, and any already on disk from an install that
        //   predates this. `RuntimeBuilder::with_auth_mode_override` resolves it
        //   at build and it outranks whatever `[users].mode` a manifest names,
        //   so an existing install migrates by relaunching.
        // - `validate_users` flags `[users].admins` under `mode = "none"` as
        //   granting nothing, and both seeding paths treat a flagged manifest as
        //   a hard error. The override never rewrites `manifest.users.mode`, so
        //   there is nothing to flag.
        //
        // Safe only because this host binds loopback with no `public_url`, which
        // is what `is_local_only()` asks and what `desktop::register` refuses a
        // `none`-mode company without.
        //
        // A *default*, though, not a ceiling: the root's `config.toml` wins when
        // it names a mode, because the setup wizard writes that key and an
        // operator who deliberately turned a sign-in on — to share their
        // instance with somebody — must not find it off again at the next
        // launch.
        auth_mode_override: Some(
            instance
                .auth_mode()
                .unwrap_or(opencompany::app::config::AuthMode::None),
        ),
        ..AppConfig::resolve_host(&opencompany::app::config::ProcessEnv, config_file.as_ref())?
    };
    let api_url = config.api_url.clone();
    let state = AppState::new(config)
        .with_home(instance.home().to_path_buf())
        // Issue #1245: the desktop is the one place with an
        // `AcpAgentFactory` implementation to give — a `local` acp harness
        // only has an engine because this line exists.
        .with_acp_agents(std::sync::Arc::new(crate::acp::LocalAcpAgentFactory))
        // Without this, `rebuild_company` fails on the one host that most
        // needs it. The desktop is the only host that runs local ACP
        // harnesses, so it is the only host where changing a teammate's
        // harness or model must take effect without a restart — and the edit
        // handler logs the failure and still returns 200, so the console
        // reported success while turns kept using the old lane.
        //
        // Wired before any company registers, matching `serve`, so the first
        // edit on a freshly booted host already has a rebuilder to reach for.
        .with_rebuilder(std::sync::Arc::new(opencompany::desktop::DesktopRebuilder))
        // Without this `hub_identity()` is `None`, and every surface that asks
        // the hub whose token this is answers as though the host belonged to no
        // ecosystem: the Account page reports the balance unknown, and
        // `credential/link/start` refuses before it builds a URL. Unconditional
        // rather than `#[cfg]`-guarded, for the same reason
        // `install_into_embedded_core` above is: this crate's `opencompany`
        // dependency enables `tinyhumans` outright, so a desktop build without
        // the exchange is not a shape that exists — and if that dependency line
        // ever loses the feature, this stops compiling rather than shipping a
        // host that silently disowns its own account.
        .with_hub_identity(std::sync::Arc::new(
            opencompany::server::hub_identity::HttpHubIdentityExchange::new(api_url),
        ));
    // Read before `state` moves into `bind`. Minting here rather than on the
    // first `/spec` also means the console can be told who this host is without
    // waiting to contact it — which is the whole point, since the address it
    // would contact is what changed.
    let instance_id = state.instance_id().to_string();
    // Before the listener, not after: a console that reached a host with an
    // empty registry would render the "no companies" dead end this exists to
    // remove, and the race is winnable — the address is handed to the webview
    // the moment `start` returns.
    let companies = match first_run {
        FirstRun::SeedStarterCompany => opencompany::desktop::bootstrap_companies(
            &state,
            opencompany::desktop::DEFAULT_PRESET_ID,
        )
        .await?
        .into_iter()
        .map(|id| id.as_ref().to_string())
        .collect::<Vec<_>>(),
        // The adopt half of the same call, and *only* that half. Adoption is
        // not optional for either kind of host: a company the setup wizard
        // wrote into this root is a bundle on disk, and a host that skipped
        // adoption would come back from every restart serving nothing.
        FirstRun::RunSetupWizard => opencompany::desktop::adopt_companies(&state)
            .await?
            .into_iter()
            .map(|(id, _)| id.as_ref().to_string())
            .collect::<Vec<_>>(),
    };

    // Called before `state` moves into `bind`: the standalone binary spawns
    // this sweeper from its own `async_main`, which the embedded host never
    // runs, so nothing here ever reclaimed an idle ACP session — they and
    // their per-connection and host-wide cap slots piled up for the life of
    // the process. `AppState::spawn_acp_session_sweeper` is a
    // no-op in a build without the `acp` feature — this crate's own default
    // dependency features don't include it (`Cargo.toml`), only the
    // release/CI feature set does, and this call site has to compile and do
    // the right thing either way.
    //
    // Never notified: `Drop` aborts the task directly, matching how `server`
    // below is already stopped, rather than plumbing a second shutdown path
    // through a struct that otherwise has none.
    let sweeper = state.spawn_acp_session_sweeper(std::sync::Arc::new(tokio::sync::Notify::new()));
    // The listener was bound at the top of this function (see there for why),
    // so nothing here can fail between starting the sweeper and serving.
    let server = tokio::spawn(async move {
        if let Err(error) = opencompany::server::serve_on(listener, state).await {
            tracing::error!(%error, "the embedded host stopped");
        }
    });

    tracing::info!(
        %address,
        %instance_id,
        companies = companies.len(),
        home = %instance.home().display(),
        "embedded host listening"
    );
    Ok(EmbeddedHost {
        address,
        instance_id,
        companies,
        _instance: instance,
        server,
        sweeper,
    })
}

#[cfg(test)]
#[path = "embedded_tests.rs"]
mod tests;

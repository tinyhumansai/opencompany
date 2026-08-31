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

    // Both of these must run before any company runtime or agent harness
    // exists, and this is the last point in the boot where that is still true.
    //
    // The keyring pin registers the instance root as the vendored runtime's
    // credential directory, so an MCP server's token lands beside the journal
    // rather than at the end of the vendored resolver's own fallback chain
    // (`$HOME`, then `/tmp` at no log level — issue #451). `main` exports
    // `OPENHUMAN_WORKSPACE` for the same root, but that only wins if nothing
    // has touched the keyring yet; registering says it outright.
    //
    // The identity tags every request the embedded core makes to the
    // TinyHumans backend as opencompany's rather than the vendored runtime's
    // own `openhuman` default (issue #376). Core reads it into a client's
    // default headers AT CONSTRUCTION, so a later call would not re-tag a
    // client that already exists.
    // Unconditional, not `#[cfg]`-guarded: this crate's `opencompany`
    // dependency enables `mcp` (and so `openhuman`) outright, so a desktop
    // build without the harness is not a shape that exists. If that dependency
    // line ever loses the feature, these two lines stop compiling — which is
    // the failure worth having, since the alternative is a bundle that boots
    // fine and cannot think.
    tracing::info!(
        "{}",
        opencompany::app::journal::pin_keyring(instance.journal()).summary()
    );
    opencompany::product::install_into_embedded_core();

    let config = AppConfig {
        bind: "127.0.0.1:0".to_string(),
        // The `[workspace]` section of the root's `config.toml`, resolved by
        // `prepare_instance`. Not layout — these two are the knobs every
        // company builder reads — but they come from the same file, and `serve`
        // sets them from it. A desktop that skipped them ran with the
        // compiled-in defaults and silently ignored the operator's config.
        workspace_quota: instance.workspace().quota,
        workspace_git_enabled: instance.workspace().git_enabled,
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
        // launch. This host builds its config by hand rather than through
        // `AppConfig::load`, so that layer reaches it only here.
        auth_mode_override: Some(
            instance
                .auth_mode()
                .unwrap_or(opencompany::app::config::AuthMode::None),
        ),
        ..AppConfig::default()
    };
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
        .with_rebuilder(std::sync::Arc::new(opencompany::desktop::DesktopRebuilder));
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

    let (address, serving) = opencompany::server::bind("127.0.0.1:0", state).await?;
    let server = tokio::spawn(async move {
        if let Err(error) = serving.run().await {
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
    })
}

#[cfg(test)]
mod test {
    use super::*;

    /// Issue #632, end to end: a packaged install must be enterable with no
    /// terminal, no mail server and no platform credential.
    ///
    /// It used to be enterable by *signing in*: the shell asked for a magic
    /// link at a synthetic loopback mailbox, read the code back out of the
    /// response, and redeemed it for a cookie. That is gone. The desktop runs
    /// [`AuthMode::None`], where the person at the machine is the principal by
    /// configuration — so the console's very first request is already
    /// authenticated and there is no screen in front of it.
    ///
    /// The change is worth stating as more than a simplification: the shell had
    /// no working session carrier for that cookie in the first place. The proxy
    /// client holds no cookie store, `x-opencompany-session` is stripped as a
    /// reserved header, and `needsCarriedSession()` is false on desktop — so the
    /// session the magic link minted was discarded the moment it arrived, and
    /// what actually let the console through was that every request came from
    /// loopback anyway.
    ///
    /// Over HTTP rather than against the registry, because the point is the
    /// request the console makes: the company has to exist *and* be reachable
    /// through the sole-company alias the console addresses before it knows any
    /// id, and the principal has to resolve with nothing presented. Asserting a
    /// company was registered would prove neither.
    ///
    /// Seeded explicitly through [`start`], because a launched install no longer
    /// seeds — it opens the wizard instead (`local::start_at`). What is under
    /// test here is the host once it *has* a company, which is where a launched
    /// install arrives the moment setup completes.
    #[tokio::test]
    async fn a_host_with_a_company_opens_with_no_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let host = start(dir.path().to_path_buf()).await.expect("host starts");
        let base = host.base_url();
        let http = reqwest::Client::new();

        assert_eq!(
            host.companies().len(),
            1,
            "the seeding entry point registers exactly one starter company"
        );

        // Where the console starts, and where it used to stop with a 401.
        let listed = http
            .get(format!("{base}/api/v1/companies"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            listed.status(),
            200,
            "an unauthenticated loopback request is the owner's, by configuration"
        );
        let companies: serde_json::Value = listed.json().await.unwrap();
        assert_eq!(
            companies.as_array().map(Vec::len),
            Some(1),
            "and it sees the company: {companies}"
        );

        // Attributed to a real stored record rather than a principal invented
        // per request — chat, task assignment and the audit trail all key off
        // `UserRecord::id`. Asked through the sole-company alias, which is what
        // the console addresses before it has discovered any id.
        let me: serde_json::Value = http
            .get(format!("{base}/api/v1/company/auth/me"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(me["email"], "local:owner", "{me}");
        assert_eq!(
            me["role"], "admin",
            "the write plane has to work, and there is nobody to outrank: {me}"
        );

        // And no second way in survives beside it. A host that still answered
        // `auth/request` would be one where the mode had not actually reached
        // the company — the seeded manifest names no mode of its own.
        let requested = http
            .post(format!("{base}/api/v1/company/auth/request"))
            .json(&serde_json::json!({ "email": "someone@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            requested.status(),
            409,
            "a magic link is refused by mode, not answered with a silent 202"
        );
    }

    /// A `none`-mode desktop is the **default**, not a ceiling.
    ///
    /// The setup wizard offers all three modes on a loopback host, and an
    /// operator who wants to share their instance with a colleague can pick
    /// `email` — it writes `auth_mode` to the root's `config.toml` and applies
    /// it live. But this host builds its `AppConfig` by hand rather than through
    /// `AppConfig::load`, so a mode forced in the literal above would be a mode
    /// the file can never win against: the choice would hold until quit and
    /// silently revert on the next launch, which is precisely the "configuration
    /// ignored" failure the setup surface exists to prevent.
    ///
    /// So the file is read and `none` is what it falls back to.
    #[tokio::test]
    async fn a_configured_sign_in_survives_a_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "auth_mode = \"email\"\n").unwrap();

        let host = start(dir.path().to_path_buf()).await.expect("host starts");
        let requested = reqwest::Client::new()
            .post(format!("{}/api/v1/company/auth/request", host.base_url()))
            .json(&serde_json::json!({ "email": "ada@example.com" }))
            .send()
            .await
            .unwrap();

        assert_ne!(
            requested.status(),
            409,
            "409 is `auth_mode` refusing the route by mode — the file said email"
        );
    }

    /// The starter company is seeded once, not per launch.
    #[tokio::test]
    async fn a_relaunch_reuses_the_company_the_root_already_holds() {
        let dir = tempfile::tempdir().unwrap();
        let first = start(dir.path().to_path_buf()).await.unwrap();
        let seeded = first.companies().to_vec();
        drop(first);

        // `take_root` retries because the data root is released asynchronously;
        // see the note in `stopping_a_host_frees_its_root_and_its_port`.
        let relaunched = take_root(dir.path().to_path_buf()).await;
        assert_eq!(relaunched.companies(), seeded.as_slice());
    }

    #[tokio::test]
    async fn an_embedded_host_answers_on_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let host = start(dir.path().to_path_buf()).await.expect("host starts");

        assert!(host.address().ip().is_loopback(), "must never be routable");
        assert_ne!(
            host.address().port(),
            0,
            "the OS-chosen port must be reported"
        );

        let health = reqwest::get(format!("{}/healthz", host.base_url()))
            .await
            .expect("the reported address is reachable");
        assert!(health.status().is_success());
    }

    #[tokio::test]
    async fn the_embedded_host_holds_its_data_root() {
        // The desktop being launched twice is ordinary rather than exceptional,
        // and two hosts over one root overwrite each other's companies.
        let dir = tempfile::tempdir().unwrap();
        let _first = start(dir.path().to_path_buf())
            .await
            .expect("the first starts");

        let second = start(dir.path().to_path_buf()).await;
        assert!(
            second.is_err(),
            "a second host over one root must be refused"
        );
    }

    #[tokio::test]
    async fn two_embedded_hosts_over_different_roots_coexist() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let first = start(a.path().to_path_buf()).await.unwrap();
        let second = start(b.path().to_path_buf()).await.unwrap();
        assert_ne!(first.address().port(), second.address().port());
    }

    #[tokio::test]
    async fn stopping_a_host_frees_its_root_and_its_port() {
        let dir = tempfile::tempdir().unwrap();
        let host = start(dir.path().to_path_buf()).await.unwrap();
        drop(host);

        // Both resources come back: a desktop restarted after a clean quit must
        // start, and it must not leak a listener per restart.
        //
        // Retried briefly, and the reason is worth recording rather than
        // hiding behind a sleep. `flock` belongs to the *open file
        // description*, and between `fork()` and `exec()` a concurrently
        // spawned child shares every descriptor its parent had. So if anything
        // else in this process spawns a subprocess in the same instant this
        // host releases its root — and the suite does, constantly: `git` in the
        // worktree tests, `python3` in the ACP ones — the lock survives until
        // that child reaches `exec` and `O_CLOEXEC` closes it. Microseconds,
        // and reproducible here about one run in five.
        //
        // Not worth engineering away: the production shape is a person quitting
        // and relaunching seconds later, and a harness the desktop spawned
        // always execs. Asserting instantaneous release would be asserting
        // something stricter than the product needs, so this asserts what it
        // does need — that the root comes back promptly.
        let mut last = None;
        for _ in 0..50 {
            match start(dir.path().to_path_buf()).await {
                Ok(host) => {
                    assert_ne!(host.address().port(), 0);
                    return;
                }
                Err(error) => last = Some(error),
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("a released root must become takeable: {last:?}");
    }

    /// The property the console keys its connection list on (#615).
    ///
    /// The port deliberately changes on every launch, so a client that
    /// recognises this host by address recognises a *new* host every run and
    /// the dead ones pile up in its sidebar. The identity is what survives
    /// exactly that restart, and this is the assertion the fix rests on.
    ///
    /// The complementary half — that the port does *not* survive — is left
    /// unasserted on purpose: the OS is free to hand the same ephemeral port
    /// back, so `assert_ne!` on it would be asserting a coincidence. Two
    /// concurrent hosts differing is covered by
    /// `two_embedded_hosts_over_different_roots_coexist`.
    #[tokio::test]
    async fn a_restarted_host_keeps_its_identity() {
        let dir = tempfile::tempdir().unwrap();
        let first = start(dir.path().to_path_buf()).await.unwrap();
        let id = first.instance_id().to_string();
        assert!(!id.is_empty(), "a host must report an identity");
        drop(first);

        let second = take_root(dir.path().to_path_buf()).await;
        assert_eq!(second.instance_id(), id, "the same root is the same host");
    }

    /// Starts over `root`, retrying while a just-released `flock` clears.
    ///
    /// See `stopping_a_host_frees_its_root_and_its_port` for why the release is
    /// not instantaneous.
    async fn take_root(root: PathBuf) -> EmbeddedHost {
        let mut last = None;
        for _ in 0..50 {
            match start(root.clone()).await {
                Ok(host) => return host,
                Err(error) => last = Some(error),
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("a released root must become takeable: {last:?}");
    }
}

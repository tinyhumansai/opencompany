//! The OpenCompany desktop shell.
//!
//! Three responsibilities, and nothing else:
//!
//! - **[`proxy`]** — every host's HTTP and event traffic, in Rust so that CORS
//!   does not apply and the credential never enters the webview.
//! - **[`embedded`]** — a host running in this process, for someone with no
//!   server to point at.
//! - **[`local`]** — the roster of those hosts, so one machine can run several
//!   companies side by side rather than exactly one.
//! - **[`ssh`]** — tunnels to hosts on *other* machines that are bound to
//!   loopback there, which is the one connector a browser cannot have.
//! - **[`keychain`]** — where a paired device's token lives, so the webview
//!   holds a handle and never the secret.
//! - **[`commands`]** — the thin Tauri surface over all three.
//!
//! The console itself is unchanged: it is the same `frontend/` bundle the web
//! deployment serves, and it reaches all of the above through the `Transport`
//! seam it already had.

pub mod acp;
pub mod commands;
pub mod embedded;
/// Who is sitting at this machine, as the OS already knows — read once, to
/// prefill a profile nobody has filled in yet. See the module docs for why it is
/// a suggestion and never an import.
pub mod identity;
pub mod keychain;
pub mod local;
pub mod proxy;
pub mod ssh;

use std::path::PathBuf;

use crate::local::LocalHosts;
use crate::ssh::SshTunnels;

/// Process-wide state the commands read.
pub struct AppHandleState {
    pub data_dir: PathBuf,
    /// Every host this machine runs, and which of them are listening.
    ///
    /// A roster rather than an `Option<EmbeddedHost>`: an operator running two
    /// companies on one laptop is the ordinary case this shell is for, and a
    /// single-valued field is exactly what makes the second one impossible.
    /// Behind a mutex because starting and stopping are operator actions
    /// arriving on command threads, not just a boot-time read.
    ///
    /// An instance that could not start is a row carrying its reason — most
    /// often another process holding its data root — not a reason to refuse to
    /// launch. The desktop also talks to *remote* hosts, and a busy root must
    /// not stop it doing that.
    pub local: tokio::sync::Mutex<LocalHosts>,
    /// Every SSH tunnel this application is holding open.
    ///
    /// Beside the local roster rather than inside it: both are processes this
    /// shell starts and must be able to stop, and neither is a host it can
    /// merely address. What they are not is the same thing — a tunnel's host
    /// belongs to somebody else's machine — so pruning one against the other
    /// would delete it.
    pub ssh: tokio::sync::Mutex<SshTunnels>,
}

/// The canonical directory this instance keeps its data in.
///
/// This must remain identical to the host binary's resolution: an explicit
/// `OPENCOMPANY_DATA_DIR`, otherwise `$HOME/.opencompany` (or `%USERPROFILE%`
/// on Windows), with a relative `.opencompany` only when no home is available.
pub fn default_data_dir() -> PathBuf {
    opencompany::app::config::data_dir_from_env()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // The `tinyagents::observability` directive is the vendored durable-append
    // writer's reporting target, and it has to be named explicitly here for a
    // reason the host binary's filter does not share: this fallback carries no
    // global directive at all, only per-target ones, so an unnamed target is
    // dropped at *every* level — `error` included. Without this the writer's
    // "still failing" reminders, its "recovered, N observation(s) lost" summary
    // and its "never recovered before shutdown" summary are silent, and so is
    // the first-failure `error` line. See `DEFAULT_LOG_FILTER` in
    // `src/bin/opencompany.rs` for the full argument (issue #450). Latent while
    // this crate does not enable the `openhuman` feature; a landmine for
    // whoever does.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "opencompany_desktop_lib=info,opencompany=info,tinyagents::observability=warn"
                    .into()
            }),
        )
        .init();

    let data_dir = default_data_dir();

    // Tauri's own runtime, entered before the webview, because the local hosts
    // have to be listening before the console asks for their addresses.
    //
    // Deliberately *not* a `tokio::runtime::Runtime` built here. The hosts
    // started at boot must live on the same runtime as the ones an operator
    // starts later from a command — and commands run on Tauri's. Two runtimes
    // would mean a `start` awaited from a command while the boot-time hosts'
    // server tasks belong to a runtime nothing else holds a handle to.
    let local = tauri::async_runtime::block_on(LocalHosts::load(data_dir.clone()));

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(proxy::SharedProxy::default())
        .manage(AppHandleState {
            data_dir,
            local: tokio::sync::Mutex::new(local),
            ssh: tokio::sync::Mutex::new(SshTunnels::default()),
        })
        .invoke_handler(tauri::generate_handler![
            commands::oc_connect,
            commands::oc_pair_device,
            commands::oc_adopt_session,
            commands::oc_forget_device,
            commands::oc_disconnect,
            commands::oc_connections,
            commands::oc_request,
            commands::oc_subscribe,
            commands::oc_embedded,
            commands::oc_device_identity,
            commands::oc_local_instances,
            commands::oc_create_local_instance,
            commands::oc_start_local_instance,
            commands::oc_stop_local_instance,
            commands::oc_rename_local_instance,
            commands::oc_forget_local_instance,
            commands::oc_delete_local_instance,
            commands::oc_acp_harnesses,
            commands::oc_acp_confirm_harness,
            commands::oc_acp_install_harness,
            commands::oc_open_ssh_tunnel,
            commands::oc_close_ssh_tunnel,
            commands::oc_ssh_tunnels,
        ])
        .run(tauri::generate_context!())
        .expect("run the desktop shell");
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn desktop_defaults_to_dot_opencompany_under_home() {
        const CHILD: &str = "OPENCOMPANY_DESKTOP_DATA_DIR_TEST_CHILD";
        let home = PathBuf::from("/opencompany-test-home");

        if std::env::var_os(CHILD).is_some() {
            assert_eq!(default_data_dir(), home.join(".opencompany"));
            return;
        }

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("test::desktop_defaults_to_dot_opencompany_under_home")
            .env(CHILD, "1")
            .env("HOME", &home)
            .env_remove("USERPROFILE")
            .env_remove("OPENCOMPANY_DATA_DIR")
            .status()
            .unwrap();

        assert!(status.success());
    }
}

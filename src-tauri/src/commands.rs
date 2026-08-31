//! The `#[tauri::command]` surface the console calls.
//!
//! Thin by design: every one of these delegates to [`crate::proxy`] or
//! [`crate::embedded`], which are plain Rust and testable without a webview.
//! Logic that lives in a command is logic that can only be exercised by
//! starting a GUI.
//!
//! **Every command takes an explicit `connection_id`.** None of them reads an
//! "active connection" from application state — that single-valued field is
//! exactly what stops block/buzz from holding more than one workspace at a
//! time, and a command that defaulted it would reintroduce the limit invisibly.

use tauri::State;
use tauri::ipc::Channel;

use crate::local::LocalInstanceInfo;
use crate::proxy::{
    Connection, Credential, ProxyRequest, ProxyResponse, SharedProxy, may_carry_a_credential,
};

/// What the console needs to construct a connection record.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedInfo {
    pub base_url: String,
    pub data_dir: String,
    /// Who is answering there, as opposed to where.
    ///
    /// Carried because `base_url` holds an ephemeral port and so cannot be an
    /// identity: keyed on the address, the console reads every launch as a
    /// first meeting and leaves the previous launch's row behind, dead (#615).
    pub instance_id: String,
}

/// Registers (or re-registers) a host this client talks to.
///
/// **Takes no device token.** The console cannot supply one, because it has
/// never seen one: a paired device's session is resolved from the keychain by
/// `connection_id`. That is the difference between "the webview does not
/// normally hold the secret" and "the webview cannot hold the secret", and only
/// the second survives a script injected into rendered agent markdown.
#[tauri::command]
pub async fn oc_connect(
    proxy: State<'_, SharedProxy>,
    connection_id: String,
    base_url: String,
    platform_token: Option<String>,
) -> Result<(), String> {
    // Device first: a paired device is a *person* on this machine, and the
    // journal records their name. A platform bearer is a machine credential
    // that writes anonymously, so preferring it would silently un-attribute
    // every write the desktop makes.
    let credential = match (
        crate::keychain::device_session(&connection_id),
        platform_token,
    ) {
        (Some(session), _) => Credential::Device(session),
        (None, Some(token)) => Credential::Platform(token),
        (None, None) => Credential::None,
    };
    proxy
        .upsert(
            connection_id,
            Connection {
                base_url,
                credential,
            },
        )
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn oc_disconnect(
    proxy: State<'_, SharedProxy>,
    connection_id: String,
) -> Result<(), String> {
    proxy.remove(&connection_id).await;
    Ok(())
}

#[tauri::command]
pub async fn oc_connections(proxy: State<'_, SharedProxy>) -> Result<Vec<String>, String> {
    Ok(proxy.ids().await)
}

/// One HTTP request against a named connection.
#[tauri::command]
pub async fn oc_request(
    proxy: State<'_, SharedProxy>,
    connection_id: String,
    request: ProxyRequest,
) -> Result<ProxyResponse, String> {
    proxy
        .request(&connection_id, request)
        .await
        .map_err(|error| error.to_string())
}

/// Subscribes to a connection's event stream, pushing payloads down `channel`.
///
/// One channel per subscription rather than one shared bus: a chatty company's
/// turn events must not be able to starve another connection's, and dropping
/// the channel is how the console unsubscribes.
#[tauri::command]
pub async fn oc_subscribe(
    proxy: State<'_, SharedProxy>,
    connection_id: String,
    path: String,
    channel: Channel<String>,
) -> Result<(), String> {
    let proxy = proxy.inner().clone();
    tokio::spawn(async move {
        let result = proxy
            .subscribe(&connection_id, &path, |event| {
                // A send failure means the console dropped the channel, i.e.
                // unsubscribed. Not an error worth reporting.
                let _ = channel.send(event);
            })
            .await;
        if let Err(error) = result {
            tracing::debug!(%error, "event stream ended");
        }
    });
    Ok(())
}

/// What the console learns after pairing. Carries no secret.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    pub company: String,
    pub device_id: String,
    pub expires_at_millis: u64,
}

/// What the host answers a claim with. The token half never leaves this module.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimedDevice {
    token: String,
    company: String,
    device_id: String,
    expires_at_millis: u64,
}

/// Redeems a pairing code against a host, and returns what it answered.
///
/// Split out of [`oc_pair_device`] so it can be tested at all: a command takes
/// `State<'_, SharedProxy>`, which needs a Tauri application to construct, and
/// the module note above says why that matters — logic reachable only by
/// starting a GUI is logic nothing checks. Everything here is the part with a
/// rule in it, and the command below is the keychain write and the
/// re-registration around it.
///
/// **The one exchange in which a session token is created rather than
/// replayed.** The pairing code goes out in the request and the token comes
/// back in the response body, so this is where an unencrypted wire costs the
/// most — and it is not covered by `ProxyRegistry::upsert`, because it never
/// goes through the registry (#731).
async fn claim(base_url: &str, code: &str, label: Option<&str>) -> Result<ClaimedDevice, String> {
    if !may_carry_a_credential(base_url) {
        // The console shows this verbatim (`device-pairing.tsx`), so it is
        // written for the person reading it rather than for a log.
        return Err(format!(
            "{base_url} is not encrypted, so pairing would send this device's session in the clear. Use https, or a host on this machine."
        ));
    }
    let base = base_url.trim_end_matches('/');
    let response = reqwest::Client::builder()
        // As `ProxyRegistry`'s client does, and here for a sharper reason: the
        // default policy follows up to ten redirects, and a 307 from an https
        // base to an http one re-sends this request — pairing code and all —
        // over the wire the check above just refused. A check on the first url
        // is worth nothing if the client will walk to a second.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())?
        .post(format!("{base}/api/v1/devices/claim"))
        .json(&serde_json::json!({ "code": code, "label": label }))
        .send()
        .await
        .map_err(|error| error.to_string())?;

    if !response.status().is_success() {
        // The host's own wording, which is deliberately one indistinguishable
        // message for every way a claim can fail. Passing it through keeps that
        // property instead of inventing a more specific one here.
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| format!("pairing failed with {status}"));
        return Err(message);
    }

    response.json().await.map_err(|error| error.to_string())
}

/// Redeems a pairing code, keeping the session token out of the webview.
///
/// The whole flow lives in Rust for one reason: the token exists for exactly
/// one HTTP response, and the console must not be on the path it takes. So this
/// command performs the claim, writes the result to the keychain, re-registers
/// the connection with the resolved credential, and returns only what a person
/// needs to see — which company, which device, how long it lasts.
///
/// Deliberately does its own request rather than going through
/// `ProxyRegistry::request`: this runs *before* the connection has a credential
/// worth attaching, and routing it through the proxy would mean a code path
/// where the claim response body — the one place a raw token appears — passes
/// through the same machinery that serialises bodies back to the webview.
///
/// Which is also why the transport rule has to be repeated here. Doing its own
/// request means doing its own checking: the registry never sees this url, so
/// `upsert`'s refusal does not cover the one exchange where the token is not
/// merely replayed but *handed over* — the code goes out in the request and the
/// session comes back in the response, both in the clear on a plain-HTTP host.
#[tauri::command]
pub async fn oc_pair_device(
    proxy: State<'_, SharedProxy>,
    connection_id: String,
    base_url: String,
    code: String,
    label: Option<String>,
) -> Result<PairedDevice, String> {
    let claimed = claim(&base_url, &code, label.as_deref()).await?;
    // `<company>.<token>` is the header carrier's form, and the only form
    // anything downstream needs.
    crate::keychain::remember_device(
        &connection_id,
        &format!("{}.{}", claimed.company, claimed.token),
    )
    .map_err(|error| error.to_string())?;

    // Re-register so the credential takes effect without waiting for a reload.
    // The console cannot do this itself — it has nothing to pass.
    //
    // The session is read back from the keychain rather than reused from the
    // claim: what matters is what the store will hand out on the *next* boot,
    // so a write that did not survive surfaces here rather than as a mysterious
    // 401 later. A miss is `Credential::None`, never `Device("")` — an empty
    // session header is a credential that authenticates as nobody while looking
    // like one to every check that only asks whether a device is paired.
    if let Ok(base_url) = proxy.base_url(&connection_id).await {
        let credential = match crate::keychain::device_session(&connection_id) {
            Some(session) => Credential::Device(session),
            None => Credential::None,
        };
        // Infallible in practice: `base_url` was just read back out of the
        // registry, so it is one `upsert` already accepted. Surfaced rather
        // than swallowed anyway — a pairing that reported success while the
        // credential never took effect is the worst of the three outcomes.
        proxy
            .upsert(
                connection_id.clone(),
                Connection {
                    base_url,
                    credential,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
    }

    Ok(PairedDevice {
        company: claimed.company,
        device_id: claimed.device_id,
        expires_at_millis: claimed.expires_at_millis,
    })
}

/// Adopts a session a sign-in just returned, as this connection's credential.
///
/// The desktop's sign-in flow asks the host for the header carrier — there is
/// no cookie jar behind [`oc_request`], so the cookie the host would otherwise
/// set has nowhere to live — and the readable session it gets back is handed
/// here rather than kept in the webview. That is the property the proxy's
/// `RESERVED_HEADERS` exists to hold: the page never holds a credential, so a
/// script injected into rendered agent markdown cannot exfiltrate one, and it
/// cannot choose what a request authenticates as either, because the proxy
/// attaches the credential itself.
///
/// This is `oc_pair_device` minus the pairing ceremony. A sign-in's session and
/// a paired device's are the same thing to every layer below: the host renders
/// both as `<company>.<token>`, the keychain stores both under the connection
/// id, and [`Credential::Device`] carries both in the same header. The claim
/// step is the only difference, and the sign-in already did its own.
#[tauri::command]
pub async fn oc_adopt_session(
    proxy: State<'_, SharedProxy>,
    connection_id: String,
    session: String,
) -> Result<(), String> {
    adopt_session(&proxy, connection_id, session).await
}

/// The body of [`oc_adopt_session`], off the `State` extractor so a test can
/// drive it against a bare registry.
///
/// Ordered so that nothing durable happens until everything refusable has been
/// refused, and everything durable is undone when a later step fails anyway:
///
/// 1. The connection is looked up first — adopting a session for a connection
///    the core does not hold stores nothing.
/// 2. The transport gate runs BEFORE the keychain write. `upsert` would refuse
///    the credential afterwards, but by then the keychain would hold a session
///    the next launch's `oc_connect` dutifully presents — and its `upsert`
///    refuses the whole registration, leaving the connection unusable until
///    someone finds the hidden keychain entry. The precheck is the same
///    function `upsert` consults, so the two cannot disagree.
/// 3. A read-back miss is an error, not a quiet `Credential::None`. The
///    read-back exists to surface a write that did not survive; installing
///    nothing and reporting success would have the console record a credential
///    while every request runs anonymous — the exact silence this command was
///    added to end.
/// 4. An `upsert` refusal rolls the keychain entry back, best-effort, for the
///    same reason as (2): a credential the registry refused must not ambush
///    the next launch.
pub(crate) async fn adopt_session(
    proxy: &crate::proxy::ProxyRegistry,
    connection_id: String,
    session: String,
) -> Result<(), String> {
    // Refused before anything is stored: a session that authenticates as
    // nobody must not survive into the keychain, where the next launch would
    // dutifully present it and read the host's 401 as a revoked sign-in.
    if session.trim().is_empty() {
        return Err("a sign-in session cannot be empty".to_string());
    }
    let base_url = proxy
        .base_url(&connection_id)
        .await
        .map_err(|error| error.to_string())?;
    if !may_carry_a_credential(&base_url) {
        // The registry's own words for this refusal, so the sign-in screen and
        // a failed registration name the problem identically.
        return Err(crate::proxy::ProxyError::InsecureBaseUrl(base_url).to_string());
    }

    crate::keychain::remember_device(&connection_id, &session)
        .map_err(|error| error.to_string())?;

    // Read back from the store rather than reused from the argument: what
    // matters is what the store will hand out on the *next* boot, so a write
    // that did not survive surfaces here — as a failure, with the entry
    // removed — rather than as a mysterious 401 later.
    let Some(stored) = crate::keychain::device_session(&connection_id) else {
        let _ = crate::keychain::forget_device(&connection_id);
        return Err(
            "the session was stored but could not be read back from the keychain".to_string(),
        );
    };
    proxy
        .upsert(
            connection_id.clone(),
            Connection {
                base_url,
                credential: Credential::Device(stored),
            },
        )
        .await
        .map_err(|error| {
            // Best-effort: the refusal is the error worth reporting, and a
            // failed tidy-up must not replace it.
            let _ = crate::keychain::forget_device(&connection_id);
            error.to_string()
        })
}

/// Forgets this machine's stored session for a connection.
///
/// Local only. The session record on the host outlives it — revoking that is
/// the operator's action from the devices list, and doing both here would mean
/// removing a row from one machine silently cut off another.
#[tauri::command]
pub async fn oc_forget_device(
    proxy: State<'_, SharedProxy>,
    connection_id: String,
) -> Result<(), String> {
    crate::keychain::forget_device(&connection_id).map_err(|error| error.to_string())?;
    if let Ok(base_url) = proxy.base_url(&connection_id).await {
        // As in `oc_pair_device`: a url the registry already accepted.
        proxy
            .upsert(
                connection_id,
                Connection {
                    base_url,
                    credential: Credential::None,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Where the host rooted at the data dir is listening, if it is running.
///
/// Kept alongside [`oc_local_instances`], which supersedes it, because the two
/// halves of this application ship independently: a `pnpm dev` console built
/// before the roster existed calls only this, and a shell built before it
/// answers only this. Both degrade to the single-instance behaviour instead of
/// to an unhandled `no such command`.
#[tauri::command]
pub async fn oc_embedded(
    state: State<'_, crate::AppHandleState>,
) -> Result<Option<EmbeddedInfo>, String> {
    let local = state.local.lock().await;
    Ok(local.default_instance().and_then(|instance| {
        Some(EmbeddedInfo {
            base_url: instance.base_url?,
            data_dir: instance.data_dir,
            instance_id: instance.instance_id?,
        })
    }))
}

/// Every host this machine runs, listening or not.
///
/// The listing is the whole surface: creating, starting and stopping all
/// answer with the affected instance, and the console re-reads this rather
/// than keeping its own idea of the roster. One source of truth, on the side
/// that actually holds the sockets.
#[tauri::command]
pub async fn oc_local_instances(
    state: State<'_, crate::AppHandleState>,
) -> Result<Vec<LocalInstanceInfo>, String> {
    Ok(state.local.lock().await.list())
}

/// Adds a host over a fresh data root on this machine, and starts it.
///
/// Its own root, never a second process over an existing one: two hosts over
/// one root overwrite each other's companies, which is why `prepare_instance`
/// locks it in the first place.
#[tauri::command]
pub async fn oc_create_local_instance(
    state: State<'_, crate::AppHandleState>,
    label: String,
) -> Result<LocalInstanceInfo, String> {
    state.local.lock().await.create(&label).await
}

#[tauri::command]
pub async fn oc_start_local_instance(
    state: State<'_, crate::AppHandleState>,
    id: String,
) -> Result<LocalInstanceInfo, String> {
    state.local.lock().await.start(&id).await
}

/// Stops a host, freeing its port and — the part that matters — its data root,
/// so a terminal `opencompany serve` can take it.
#[tauri::command]
pub async fn oc_stop_local_instance(
    state: State<'_, crate::AppHandleState>,
    id: String,
) -> Result<LocalInstanceInfo, String> {
    state.local.lock().await.stop(&id)
}

#[tauri::command]
pub async fn oc_rename_local_instance(
    state: State<'_, crate::AppHandleState>,
    id: String,
    label: String,
) -> Result<LocalInstanceInfo, String> {
    state.local.lock().await.rename(&id, &label)
}

/// Opens a tunnel to a host on another machine, and answers with the loopback
/// address the console should use for it.
///
/// Idempotent per target: asking for a host that is already tunnelled hands
/// back the tunnel that is up. The console reopens every remembered `ssh`
/// connection at launch, and a second call must not mean a second child.
#[tauri::command]
pub async fn oc_open_ssh_tunnel(
    state: State<'_, crate::AppHandleState>,
    target: crate::ssh::SshTarget,
) -> Result<crate::ssh::SshTunnelInfo, String> {
    state.ssh.lock().await.open(target).await
}

/// Closes the tunnel to a target. Not an error when there is none — the
/// console closes on removal, and removal can arrive twice.
#[tauri::command]
pub async fn oc_close_ssh_tunnel(
    state: State<'_, crate::AppHandleState>,
    target: crate::ssh::SshTarget,
) -> Result<(), String> {
    state.ssh.lock().await.close(&target).await;
    Ok(())
}

/// Every tunnel, and which of them stopped forwarding.
///
/// The roster the console re-reads rather than keeping its own copy of, for
/// the same reason [`oc_local_instances`] is: one source of truth, on the side
/// that actually holds the processes.
#[tauri::command]
pub async fn oc_ssh_tunnels(
    state: State<'_, crate::AppHandleState>,
) -> Result<Vec<crate::ssh::SshTunnelInfo>, String> {
    Ok(state.ssh.lock().await.list())
}

/// Drops a host from the roster. **Leaves its data on disk** — see
/// [`crate::local::LocalHosts::forget`].
#[tauri::command]
pub async fn oc_forget_local_instance(
    state: State<'_, crate::AppHandleState>,
    id: String,
) -> Result<(), String> {
    let mut local = state.local.lock().await;
    // Stopping first is what makes the removal complete: a forgotten instance
    // whose host kept listening would hold its root against the terminal, and
    // stay reachable from a console row nothing lists any more.
    let _ = local.stop(&id);
    local.forget(&id)
}

/// Permanently deletes a desktop-created host and everything in its data root.
///
/// This is intentionally distinct from [`oc_forget_local_instance`], whose
/// recoverable contract leaves the data root intact.
#[tauri::command]
pub async fn oc_delete_local_instance(
    state: State<'_, crate::AppHandleState>,
    id: String,
) -> Result<(), String> {
    state.local.lock().await.delete(&id).await
}

/// Every coding harness this shell knows how to drive over ACP, and what the
/// **filesystem** says about each right now.
///
/// Takes no state and no connection id: unlike everything else in this file,
/// readiness is a property of *this machine*, not of a host it talks to.
///
/// Answers nothing on its own. Every harness comes back `checking`, because
/// nothing short of running the adapter can say whether it is installed,
/// working, and signed in — and this call runs nothing. It exists to paint the
/// list; [`oc_acp_confirm_harness`] is what settles each row.
#[tauri::command]
pub fn oc_acp_harnesses() -> Vec<crate::acp::discovery::HarnessStatus> {
    crate::acp::discovery::survey()
}

/// Actually starts one harness, resolving its `checking` state to `ready` or
/// `spawnFailed` — and returning the models it advertises.
///
/// Both answers come from one spawn because they come from the same call:
/// `session/new` is where an adapter both proves it can open a session and
/// lists what it can run. Asking twice would spawn twice for no more
/// information.
///
/// Split from [`oc_acp_harnesses`] rather than folded into it so the list
/// paints immediately and each row settles on its own: one slow CLI must not
/// hold up the others, and the operator sees "Checking…" rather than an empty
/// pane. Safe to call concurrently for every harness.
///
/// The subprocess is killed when the probe's client drops, so nothing is left
/// running whether it succeeded, failed, or timed out.
#[tauri::command]
pub async fn oc_acp_confirm_harness(
    state: tauri::State<'_, crate::AppHandleState>,
    id: String,
) -> Result<crate::acp::discovery::ConfirmedHarness, String> {
    // A dedicated empty directory, not the data root itself.
    //
    // Still stable and ordinary — an agent that inspects its working directory
    // on startup sees a real place, and nothing is left behind to clean up —
    // but no longer the root holding every company's journal, ledger and
    // derived state. These CLIs read their working directory on startup
    // looking for project configuration and repository markers, and pointing
    // one at the whole data root hands it that surface for no benefit the
    // probe actually needs.
    let cwd = state.data_dir.join("acp-probe");
    if let Err(error) = std::fs::create_dir_all(&cwd) {
        // Not fatal: the probe only needs *a* directory. Falling back keeps a
        // read-only or full disk from turning every harness into "won't start"
        // when the real answer has nothing to do with the harness.
        tracing::debug!(%error, "could not create the ACP probe directory");
        return Ok(crate::acp::discovery::confirm(&id, &state.data_dir).await);
    }
    Ok(crate::acp::discovery::confirm(&id, &cwd).await)
}

/// Installs (or updates) the ACP adapter this app owns for one harness.
///
/// The adapter is *our* dependency, not the operator's: they installed Claude
/// Code, and `@agentclientprotocol/claude-agent-acp` is the piece that makes it
/// speak this protocol. So the app fetches it, into its own directory, at the
/// version this build pins — never into the operator's global npm prefix.
///
/// **Explicit, never automatic.** It is a network fetch that writes
/// executables, and doing that unannounced on launch is not something an app
/// should decide for someone. The console offers a button; this is what the
/// button calls.
///
/// One install per harness at a time. Two concurrent `npm install --prefix`
/// runs against the same directory interleave their writes, and the failure
/// that produces is a half-populated `node_modules` that reads as a corrupt
/// install rather than as a collision.
#[tauri::command]
pub async fn oc_acp_install_harness(id: String) -> Result<(), String> {
    use tokio::sync::Mutex;

    /// Serialises **every** install, not one per harness.
    ///
    /// Both adapters install into the same `npm --prefix` root, so two
    /// concurrent `npm install` runs there interleave their writes to one
    /// `node_modules` and one lockfile. An earlier version of this guarded
    /// per-id so Claude's install would not block Codex's — which is exactly
    /// the case that corrupts the tree, since those are the two that share the
    /// prefix. The wait is seconds and the button is per-row, so the cost of
    /// serialising is a queue nobody notices.
    ///
    /// A `tokio::sync::Mutex` rather than the `std` one because it is held
    /// across an await. The guard also removes the need to un-register an id
    /// by hand: a cancelled or panicking install drops the guard and releases
    /// the lock, where the previous insert/remove pair leaked the id forever
    /// and made every later attempt report "already running".
    static INSTALLING: Mutex<()> = Mutex::const_new(());

    let harness = crate::acp::discovery::HARNESSES
        .iter()
        .find(|h| h.id == id)
        .ok_or_else(|| format!("`{id}` is not a harness this build knows"))?;

    let _guard = INSTALLING.lock().await;
    crate::acp::tools::install(harness).await
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// The guarantee the console cannot make about itself.
    ///
    /// `pairDevice` in the console returns whatever the core sends, so a mock
    /// there proves nothing — this is where "the token never reaches the
    /// webview" is actually enforced, by a type that has nowhere to put one.
    /// If a `token` field is ever added to `PairedDevice`, this fails.
    #[test]
    fn a_paired_device_carries_no_token() {
        let wire = serde_json::to_value(PairedDevice {
            company: "acme".into(),
            device_id: "dev-1".into(),
            expires_at_millis: 1,
        })
        .expect("serialise");

        // Sorted, for the same reason as the instance row below: the closed set
        // is the claim. `PairedDevice`'s field order happens to be alphabetical
        // today, so an ordered comparison passes by coincidence rather than by
        // design — and would go red the moment a field is inserted out of that
        // position, for a reason that has nothing to do with what this test is
        // about.
        let mut keys: Vec<&str> = wire
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["company", "deviceId", "expiresAtMillis"],
            "pairing must answer with these three fields and nothing else"
        );
        assert!(!wire.to_string().to_lowercase().contains("token"));
    }

    /// The keys the console reads off an instance row, by name.
    ///
    /// Same argument as `the_embedded_record_answers_in_the_keys_the_console_reads`:
    /// nothing type-checks a Rust struct against the TypeScript that reads it,
    /// and every optional field degrades silently. A renamed key lands as "the
    /// instance list is full of blank rows", not as an error.
    #[test]
    fn an_instance_row_answers_in_the_keys_the_console_reads() {
        let wire = serde_json::to_value(LocalInstanceInfo {
            id: "acme".into(),
            label: "Acme".into(),
            data_dir: "/data/instances/acme".into(),
            running: true,
            base_url: Some("http://127.0.0.1:1234".into()),
            instance_id: Some("inst-1".into()),
            companies: vec!["acme".into()],
            error: None,
        })
        .expect("serialise");

        // Sorted before comparing, because the set is what this asserts and the
        // order is not. This crate inherits `serde_json`'s `preserve_order`
        // through its path dependency on `opencompany` (root `Cargo.toml:86`),
        // so a JSON object is backed by an `IndexMap` and emits **struct field
        // order**, not alphabetical order. Pinning the order here asserted a
        // property nothing needs — JSON object order means nothing to the
        // TypeScript that reads these by name — and it would break again the
        // next time a field is added in the middle of the struct.
        let mut keys: Vec<&str> = wire
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "baseUrl",
                "companies",
                "dataDir",
                "id",
                "instanceId",
                "label",
                "running",
            ],
            "the instance row answers in exactly these keys: {wire}"
        );
    }

    /// A stopped row carries no address, so the console cannot render one that
    /// would fail its probe forever.
    #[test]
    fn a_stopped_instance_carries_no_address() {
        let wire = serde_json::to_value(LocalInstanceInfo {
            id: "acme".into(),
            label: "Acme".into(),
            data_dir: "/data/instances/acme".into(),
            running: false,
            base_url: None,
            instance_id: None,
            companies: Vec::new(),
            error: Some("the data root is in use".into()),
        })
        .expect("serialise");

        let object = wire.as_object().expect("an object");
        assert!(!object.contains_key("baseUrl"));
        assert_eq!(object["error"], "the data root is in use");
        assert_eq!(object["running"], false);
    }

    /// A one-shot host that answers every request with `head`, then closes.
    ///
    /// Returns its base url and a handle that says whether anything ever
    /// connected — which is the assertion for a refusal that must happen
    /// *before* the wire, not on the answer that comes back over it.
    async fn host(head: &'static str) -> (String, Arc<AtomicBool>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let reached = Arc::new(AtomicBool::new(false));
        let flag = reached.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                flag.store(true, Ordering::SeqCst);
                use tokio::io::AsyncWriteExt as _;
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}"), reached)
    }

    /// The claim is refused before a socket is opened, not after an answer.
    ///
    /// A token that travelled once has been read; there is no recovering from
    /// it by rejecting the response. So this asserts on the connection, not on
    /// the `Err` — the message alone would pass on a version that sent the
    /// pairing code first and complained afterwards (#731).
    #[tokio::test]
    async fn pairing_over_an_unencrypted_remote_host_sends_nothing() {
        // A real listener, addressed by a name that is not loopback. The
        // resolver never runs, because the refusal comes first — which is the
        // point.
        let (_, reached) = host("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
        for base in [
            "http://192.168.1.20:8080",
            "http://acme.example.com",
            "http://10.0.0.4:8080",
        ] {
            // `let else` rather than `expect_err`, which would need
            // `ClaimedDevice: Debug` — and a `token` field behind a `{:?}` is
            // the thing the type is shaped to prevent.
            let Err(error) = claim(base, "code-123", None).await else {
                panic!("{base} must not be paired with");
            };
            assert!(
                error.contains("not encrypted"),
                "{base} must be refused for the reason it is refused: {error}"
            );
        }
        assert!(
            !reached.load(Ordering::SeqCst),
            "nothing may be sent to a host the rule refuses"
        );
    }

    /// Loopback still pairs — the embedded host is reached no other way.
    #[tokio::test]
    async fn pairing_with_a_host_on_this_machine_still_works() {
        let body = r#"{"token":"t","company":"acme","deviceId":"dev-1","expiresAtMillis":1}"#;
        let head: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (base, reached) = host(head).await;

        let claimed = claim(&base, "code-123", Some("a laptop"))
            .await
            .expect("a loopback host pairs");

        assert!(reached.load(Ordering::SeqCst));
        assert_eq!(claimed.company, "acme");
        assert_eq!(claimed.device_id, "dev-1");
    }

    /// A redirect is not followed, so an https base cannot be walked to http.
    ///
    /// `reqwest`'s default policy follows up to ten, and a 307 re-sends the
    /// body — so a host answering `307 → http://…` would put the pairing code
    /// on exactly the wire the check above refuses, having passed it. Checking
    /// the first url is worth nothing if the client will walk to a second.
    #[tokio::test]
    async fn a_redirect_away_from_the_checked_host_is_not_followed() {
        let (base, _) = host(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://192.168.1.20:8080/api/v1/devices/claim\r\ncontent-length: 0\r\n\r\n",
        )
        .await;

        let Err(error) = claim(&base, "code-123", None).await else {
            panic!("a redirect is an answer, not a detour to follow");
        };
        // The host's status, passed through — which is what "not followed"
        // looks like from here.
        assert!(error.contains("307"), "{error}");
    }

    /// The console reads these keys by name, and a rename here is silent on
    /// both sides: TypeScript has nothing to check a Rust struct against, and
    /// every field is optional in the console precisely so an older shell
    /// degrades instead of failing. A wrong `instanceId` therefore lands as a
    /// sidebar accumulating one dead connection per launch (#615), not as an
    /// error anybody sees.
    ///
    /// The set is deliberately *shrinking* here: `operatorEmail` left with the
    /// desktop's sign-in. A console built before that still reads it as
    /// optional and simply finds nothing, which is the same degrade an older
    /// shell has always got from the other direction.
    #[test]
    fn the_embedded_record_answers_in_the_keys_the_console_reads() {
        let wire = serde_json::to_value(EmbeddedInfo {
            base_url: "http://127.0.0.1:1234".into(),
            data_dir: "/data".into(),
            instance_id: "inst-1".into(),
        })
        .expect("serialise");

        // Sorted, as above. `EmbeddedInfo`'s field order is simultaneously
        // struct order and alphabetical, which is why this test passed either
        // way and why it could never have established the precedent the
        // instance-row test cited it for.
        let mut keys: Vec<&str> = wire
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["baseUrl", "dataDir", "instanceId"],
            "the embedded record answers in exactly these keys: {wire}"
        );
    }
}

/// Who is sitting at this machine, as the operating system already knows.
///
/// A **suggestion** for a profile nobody has filled in yet — see
/// [`crate::identity`] for why it is read rather than imported. Every field is
/// optional and a machine that knows nothing answers an empty record, which the
/// console reads as "ask them to type it".
///
/// Takes no `connection_id`, unlike every other command here: it is a fact about
/// this computer, not about a host — the same answer whichever workspace the
/// person is looking at.
#[tauri::command]
pub async fn oc_device_identity() -> Result<crate::identity::DeviceIdentity, String> {
    Ok(crate::identity::device_identity())
}

#[cfg(test)]
mod adopt_session_tests {
    use super::adopt_session;
    use crate::proxy::{Connection, Credential, ProxyRegistry};

    async fn registry_with(id: &str, base_url: &str) -> ProxyRegistry {
        let proxy = ProxyRegistry::new();
        proxy
            .upsert(
                id.to_string(),
                Connection {
                    base_url: base_url.to_string(),
                    credential: Credential::None,
                },
            )
            .await
            .expect("a bare registration is always accepted");
        proxy
    }

    /// The bricking sequence issue #1858's review named: a plain-HTTP remote
    /// host's sign-in must be refused BEFORE the keychain write, because a
    /// stored session the next launch's `oc_connect` presents makes `upsert`
    /// refuse the whole registration — a connection unusable until someone
    /// finds the hidden keychain entry.
    #[tokio::test]
    async fn an_insecure_host_is_refused_before_anything_is_stored() {
        let proxy = registry_with("insecure-1", "http://192.168.1.20:8080").await;

        let result = adopt_session(&proxy, "insecure-1".to_string(), "acme.tok".to_string()).await;

        let error = result.expect_err("a credential must not ride plain HTTP off-machine");
        assert!(error.contains("not encrypted"), "{error}");
        assert!(
            crate::keychain::device_session("insecure-1").is_none(),
            "nothing may survive into the keychain for the next launch to trip over"
        );
    }

    #[tokio::test]
    async fn an_unknown_connection_stores_nothing() {
        let proxy = ProxyRegistry::new();

        let result = adopt_session(&proxy, "nobody-1".to_string(), "acme.tok".to_string()).await;

        assert!(result.is_err());
        assert!(crate::keychain::device_session("nobody-1").is_none());
    }

    #[tokio::test]
    async fn an_empty_session_is_refused_outright() {
        let proxy = registry_with("empty-1", "https://acme.example.com").await;

        let result = adopt_session(&proxy, "empty-1".to_string(), "   ".to_string()).await;

        assert!(result.is_err());
        assert!(crate::keychain::device_session("empty-1").is_none());
    }

    /// The happy path, on the transports a credential may ride: https anywhere,
    /// and plain HTTP only to this machine (the embedded host's own case).
    #[tokio::test]
    async fn a_session_is_kept_where_a_credential_may_travel() {
        for (id, base_url) in [
            ("kept-https", "https://acme.example.com"),
            ("kept-local", "http://127.0.0.1:8080"),
        ] {
            let proxy = registry_with(id, base_url).await;

            adopt_session(&proxy, id.to_string(), "acme.tok".to_string())
                .await
                .expect("a securely-reachable host keeps its sign-in");

            assert_eq!(
                crate::keychain::device_session(id).as_deref(),
                Some("acme.tok"),
                "the next launch's oc_connect reads this back"
            );
        }
    }
}

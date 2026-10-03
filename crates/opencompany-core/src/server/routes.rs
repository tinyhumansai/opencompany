use std::path::PathBuf;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tokio::net::TcpListener;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::{AppState, Result};

/// Path prefixes owned by the server's API and discovery surfaces. The console
/// SPA fallback must never answer these: a request under one of them either
/// hits a real handler or is genuinely absent (e.g. a feature-gated route in a
/// build without that feature), and absent server routes must keep 404ing so
/// API and external clients can detect an unwired surface instead of receiving
/// the `index.html` shell with a `200`.
///
/// `/a2a` is retired: it was the tiny.place inbound A2A surface, removed with
/// tiny.place. It stays reserved so a peer still probing it gets an honest 404
/// rather than the console shell with a `200`.
const RESERVED_PREFIXES: [&str; 9] = [
    "/api", "/graphql", "/healthz", "/spec", "/tiny", "/a2a", "/hooks", "/oauth",
    // The ACP endpoint. Reserved even in a build that does not mount it: an
    // ACP client probing an older or feature-less host must get a 404, not the
    // console shell with a `200`. A client cannot tell "no ACP here" from
    // "here is your JSON-RPC" if both answer 200 with an HTML body.
    "/acp",
];

/// True when `path` is server-owned and so must 404 rather than fall through to
/// the console shell. That is either a path under a reserved prefix — an exact
/// match (`/spec`) or a sub-path (`/api/v1/...`) — or any `.well-known`
/// discovery URI (RFC 8615). The latter is reserved wherever the segment
/// appears, not just at the root: `/companies/{handle}/.well-known/agent-card.json`
/// was the retired tiny.place Agent Card endpoint, and no route serves any
/// `.well-known` URI today — so a directory client probing one must get a 404,
/// never the SPA masquerading as a discovery document.
fn is_reserved_path(path: &str) -> bool {
    if path.split('/').any(|segment| segment == ".well-known") {
        return true;
    }
    RESERVED_PREFIXES.iter().any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The directory whose built operator console the host serves at `/`, read from
/// `OPENCOMPANY_CONSOLE_DIR`. Returns `None` when the variable is unset, empty,
/// or does not point at an existing directory — in which case the host keeps
/// its historical behavior and 404s on unknown paths (no console fallback).
fn console_dir_from_env() -> Option<PathBuf> {
    let raw = std::env::var_os("OPENCOMPANY_CONSOLE_DIR")?;
    if raw.is_empty() {
        return None;
    }
    let dir = PathBuf::from(raw);
    dir.is_dir().then_some(dir)
}

/// Stamps the cache policy the console's own file naming already implies.
///
/// The bundle is content-hashed: `index.html` names `index-<hash>.js`, and a
/// build that changes the bytes changes the name. That makes the assets safe to
/// keep forever and makes the shell the one file that must never be kept — it
/// is the only thing that knows which hashes are current.
///
/// Without this the shell was served with an `etag` and no `cache-control`, so
/// browsers applied heuristic freshness and held it across a deploy. The held
/// shell asks for chunks the new image no longer contains; those requests fall
/// through to the SPA fallback and answer with `index.html`, so the dynamic
/// import receives HTML, throws, and unmounts the app — a blank page with no
/// error anywhere a user can see it (issue #979).
///
/// Keyed on the response's own content type rather than the request path,
/// because the SPA fallback serves the shell at paths that look like anything
/// at all — including, in the failure above, paths under `/assets/`.
fn cache_console_response(path: &str, mut response: Response) -> Response {
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue};

    let is_html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));

    // `no-cache` still allows the browser to keep the file; it requires a
    // revalidation before reuse, which is what makes a deploy visible on the
    // very next request. `no-store` would be stricter and slower for no gain.
    let policy = if is_html {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(policy));
    // The page shell is an opaque-origin iframe, so its ES module imports send
    // `Origin: null` even though the console and host share a URL origin. The
    // module graph therefore needs explicit CORS permission. These SDK files
    // are only the fixed React/site runtime; the company-authenticated bundle
    // receives the same headers in `ops::pages`.
    if path.starts_with("/pages-sdk/") && !is_html {
        crate::server::ops::pages::apply_page_module_cors_headers(response.headers_mut());
    }
    response
}

/// Builds the Axum router, mounting the operator console at `/` when
/// `OPENCOMPANY_CONSOLE_DIR` is configured.
pub fn router(state: AppState) -> Router {
    router_with_console(state, console_dir_from_env())
}

/// Router builder with an explicit console directory, so tests can inject a
/// temporary console tree without touching process environment.
fn router_with_console(state: AppState, console_dir: Option<PathBuf>) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/healthz/busy", get(busy))
        .route("/opencompany-config.js", get(console_config))
        .route("/spec", get(spec))
        .route("/tiny", get(tiny))
        .merge(crate::server::operator::router())
        .merge(crate::server::ops::router())
        .merge(crate::server::hooks_chargebee::router())
        .merge(crate::server::provision::router())
        .merge(crate::server::setup::router())
        .merge(crate::server::feedback::router())
        .merge(crate::server::feedback_board::router())
        .merge(crate::server::users::router())
        .merge(crate::server::users::admin::router())
        .merge(crate::server::sso::router())
        .merge(crate::server::admin_password::router())
        .merge(crate::server::graphql::router())
        // Unauthenticated TinyHumans key-grant return leg, for a host with no
        // console at its own origin (the desktop). Trust is the parked state.
        .merge(crate::server::hub_link_callback::router());
    #[cfg(feature = "acp")]
    let router = router.merge(crate::server::acp::router());
    // Unauthenticated console MCP OAuth callback (issue #90), only under `mcp`.
    #[cfg(feature = "mcp")]
    let router = router.merge(crate::server::mcp_oauth::router());
    let router = router.with_state(state.clone());

    // Operator console: the lowest-priority fallback. Every real route above —
    // `/api`, `/graphql`, `/spec`, `/tiny`, `/healthz` — is matched first and
    // always wins. Only when nothing else matches does `ServeDir` answer:
    // asset paths (`/assets/app.js`, `/favicon.ico`) serve their file, and any
    // other unknown path (a client-side SPA route) falls through to
    // `index.html` so the React router can take over. Unmatched paths under a
    // reserved server prefix (`/api`, `/a2a`, `/.well-known`, ...) are the one
    // exception: they 404 rather than serve the shell, so a feature-gated,
    // retired, or otherwise absent API/discovery route stays detectable by its
    // callers.
    // When no console dir is configured this is skipped entirely and unknown
    // paths keep 404ing.
    let router = match console_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            let serve = ServeDir::new(dir)
                .append_index_html_on_directories(true)
                .fallback(ServeFile::new(index));
            router.fallback(move |request: Request| {
                let serve = serve.clone();
                async move {
                    if is_reserved_path(request.uri().path()) {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    let path = request.uri().path().to_owned();
                    match serve.oneshot(request).await {
                        Ok(response) => cache_console_response(&path, response.into_response()),
                        Err(err) => match err {},
                    }
                }
            })
        }
        None => router,
    };

    // CORS is off unless origins are configured, which is every same-origin
    // deployment. When it is on, `map_response` cannot see the request, so the
    // origin is captured per-request in a closure instead — cheap, and it keeps
    // this to two small pieces rather than a middleware stack the codebase
    // otherwise has none of.
    // A Sentry transaction per served request, and continuation of a
    // `sentry-trace` header the console sent — so a failed action in the
    // browser and the request that served it are one trace. A no-op unless the
    // operator asked for a sample rate, and absent entirely from a build
    // without the `crash-reporting` feature; see
    // `docs/spec/runtime/crash-reporting.md`.
    let router = crate::observability::instrument_http(router);

    let cors = state.cors().clone();
    if !cors.is_enabled() {
        return router;
    }
    router.layer(axum::middleware::from_fn(
        move |request: Request, next: axum::middleware::Next| {
            let cors = cors.clone();
            async move {
                let headers = request.headers().clone();
                // A preflight never reaches a handler: answer it here.
                if crate::server::cors::is_preflight(request.method())
                    && let Some(response) = cors.preflight(&headers)
                {
                    return response;
                }
                let mut response: Response = next.run(request).await;
                for (name, value) in cors.headers_for(&headers) {
                    response.headers_mut().insert(name, value);
                }
                response.into_response()
            }
        },
    ))
}

/// Supplies the console's optional, public-only runtime configuration.
///
/// The console bundle is shared by every tenant, while an OpenPanel collector
/// is deployment configuration.  Baking its URL into Vite therefore left the
/// browser tracker permanently off in hosted containers: no one populated the
/// `window.OPENCOMPANY_CONFIG` object that its loader requires.  Serve this
/// small script from the host instead.  It intentionally exposes no client
/// secret; browser collection uses OpenPanel's public client id.
async fn console_config() -> Response {
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue};

    // Blank or unset falls back to the TinyHumans collector; the render below
    // still requires a hosted tenant with `OPENCOMPANY_ANALYTICS=on`. Read
    // through `var_os`, not `var`: the latter maps a non-Unicode configured
    // value to the same `Err` as an unset one, which would silently publish
    // the default collector for a value the operator did set but this
    // process cannot read — the same failure `analytics::config::resolve`
    // avoids by reading `ENDPOINT_ENV` through `get_os` and reporting
    // `Silence::UnusableEndpoint` instead of falling back. Passing `None`
    // here reaches the same silent branch in `render_console_config`.
    let endpoint = match std::env::var_os(crate::analytics::config::ENDPOINT_ENV) {
        None => Some(crate::analytics::config::DEFAULT_ENDPOINT.to_string()),
        Some(raw) => match raw.into_string() {
            Err(_) => None,
            Ok(value) if value.trim().is_empty() => {
                Some(crate::analytics::config::DEFAULT_ENDPOINT.to_string())
            }
            Ok(value) => Some(value),
        },
    };
    let body = render_console_config(
        endpoint.as_deref(),
        hosted_deployment(),
        browser_analytics_enabled(),
    );
    let mut response = (
        [(CONTENT_TYPE, "application/javascript; charset=utf-8")],
        body,
    )
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn render_console_config(endpoint: Option<&str>, hosted: bool, analytics_enabled: bool) -> String {
    match (
        hosted && analytics_enabled,
        endpoint.and_then(public_browser_endpoint),
    ) {
        (true, Some(endpoint)) => format!(
            "window.OPENCOMPANY_CONFIG=Object.assign(window.OPENCOMPANY_CONFIG||{{}},{{analytics:true,analyticsEndpoint:{}}});\n",
            serde_json::to_string(&endpoint).expect("endpoint serializes")
        ),
        _ => "window.OPENCOMPANY_CONFIG=window.OPENCOMPANY_CONFIG||{};\n".to_owned(),
    }
}

/// Browser configuration must never turn a host-only credential URL into a
/// public script.  OpenPanel credentials belong in headers, so a URL with
/// userinfo, query parameters, or a fragment is neither needed nor safe here.
fn public_browser_endpoint(endpoint: &str) -> Option<String> {
    let Ok(mut url) = url::Url::parse(endpoint) else {
        return None;
    };
    let safe = matches!(url.scheme(), "https" | "http")
        && url.host().is_some()
        && (url.scheme() == "https" || is_loopback_host(url.host_str()))
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    if !safe {
        return None;
    }

    // The host transport takes the exact ingestion URL (`…/track`), while the
    // browser SDK takes the API *base* and appends `/track` itself. So a
    // `…/track` endpoint hands the browser its parent path: a self-hosted
    // collector behind its bundled Caddy lives at `https://<domain>/api/track`,
    // and cutting that to the bare origin sent every browser event to the
    // dashboard's `/track` instead of the API's. Any other path is not a shape
    // this can reason about, so it is still never serialized into this
    // unauthenticated response and the browser gets the origin alone.
    let base = url
        .path()
        .strip_suffix("/track")
        .or_else(|| url.path().strip_suffix("/track/"))
        .unwrap_or("")
        .to_owned();
    url.set_path(&base);
    Some(url.into())
}

fn is_loopback_host(host: Option<&str>) -> bool {
    host.is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .and_then(|host| host.parse::<std::net::IpAddr>().ok())
                .is_some_and(|address| address.is_loopback())
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn hosted_deployment() -> bool {
    hosted_deployment_from_values(
        std::env::var("OPENCOMPANY_DEPLOYMENT").ok().as_deref(),
        std::env::var("OPENCOMPANY_TENANT_ID").ok().as_deref(),
    )
}

fn hosted_deployment_from_values(deployment: Option<&str>, tenant_id: Option<&str>) -> bool {
    deployment
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("hosted-tenant"))
        || tenant_id.is_some_and(|value| !value.trim().is_empty())
}

fn browser_analytics_enabled() -> bool {
    browser_analytics_enabled_from_value(std::env::var("OPENCOMPANY_ANALYTICS").ok().as_deref())
}

fn browser_analytics_enabled_from_value(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("on"))
}

/// Serves the Axum application on the configured bind address.
pub async fn serve(state: AppState) -> Result<()> {
    // Cloned, not borrowed: `router(state)` below takes the state by value.
    let bind_addr = state.config().bind.clone();
    let (_addr, serving) = bind(&bind_addr, state).await?;
    serving.run().await
}

/// Binds `addr` and reports the address actually bound, alongside the future
/// that serves it.
///
/// Exists for an **ephemeral port**. An embedder — the desktop shell — wants
/// `127.0.0.1:0` so it cannot collide with a dev server or a second app, and
/// then needs to know which port the OS chose in order to point a webview at
/// it. [`serve`] cannot answer that: by the time it is running, the listener is
/// consumed and `config().bind` still says `:0`.
///
/// The two halves come back separately because the caller has to learn the
/// address *before* awaiting the server, which never returns.
pub async fn bind(addr: &str, state: AppState) -> Result<(std::net::SocketAddr, Serving)> {
    // Name the address in the error. A bare `?` surfaces the bind failure as
    // `openhuman process error: Address already in use` — the io::Error `#[from]`
    // arm — which says nothing about *which* address, and points at the wrong
    // subsystem entirely. The configured address may have come from a flag, a
    // variable, or `config.toml` (issue #425), so the value actually honoured is
    // the one piece of context worth carrying. It matters just as much for an
    // embedded host, whose address nobody typed.
    //
    // Deliberately not pre-parsed to a `SocketAddr`: `ToSocketAddrs` resolves
    // hostnames, so `localhost:8080` binds today and must keep binding.
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        crate::error::OpenCompanyError::Config(format!("could not bind `{addr}`: {e}"))
    })?;
    let local = listener.local_addr()?;
    Ok((local, Serving { listener, state }))
}

/// A bound-but-not-yet-serving host. Await [`Serving::run`] to serve it.
#[must_use = "binding a port without serving it accepts no connections"]
pub struct Serving {
    listener: TcpListener,
    state: AppState,
}

impl Serving {
    /// The address this host is listening on.
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Serves until a termination signal arrives (see [`serve_on`]) or the task
    /// is dropped.
    pub async fn run(self) -> Result<()> {
        serve_on(self.listener, self.state).await
    }
}

/// Serves on a listener the caller already bound, until a termination signal.
///
/// Returns `Ok(())` on a graceful shutdown, so the CLI's `serve` exits `0` on a
/// rollout rather than reporting a failure it did not have. What "graceful"
/// means here — and what it deliberately does not wait for — is spelled out on
/// [`serve_on_until`].
///
/// The one production serving path — `bind`/`serve` and the desktop app's own
/// [`start_local`](crate::desktop::start_local) both end up here, so there is
/// one set of guarantees rather than one that only some servers get. Wired
/// with the connecting peer's socket address (`ConnectInfo`), not just the
/// router: `none` mode's local-owner resolution
/// ([`local_owner`](crate::server::graphql::auth::local_owner)) checks it, and
/// the request's proxy-forwarding headers, as two independent gates alongside
/// the bind-time refusal a `none`-mode company on a routable bind already
/// gets. See `local_owner`'s own doc comment for exactly what each of the
/// three catches — briefly: the bind guard refuses a *declared* routable bind
/// or a declared public URL before the company ever goes live; the peer check
/// catches a directly reachable socket; the header check catches an
/// *undeclared* reverse proxy in front of an otherwise-correctly-loopback-bound
/// host, which the peer check cannot see (the proxy still connects over
/// loopback, so the peer this process observes reads as loopback regardless of
/// where its own caller actually was).
pub async fn serve_on(listener: TcpListener, state: AppState) -> Result<()> {
    serve_on_until(listener, state, crate::server::shutdown::signal()).await
}

/// [`serve_on`] with the termination signal supplied by the caller.
///
/// Exists for tests: a test cannot raise a real `SIGTERM` at its own process
/// without every *other* test in the binary receiving it too, and the thing
/// worth proving here is what happens *after* the signal, not that tokio
/// delivers one.
///
/// ## The shutdown sequence
///
/// On the signal, in order:
///
/// 1. Every registered company stops accepting new cycles and the ones in
///    flight are waited on, bounded by
///    [`shutdown::grace_from_env`](crate::server::shutdown::grace_from_env).
///    The server is deliberately **still serving** through this: the console's
///    event stream is how an operator watches the turn land, and cutting it at
///    the signal would hide the very work the drain exists to preserve. New
///    cycles are refused with `503 Quiescing` in the meantime, which is what
///    "stop accepting new work" means for a host whose work does not arrive on
///    the connection it will be done on.
/// 2. The listener stops accepting and open connections are given
///    [`CONNECTION_GRACE`](crate::server::shutdown::CONNECTION_GRACE) to finish
///    writing.
/// 3. The process returns regardless. This is the ceiling that matters: the
///    console's event stream never ends on its own, so waiting for connections
///    to close on their own terms would hold the pod open until the kubelet's
///    `SIGKILL` — trading a clean exit for the exact abrupt one this is here to
///    remove.
///
/// Step 2's clock starts when the drain *returns*, not at the signal, so an idle
/// host with an open event stream exits in about `CONNECTION_GRACE` rather than
/// sitting out the whole bound. Total time from signal to exit is therefore at
/// most `grace + CONNECTION_GRACE` — the number the pod's
/// `terminationGracePeriodSeconds` has to stay above.
///
/// `/healthz` is untouched. Nothing in this path runs before the signal, and the
/// signal only arrives at the end of a pod's life — the manager's
/// wake-on-request proxy blocks on that endpoint during *boot*, which this
/// cannot reach.
pub async fn serve_on_until<S>(listener: TcpListener, state: AppState, signal: S) -> Result<()>
where
    S: std::future::Future<Output = ()> + Send + 'static,
{
    serve_on_until_with_grace(
        listener,
        state,
        signal,
        crate::server::shutdown::grace_from_env(),
    )
    .await
}

/// [`serve_on_until`] with the drain bound supplied by the caller.
///
/// Split out so a test can prove the ceiling without waiting the real
/// twenty-five seconds for it — and without mutating process environment, which
/// no test can do safely in a binary whose other tests share the process.
pub(crate) async fn serve_on_until_with_grace<S>(
    listener: TcpListener,
    state: AppState,
    signal: S,
    grace: std::time::Duration,
) -> Result<()>
where
    S: std::future::Future<Output = ()> + Send + 'static,
{
    use std::future::IntoFuture;

    let drain_state = state.clone();
    // Starts the ceiling's clock when the *drain* returns, not at the signal.
    //
    // Timing it from the signal instead would make the connection window
    // whatever the drain left over — up to the whole of `grace` on an idle host
    // — so a tenant with nothing in flight but an open event stream would sit
    // there for the full bound before exiting. That is the rollout latency this
    // whole change exists to reduce. Drained-then-two-seconds keeps the worst
    // case identical (`grace` + `CONNECTION_GRACE`, since `drain` is itself
    // bounded by `grace`) while letting the common case go quickly.
    let (drained_tx, drained_rx) = tokio::sync::oneshot::channel::<()>();
    let shutting_down = async move {
        signal.await;
        crate::server::shutdown::arm_force_exit_on_second_signal();
        crate::server::shutdown::drain(&drain_state, grace).await;
        let _ = drained_tx.send(());
    };

    // `into_future` because `WithGracefulShutdown` is `IntoFuture`, not
    // `Future`, and the ceiling below has to race a *pinned* server future.
    let serving = axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutting_down)
    .into_future();
    tokio::pin!(serving);
    // `Err` means the sender was dropped, which can only happen once the serve
    // future is gone — at which point the other arm has already won.
    let ceiling = async {
        if drained_rx.await.is_err() {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(crate::server::shutdown::CONNECTION_GRACE).await;
    };

    tokio::select! {
        served = &mut serving => served?,
        () = ceiling => tracing::warn!(
            "connections were still open {}s after the drain finished; exiting anyway",
            crate::server::shutdown::CONNECTION_GRACE.as_secs()
        ),
    }
    Ok(())
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// Whether this workload is doing anything — the signal the manager consults
/// before scaling the tenant to zero (opencompany-microservice#22).
///
/// The manager measures "idle" by inbound proxied traffic alone, so a company
/// working through a long turn produces none of its own and looks exactly like
/// one nobody has opened. Parking it there destroys the work in flight. This
/// answers the question the manager cannot infer.
///
/// Deliberately a **sibling** of `/healthz` rather than a field on it. The
/// wake-on-request proxy blocks on `/healthz` and gives up after its startup
/// budget, so anything that makes that endpoint slower or heavier directly
/// degrades every cold start — a hard constraint in the issue.
///
/// Asks each company runtime, which combines three sources — the per-company
/// cycle lock, the workflow run supervisor, and the in-flight steer registry.
/// No one of them sees all the work: the first version of this endpoint read
/// only the last and therefore missed the top-level operator chat turn, which
/// is the case #22 actually measured.
///
/// Holds no lock across an await and does no I/O. The cost is a non-blocking
/// `try_lock`, a `RwLock` read to enumerate companies, and one `Mutex`
/// acquisition each — the manager calls this once per idle tenant per scan
/// against a short timeout, so anything that could block would stall the sweep.
///
/// Unauthenticated on purpose. It reveals one boolean about the workload the
/// caller can already reach, and requiring a credential would mean the manager
/// holding a per-tenant secret purely to ask whether to stop it.
async fn busy(State(state): State<AppState>) -> Json<BusyResponse> {
    let busy = state
        .registry()
        .list()
        .into_iter()
        .filter_map(|id| state.registry().get(&id))
        .any(|runtime| runtime.is_busy());
    Json(BusyResponse { busy })
}

async fn spec(State(state): State<AppState>) -> Json<crate::app::AppSpec> {
    Json(state.spec())
}

async fn tiny(State(state): State<AppState>) -> Json<Vec<crate::tiny::RuntimeModuleStatus>> {
    Json(state.spec().runtime_modules)
}

#[derive(Clone, Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct BusyResponse {
    busy: bool,
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "routes_analytics_tests.rs"]
mod tests_analytics;

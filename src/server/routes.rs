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
/// is a tiny.place Agent Card endpoint (feature-gated on `tinyplace`), and the
/// SPA must never masquerade as one for a directory client probing it.
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
        .merge(crate::server::graphql::router());
    #[cfg(feature = "acp")]
    let router = router.merge(crate::server::acp::router());
    // tiny.place A2A inbound + discovery routes, only when the feature is on.
    #[cfg(feature = "tinyplace")]
    let router = router.merge(crate::server::a2a::router());
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
    // exception: they 404 rather than serve the shell, so a feature-gated or
    // otherwise absent API/discovery route stays detectable by its callers.
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
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::*;
    use crate::AppConfig;

    /// Writes a minimal console tree — the shell plus one hashed asset, which
    /// is the shape the cache policy distinguishes between.
    fn console_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.html"),
            "<!doctype html><title>console</title>",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::write(
            dir.path().join("assets").join("index-abc123.js"),
            "export const x = 1;\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("pages-sdk")).unwrap();
        std::fs::write(
            dir.path().join("pages-sdk").join("react.mjs"),
            "export const createElement = () => null;\n",
        )
        .unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    fn cache_control(response: &axum::response::Response) -> &str {
        response
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .expect("the console fallback stamps a cache policy on every response")
            .to_str()
            .unwrap()
    }

    async fn body_text(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn console_serves_index_at_root() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("<title>console</title>"));
    }

    #[tokio::test]
    async fn console_falls_back_to_index_for_spa_routes() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/some/spa/route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("<title>console</title>"));
    }

    #[tokio::test]
    async fn the_shell_is_never_kept_without_revalidating() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        for path in ["/", "/some/spa/route"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(cache_control(&response), "no-cache", "for {path}");
        }
    }

    #[tokio::test]
    async fn a_hashed_asset_is_kept_forever() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/assets/index-abc123.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            cache_control(&response),
            "public, max-age=31536000, immutable"
        );
    }

    #[tokio::test]
    async fn page_sdk_modules_allow_credentialed_imports_from_opaque_frames() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/pages-sdk/react.mjs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "null"
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .unwrap(),
            "true"
        );
        assert_eq!(
            response.headers().get(axum::http::header::VARY).unwrap(),
            "Origin"
        );
    }

    /// The failure that made issue #979 a blank page rather than a 404: a
    /// stale shell asks for a chunk this build does not have, the SPA fallback
    /// answers with `index.html`, and the browser is handed HTML where it
    /// expected a module. Whatever else that response is, it must not be
    /// cached as though it were the immutable asset it was addressed as —
    /// which is why the policy is keyed on what came back, not what was asked
    /// for.
    #[tokio::test]
    async fn a_missing_chunk_answers_with_the_shell_and_is_not_kept() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/assets/KnowledgeGraph-fromlastbuild.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(cache_control(&response), "no-cache");
        assert!(body_text(response).await.contains("<title>console</title>"));
    }

    #[tokio::test]
    async fn console_does_not_shadow_api_routes() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        // A real API route still reaches the API and answers with its own auth
        // status (401), never the SPA shell.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!body_text(response).await.contains("<title>console</title>"));
    }

    #[tokio::test]
    async fn console_does_not_shadow_unmatched_reserved_paths() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        // An unmatched path under a reserved API/discovery prefix (e.g. a
        // feature-gated route in a build without that feature) must 404, not
        // fall through to the SPA shell, so callers can still detect the
        // surface as unwired.
        for path in [
            "/api/v1/does-not-exist",
            "/.well-known/agent-card.json",
            "/companies/acme/.well-known/agent-card.json",
            // The ACP endpoint in a build that does not mount it. An ACP client
            // probing a host cannot distinguish "no ACP here" from "here is
            // your JSON-RPC" if both answer `200` with an HTML body — it would
            // try to parse the console shell as a protocol response.
            "/acp",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::NOT_FOUND, "path: {path}");
            assert!(!body_text(response).await.contains("<title>console</title>"));
        }
    }

    #[test]
    fn reserved_path_matches_prefixes_and_subpaths_only() {
        assert!(is_reserved_path("/api"));
        assert!(is_reserved_path("/api/v1/companies"));
        // `/hooks/{companyId}/{channel}` inbound webhooks (api.md) — a
        // server-owned namespace, reserved even before the route is wired.
        assert!(is_reserved_path("/hooks/acme/slack"));
        assert!(is_reserved_path("/.well-known/agent-card.json"));
        assert!(is_reserved_path("/a2a/handle"));
        // A `.well-known` discovery URI is reserved wherever the segment sits,
        // including under a company handle the SPA otherwise owns.
        assert!(is_reserved_path(
            "/companies/acme/.well-known/agent-card.json"
        ));
        // A console route that merely shares a prefix substring is not reserved.
        assert!(!is_reserved_path("/apidocs"));
        assert!(!is_reserved_path("/tinyplace-console"));
        // `/companies/{handle}` client-side console routes still fall through.
        assert!(!is_reserved_path("/companies/acme"));
        assert!(!is_reserved_path("/"));
        assert!(!is_reserved_path("/some/spa/route"));
    }

    #[tokio::test]
    async fn root_404s_without_console_dir() {
        let app = router_with_console(AppState::new(AppConfig::default()), None);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// An idle workload reports `busy: false`, so the manager parks it as it
    /// always has.
    ///
    /// Note what this does *not* prove: with an empty registry the handler
    /// iterates nothing and returns `false` without consulting any runtime, so
    /// this cannot fail for an aggregation bug. The direction that matters —
    /// that the endpoint can report `true` — is covered by
    /// `is_busy_sees_every_source_of_work` in `company::runtime`, which
    /// exercises the real signal against a real runtime, and by
    /// `is_busy_fails_closed_on_a_poisoned_run_supervisor` alongside it.
    #[tokio::test]
    async fn busy_is_false_when_nothing_is_running() {
        let app = router(AppState::new(AppConfig::default()));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz/busy")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["busy"], serde_json::Value::Bool(false), "{json}");
    }

    /// The wire shape the manager parses. It reads a top-level boolean `busy`
    /// and treats anything else — a missing field, a rename, a string `"true"` —
    /// as *not busy*, so a change here would silently stop protecting work in
    /// flight rather than fail loudly.
    #[tokio::test]
    async fn busy_responds_with_the_shape_the_manager_parses() {
        let app = router(AppState::new(AppConfig::default()));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz/busy")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("busy")
                .and_then(serde_json::Value::as_bool)
                .is_some(),
            "the manager reads a top-level boolean `busy`; got {json}"
        );
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = router(AppState::new(AppConfig::default()));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn spec_returns_axum_framework() {
        let app = router(AppState::new(AppConfig::default()));

        let response = app
            .oneshot(Request::builder().uri("/spec").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// `/spec` is the handshake a multi-connection client boots from.
    async fn spec_body(state: AppState) -> serde_json::Value {
        let response = router(state)
            .oneshot(Request::builder().uri("/spec").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// The embedded shell asks for `:0` and needs the port back.
    ///
    /// A desktop app cannot hardcode 8080 — it would collide with a dev server
    /// or a second app, and a support case that reads "it works on my machine
    /// unless I have the terminal open" is the result. `serve` cannot answer
    /// which port the OS chose, because by the time it runs the listener is
    /// consumed and `config().bind` still says `:0`.
    #[tokio::test]
    async fn binding_an_ephemeral_port_reports_the_one_actually_bound() {
        let state = AppState::new(AppConfig::default());
        let (addr, serving) = bind("127.0.0.1:0", state)
            .await
            .expect("bind an ephemeral port");

        assert_ne!(addr.port(), 0, "the OS-chosen port must come back");
        assert!(
            addr.ip().is_loopback(),
            "an embedded host must not be routable"
        );

        // The listener really is open: something is accepting on that port
        // before anything is served, which is what lets the caller hand the
        // address to a webview without racing the server task.
        let serve_task = tokio::spawn(serving.run());
        let probe = tokio::net::TcpStream::connect(addr).await;
        assert!(probe.is_ok(), "the reported address must be connectable");
        serve_task.abort();
    }

    #[tokio::test]
    async fn spec_identifies_the_instance_and_its_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());

        let body = spec_body(state.clone()).await;
        let id = body["instance_id"].as_str().expect("an instance id");
        assert_eq!(id.len(), 32, "a 128-bit id, hex-encoded");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

        // Stable across calls, and across a fresh state over the same home —
        // the client's whole reason for reading it.
        assert_eq!(spec_body(state).await["instance_id"], id);
        let restarted = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());
        assert_eq!(spec_body(restarted).await["instance_id"], id);

        let caps = body["capabilities"].as_array().expect("capabilities");
        for expected in ["rest", "graphql", "sse"] {
            assert!(caps.iter().any(|c| c == expected), "missing {expected}");
        }
        assert_eq!(body["storage"], "fs");
        // Unset by default rather than an empty string, so a client can tell
        // "unnamed" from "named the empty string".
        assert!(body.get("display_name").is_none());
    }

    #[tokio::test]
    async fn spec_names_the_build_commit_beside_the_version() {
        // `version` has read `0.1.0` for thousands of commits, so it alone
        // cannot tell an operator which build a host is running. The commit is
        // the same *kind* of fact — identical for every instance compiled from
        // one artifact, and saying nothing about this host — which is why it
        // sits on the unauthenticated handshake where `version` already does.
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());
        let body = spec_body(state).await;

        let commit = body["build_commit"].as_str().expect("a build commit");
        assert_eq!(commit, crate::BUILD_COMMIT);
        assert!(!commit.is_empty(), "an absent stamp must read `unknown`");
        assert!(
            commit
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "{commit:?} is not a sanitized stamp"
        );
        assert_eq!(body["version"], crate::VERSION);
    }

    #[tokio::test]
    async fn two_instances_are_distinguishable() {
        // The multi-connection requirement in one assertion: a client holding
        // two servers must be able to tell them apart even when both are
        // freshly initialised with identical config.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let spec_a =
            spec_body(AppState::new(AppConfig::default()).with_home(a.path().to_path_buf())).await;
        let spec_b =
            spec_body(AppState::new(AppConfig::default()).with_home(b.path().to_path_buf())).await;
        assert_ne!(spec_a["instance_id"], spec_b["instance_id"]);
    }

    #[tokio::test]
    async fn spec_never_leaks_the_storage_location() {
        // `/spec` is unauthenticated. The backend *kind* is useful to a client;
        // a path or a connection string would be a gift to anyone who asks.
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(AppConfig {
            instance_name: Some("prod-eu".to_string()),
            ..AppConfig::default()
        })
        .with_home(dir.path().to_path_buf());

        let body = spec_body(state).await;
        assert_eq!(body["display_name"], "prod-eu");
        let rendered = body.to_string();
        assert!(
            !rendered.contains(&dir.path().display().to_string()),
            "the home path must not appear in /spec: {rendered}"
        );
        assert!(!rendered.contains("mongodb://"), "no connection strings");
    }

    #[tokio::test]
    async fn spec_does_not_disclose_memory_engine_details() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());
        let body = spec_body(state).await;
        // `/spec` is the unauthenticated manager handshake. Memory-provider
        // identity, capability and reachability details belong to the
        // operator-authenticated engine route instead.
        assert!(
            body.get("memory").is_none(),
            "memory leaked through /spec: {body}"
        );
    }
}

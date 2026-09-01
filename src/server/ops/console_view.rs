//! Which console page an operator opened (issue #1739).
//!
//! The one event in this module's set that the **console** raises rather than
//! the host, because the host cannot see it: the console is a single-page app,
//! so moving between pages is a hash change and no request reaches this process
//! at all. Without a route, "which surfaces do operators actually use" is a
//! question the product cannot answer about itself.
//!
//! ## What it deliberately does not accept
//!
//! The **view**, never the hash. `#/chat/dm:ada-1f3k` names a teammate,
//! `#/tasks/<uuid>` names a task, and `#/ledgers/<slug>` names a business
//! record — all of which are the company's content, which no payload here may
//! carry. So the body is one field, the caller sends the routed view alone, and
//! `console_view_slug` folds even that onto a closed list before it can reach an
//! event. A second segment is not accepted, not trimmed, not read.
//!
//! Nothing is stored and nothing is returned: this is a write to the analytics
//! tracker and a `204`. `Tracker::track` is synchronous and infallible, so the
//! request cannot be slowed by, or fail because of, telemetry — and a host with
//! analytics off drops it into the null tracker, which is why the route stays
//! registered in every build rather than living behind the cargo feature.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use serde::Deserialize;

use crate::AppState;
use crate::analytics::Event;
use crate::analytics::console::console_view_slug;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the console-view route fragment.
pub fn router() -> Router<AppState> {
    scoped("/analytics/console-view", post(record_view))
}

/// One routed view, as the console names it.
#[derive(Debug, Deserialize)]
struct ConsoleViewBody {
    /// The routed view (`overview`, `chat`, `tasks`, …) — never the full hash.
    view: String,
}

async fn record_view(
    State(state): State<AppState>,
    // Extracted and dropped: the route exists to be *scoped and authorized*
    // like every other company route, not to read the company. Recording which
    // page an operator opened must not be a way to ask whether a company
    // exists, so the same guard runs here as on a read of its contents.
    _scoped: ScopedCompany,
    axum::Json(body): axum::Json<ConsoleViewBody>,
) -> StatusCode {
    state.analytics().track(Event::ConsoleViewed {
        view: console_view_slug(&body.view),
    });
    StatusCode::NO_CONTENT
}

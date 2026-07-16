//! GraphQL read plane: the single read surface behind every console view.
//!
//! The schema is rooted at a [`Company`](company::CompanyGql) aggregation
//! object so a view fetches everything it needs in one round trip; the only
//! top-level queries are `companies`, `company(id)`, and `skillRegistry`. The
//! [`Schema`] is built **once at startup** ([`build_schema`]) and stored on
//! [`AppState`](crate::AppState); each request injects its resolved
//! [`GqlAuth`](auth::GqlAuth) principal via request data. Mutations and
//! subscriptions are out of scope — REST owns the write plane.

pub mod auth;
pub mod company;
pub mod connections;
pub mod inbox;
pub mod memory_facts;
pub mod pagination;
pub mod skills;
pub mod tasks;
pub mod workflows;
pub mod workspace;

use async_graphql::{Context, EmptyMutation, EmptySubscription, ID, Object, Schema};
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Router, extract::State};

use crate::AppState;
use crate::ports::types::CompanyId;
use auth::{GqlAuth, resolve_claims};
use company::CompanyGql;
use skills::RegistrySkillGql;

/// The concrete schema type stored on [`AppState`].
pub type OcSchema = Schema<QueryRoot, EmptyMutation, EmptySubscription>;

/// Builds the read-plane schema once. It carries no request data; per-request
/// [`AppState`] and [`GqlAuth`] are injected by [`graphql_handler`].
pub fn build_schema() -> OcSchema {
    Schema::build(QueryRoot, EmptyMutation, EmptySubscription).finish()
}

/// The schema's SDL, for snapshot tests and query-authoring against the contract.
pub fn sdl() -> String {
    build_schema().sdl()
}

/// Builds the GraphQL route fragment, merged into the main router.
///
/// `POST /graphql` serves queries; `GET /graphql` serves an embedded GraphiQL
/// explorer for interactive use during development.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/graphql", post(graphql_handler))
        .route("/graphql", get(graphiql))
}

/// The query root: the three top-level entry points into the read plane.
pub struct QueryRoot;

#[Object(name = "Query")]
impl QueryRoot {
    /// Every company visible to the caller: all registered companies for the
    /// operator / platform-scope principal, or just a tenant's own in platform
    /// mode.
    async fn companies(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<CompanyGql>> {
        let state = ctx.data::<AppState>()?;
        let auth = ctx.data::<GqlAuth>()?;
        let mut out = Vec::new();
        for id in auth.visible_companies(state) {
            if let Some(runtime) = state.registry().get(&id) {
                out.push(CompanyGql::new(id, runtime));
            }
        }
        Ok(out)
    }

    /// One company by id, or — when `id` is omitted in single-company mode — the
    /// sole registered company. `null` when no such company is registered.
    async fn company(
        &self,
        ctx: &Context<'_>,
        id: Option<ID>,
    ) -> async_graphql::Result<Option<CompanyGql>> {
        let state = ctx.data::<AppState>()?;
        let auth = ctx.data::<GqlAuth>()?;
        let runtime = match &id {
            Some(id) => state.registry().get(&CompanyId::new(id.as_str())),
            None => state.registry().sole(),
        };
        let Some(runtime) = runtime else {
            return Ok(None);
        };
        let company = runtime.id().clone();
        auth.authorize(state, &company)?;
        Ok(Some(CompanyGql::new(company, runtime)))
    }

    /// The repo-level shared skill registry (`skills/*/SKILL.md`), installable
    /// into any company. Unscoped — the library is the same for every caller.
    async fn skill_registry(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<Vec<RegistrySkillGql>> {
        skills::resolve_registry(ctx).await
    }
}

/// `POST /graphql` — executes a query against the prebuilt schema.
///
/// The schema is built once and lives on [`AppState`]; each request injects a
/// cheap `AppState` clone and the resolved [`GqlAuth`] principal as request
/// data. An unauthenticated request in a guarded mode returns a single
/// `unauthorized` error instead of executing.
async fn graphql_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: GraphQLRequest,
) -> GraphQLResponse {
    let auth = match resolve_claims(&headers, &state) {
        Ok(auth) => auth,
        Err(_) => {
            let err = async_graphql::ServerError::new("unauthorized", None);
            return async_graphql::Response::from_errors(vec![err]).into();
        }
    };
    let request = req.into_inner().data(state.clone()).data(auth);
    state.schema().execute(request).await.into()
}

/// `GET /graphql` — a minimal embedded GraphiQL explorer.
async fn graphiql() -> impl IntoResponse {
    Html(async_graphql::http::graphiql_source("/graphql", None))
}

/// Milliseconds in one UTC day.
const MILLIS_PER_DAY: u64 = 86_400_000;

/// The `(year, month, day)` of an epoch day, via Hinnant's public-domain
/// `civil_from_days`. Kept local so the read plane needs no date dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Formats epoch-millis as an RFC-3339 / ISO-8601 UTC timestamp (second
/// precision), the string form the console's `updatedAt`/`at` fields use.
pub(crate) fn iso8601(at_millis: u64) -> String {
    let (y, m, d) = civil_from_days((at_millis / MILLIS_PER_DAY) as i64);
    let secs = (at_millis % MILLIS_PER_DAY) / 1000;
    let (h, min, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

#[cfg(test)]
mod test;

//! Route-authority matrix for the production ops router.
//!
//! This target asserts authorization verdicts, not handler success. A request
//! admitted past the guard may reach a deliberately absent resource, invalid
//! sentinel, or unwired optional service. `Permitted` therefore means neither
//! the status nor the JSON error code is an authorization refusal.
//!
//! The source gate covers the ops inventory: every production `scoped(...)`
//! call plus the three literal, non-dual ops routes. Lifecycle and approval
//! decisions live outside that inventory and are declared separately because
//! they are known authority defects. Runtime `TRACE` probes pin every method
//! set, and the anonymous column pins route existence.
//!
//! `operator.rs` is a third source, closed the same way the ops inventory is:
//! `source_path_set_equals_the_ops_matrix_path_set` scans its `scoped(...)`
//! calls on their own and asserts that set equals `OPERATOR_AUTHORITY_ROUTES`,
//! separately from the ops directory scan asserted against
//! `OPS_SCOPED_ROUTES` — a route whose registration moves between the two
//! sources without changing its suffix still fails, because each side is
//! checked against its own table rather than a combined one. Its direct
//! `.route(...)` calls (dual-address writes not registered through `scoped`,
//! e.g. chat and approvals) are asserted against `OPERATOR_DIRECT_ROUTES` plus
//! the operator-sourced subset of `EXTERNAL_AUTHORITY_ROUTES` and
//! `OVERLAPPING_EXTERNAL_ROUTES`. A route added to `operator.rs` without a
//! matrix row fails one of those set-equality assertions — the same
//! closed-set guarantee the ops scan gives, not a hand-maintained list a new
//! route could silently miss.
//!
//! Red-proof log (all temporary edits were restored from `/tmp` copies before
//! continuing, and `git diff` was byte-identical to the pre-proof tree):
//! - Removed `clear_policy`'s `require_admin`: the member `DELETE /policy`
//!   cells changed from expected 403 to observed 200 on both address forms.
//! - Added `scoped("/matrix-canary", ...)`: the source gate failed with the
//!   unexpected canonical suffix `/matrix-canary`.
//! - Added `.delete(get_activation)` to `/activation`: the method gate failed
//!   on both forms with runtime `{DELETE, GET}` versus declared `{GET}`.
//! - Duplicated `scoped("/activation", ...)` (an ops-owned suffix) into an
//!   unreferenced `operator.rs` function, leaving `OPS_SCOPED_ROUTES` and
//!   `OPERATOR_AUTHORITY_ROUTES` untouched — the same suffix now surfaces
//!   from both scans, simulating a route whose registration crossed sources
//!   without the matrix following it. The pre-fix union-of-both-scans
//!   comparison stayed green, because the suffix was already present in the
//!   combined actual set from the ops side. The per-source comparison this
//!   file now runs failed as intended: `operator scoped suffix set drifted;
//!   missing=[]; unexpected=["/activation"]`.

#![cfg(feature = "openhuman")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use opencompany::company::CompanyManifest;
use opencompany::ports::types::CompanyId;
use opencompany::runtime::RuntimeBuilder;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

// `server::test_support` is crate-private and `cfg(test)`. Including this owned
// helper source keeps the integration target on the same fixture seam without
// exposing fixed test credentials in production.
pub use opencompany::{AppState, OpenCompanyError, ports, server};
#[path = "../src/server/test_support.rs"]
mod test_support;

const COMPANY: &str = "matrix-acme";
const ABSENT_ID: &str = "__matrix_absent__";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Principal {
    Anonymous,
    Member,
    Admin,
    MustChangePasswordAdmin,
    TenantOwner,
    TenantNonOwner,
    Platform,
}

impl Principal {
    const ALL: [Self; 7] = [
        Self::Anonymous,
        Self::Member,
        Self::Admin,
        Self::MustChangePasswordAdmin,
        Self::TenantOwner,
        Self::TenantNonOwner,
        Self::Platform,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Member => "member",
            Self::Admin => "admin",
            Self::MustChangePasswordAdmin => "must-change-password-admin",
            Self::TenantOwner => "tenant-owner",
            Self::TenantNonOwner => "tenant-non-owner",
            Self::Platform => "platform",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verdict {
    Refused(StatusCode, &'static str),
    Permitted,
    Exact(StatusCode, Option<&'static str>),
}

impl Verdict {
    fn snapshot(self) -> String {
        match self {
            Self::Refused(status, code) => format!("refused:{}:{code}", status.as_u16()),
            Self::Permitted => "permitted".to_string(),
            Self::Exact(status, Some(code)) => format!("exact:{}:{code}", status.as_u16()),
            Self::Exact(status, None) => format!("exact:{}", status.as_u16()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Access {
    Scoped,
    Admin,
    Addressed,
    Person,
    Capability,
    Hmac,
    PublicGone,
    Visible,
}

impl Access {
    const fn expected(self, principal: Principal) -> Verdict {
        use Principal::{
            Admin, Anonymous, Member, MustChangePasswordAdmin, Platform, TenantNonOwner,
            TenantOwner,
        };
        match self {
            Self::Scoped => match principal {
                Anonymous => Verdict::Refused(StatusCode::UNAUTHORIZED, "unauthorized"),
                Member | Admin | TenantOwner | Platform => Verdict::Permitted,
                MustChangePasswordAdmin => {
                    Verdict::Refused(StatusCode::FORBIDDEN, "password_change_required")
                }
                TenantNonOwner => Verdict::Refused(StatusCode::FORBIDDEN, "forbidden"),
            },
            Self::Admin => match principal {
                Anonymous => Verdict::Refused(StatusCode::UNAUTHORIZED, "unauthorized"),
                Member | TenantNonOwner => Verdict::Refused(StatusCode::FORBIDDEN, "forbidden"),
                Admin | TenantOwner | Platform => Verdict::Permitted,
                MustChangePasswordAdmin => {
                    Verdict::Refused(StatusCode::FORBIDDEN, "password_change_required")
                }
            },
            // `company_status` uses CompanyAuth + authorize_address directly.
            Self::Addressed => match principal {
                Anonymous => Verdict::Refused(StatusCode::UNAUTHORIZED, "unauthorized"),
                Member | Admin | TenantOwner | Platform => Verdict::Permitted,
                MustChangePasswordAdmin => {
                    Verdict::Refused(StatusCode::FORBIDDEN, "password_change_required")
                }
                TenantNonOwner => Verdict::Refused(StatusCode::FORBIDDEN, "forbidden"),
            },
            Self::Person => match principal {
                Anonymous | TenantOwner | Platform => {
                    Verdict::Refused(StatusCode::UNAUTHORIZED, "unauthorized")
                }
                Member | Admin => Verdict::Permitted,
                MustChangePasswordAdmin => {
                    Verdict::Refused(StatusCode::FORBIDDEN, "password_change_required")
                }
                TenantNonOwner => Verdict::Refused(StatusCode::FORBIDDEN, "forbidden"),
            },
            Self::Capability => Verdict::Exact(StatusCode::NOT_FOUND, Some("not_found")),
            Self::Hmac => Verdict::Refused(StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::PublicGone => Verdict::Exact(StatusCode::GONE, None),
            // `list_companies`: no address to check ownership against, so it
            // filters the registry down to what each principal may see rather
            // than refusing anyone who is merely authenticated (matches
            // `GqlAuth::visible_companies`'s own doc: a tenant sees only what
            // it owns, never a 403 revealing more exists). `Permitted` here
            // covers a 200 with an empty list exactly as much as a populated
            // one — `check_verdict` only reads status/code, never the body.
            Self::Visible => match principal {
                Anonymous => Verdict::Refused(StatusCode::UNAUTHORIZED, "unauthorized"),
                MustChangePasswordAdmin => {
                    Verdict::Refused(StatusCode::FORBIDDEN, "password_change_required")
                }
                Member | Admin | TenantOwner | TenantNonOwner | Platform => Verdict::Permitted,
            },
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Scoped => "scoped",
            Self::Admin => "admin",
            Self::Addressed => "addressed",
            Self::Person => "person",
            Self::Capability => "capability",
            Self::Hmac => "hmac",
            Self::PublicGone => "public-gone",
            Self::Visible => "visible",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Blast {
    Ordinary,
    Authority,
    Destructive,
    Credential,
}

impl Blast {
    const fn label(self) -> &'static str {
        match self {
            Self::Ordinary => "ordinary",
            Self::Authority => "authority",
            Self::Destructive => "destructive",
            Self::Credential => "credential",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Verb {
    const fn label(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    fn method(self) -> Method {
        Method::from_bytes(self.label().as_bytes()).expect("static matrix method")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Address {
    Dual,
    Exact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Source {
    Ops,
    ExternalAuthority,
    Operator,
}

impl Source {
    const fn label(self) -> &'static str {
        match self {
            Self::Ops => "ops",
            Self::ExternalAuthority => "external",
            Self::Operator => "operator",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Probe {
    Empty,
    Json(&'static str),
    Capability,
    /// A Server-Sent Events endpoint: the body never ends on its own (a live
    /// subscription plus a periodic keep-alive), so a permitted response's
    /// body is never drained — see [`Harness::request`].
    Sse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Wait {
    None,
    BodyAdminFix,
    LedgerFix,
    TeamFix,
    WorkflowFix,
    TempPasswordBoundaryFix,
}

impl Wait {
    const fn label(self) -> &'static str {
        match self {
            Self::None => "-",
            Self::BodyAdminFix => "none-assigned:body-admin-signature",
            Self::LedgerFix => "none-assigned:ledger-authority",
            Self::TeamFix => "none-assigned:team-delete-authority",
            Self::WorkflowFix => "none-assigned:workflow-authority",
            Self::TempPasswordBoundaryFix => "none-assigned:temp-password-boundary",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RedCells {
    None,
    Member,
    TenantOwnerAndPlatform,
    TempPassword,
}

impl RedCells {
    const fn contains(self, principal: Principal) -> bool {
        match self {
            Self::None => false,
            Self::Member => matches!(principal, Principal::Member),
            Self::TenantOwnerAndPlatform => {
                matches!(principal, Principal::TenantOwner | Principal::Platform)
            }
            Self::TempPassword => matches!(principal, Principal::MustChangePasswordAdmin),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Route {
    method: Verb,
    path: &'static str,
    address: Address,
    source: Source,
    access: Access,
    features: &'static [&'static str],
    blast: Blast,
    probe: Probe,
    note: &'static str,
    wait: Wait,
    red_cells: RedCells,
}

macro_rules! r {
    ($method:ident, $path:literal, $access:ident, $blast:ident, $note:literal) => {
        Route {
            method: Verb::$method,
            path: $path,
            address: Address::Dual,
            source: Source::Ops,
            access: Access::$access,
            features: &["openhuman"],
            blast: Blast::$blast,
            probe: Probe::Empty,
            note: $note,
            wait: Wait::None,
            red_cells: RedCells::None,
        }
    };
}

macro_rules! rj {
    ($method:ident, $path:literal, $access:ident, $blast:ident, $body:literal, $note:literal) => {{
        let mut route = r!($method, $path, $access, $blast, $note);
        route.probe = Probe::Json($body);
        route
    }};
}

macro_rules! red {
    ($method:ident, $path:literal, $blast:ident, $wait:ident) => {{
        let mut route = r!($method, $path, Admin, $blast, "");
        route.wait = Wait::$wait;
        route.red_cells = RedCells::Member;
        route
    }};
}

macro_rules! body_admin {
    ($route:expr) => {{
        let mut route = $route;
        route.wait = Wait::BodyAdminFix;
        route.red_cells = RedCells::TenantOwnerAndPlatform;
        route
    }};
}

// Source-derived at upstream/main 54de00102: 185 canonical scoped route-method
// pairs across 143 suffixes. Forty method pairs use `AdminScopedCompany`; the
// earlier count of 28 conflated an older source revision with unique paths.
const OPS_SCOPED_ROUTES: &[Route] = &[
    r!(Get, "/activation", Scoped, Ordinary, ""),
    r!(Get, "/tasks/{task_id}/artifacts", Scoped, Ordinary, ""),
    r!(Post, "/artifacts", Scoped, Ordinary, ""),
    r!(Get, "/artifacts/{artifact_id}", Scoped, Ordinary, ""),
    r!(
        Delete,
        "/artifacts/{artifact_id}",
        Scoped,
        Destructive,
        "Members may remove task artifacts."
    ),
    r!(
        Post,
        "/artifacts/{artifact_id}/versions",
        Scoped,
        Ordinary,
        ""
    ),
    r!(Get, "/artifacts/{artifact_id}/diff", Scoped, Ordinary, ""),
    r!(Post, "/avatars", Scoped, Ordinary, ""),
    r!(Get, "/billing/chargebee", Scoped, Ordinary, ""),
    r!(Put, "/billing/chargebee", Admin, Credential, ""),
    r!(Delete, "/billing/chargebee/key", Admin, Credential, ""),
    r!(Get, "/billing/paypal", Scoped, Ordinary, ""),
    r!(Put, "/billing/paypal", Admin, Credential, ""),
    r!(Delete, "/billing/paypal/key", Admin, Credential, ""),
    r!(Get, "/agents/{agent_id}/budget-pause", Scoped, Ordinary, ""),
    r!(
        Post,
        "/agents/{agent_id}/budget-pause/redeem",
        Scoped,
        Authority,
        "Members may resume an agent after the budget interruption is cleared."
    ),
    r!(Get, "/capabilities", Scoped, Ordinary, ""),
    r!(Get, "/credential", Scoped, Ordinary, ""),
    r!(Put, "/credential", Admin, Credential, ""),
    r!(Post, "/credential/link/start", Admin, Credential, ""),
    r!(Post, "/credential/link/finish", Admin, Credential, ""),
    r!(
        Get,
        "/credential/billing",
        Scoped,
        Credential,
        "Members may read what the company's key has left to spend — the person \
         whose agents stopped mid-afternoon is the one who most needs to see a \
         balance of zero, and nothing here names the credential itself."
    ),
    r!(Put, "/logo", Admin, Authority, ""),
    body_admin!(rj!(Patch, "", Admin, Authority, "{}", "")),
    r!(Get, "/composio", Scoped, Ordinary, ""),
    r!(Put, "/composio/token", Admin, Credential, ""),
    r!(Put, "/composio/api-key", Admin, Credential, ""),
    r!(Post, "/composio/authorize", Admin, Credential, ""),
    r!(Get, "/composio/connections", Scoped, Ordinary, ""),
    r!(
        Delete,
        "/composio/connections/{connection_id}",
        Admin,
        Credential,
        ""
    ),
    r!(
        Put,
        "/composio/connections/{connection_id}/default",
        Admin,
        Credential,
        ""
    ),
    r!(
        Delete,
        "/composio/connections/{connection_id}/default",
        Admin,
        Credential,
        ""
    ),
    r!(Post, "/connections/{provider}/start", Admin, Credential, ""),
    r!(
        Post,
        "/connections/{provider}/disconnect",
        Admin,
        Credential,
        ""
    ),
    r!(Get, "/connections", Scoped, Ordinary, ""),
    r!(Delete, "/deep-trace", Admin, Destructive, ""),
    r!(Delete, "/deep-trace/{run_id}", Admin, Destructive, ""),
    r!(Get, "/domain", Scoped, Ordinary, ""),
    r!(Put, "/domain", Admin, Authority, ""),
    r!(Post, "/domain/verify", Scoped, Ordinary, ""),
    r!(
        Get,
        "/finance/chargebee/invoices",
        Scoped,
        Authority,
        "Members may read Chargebee invoice data."
    ),
    r!(Post, "/finance/chargebee/invoices", Admin, Authority, ""),
    r!(
        Get,
        "/finance/chargebee/invoices/{invoice_id}",
        Scoped,
        Authority,
        "Members may read Chargebee invoice data."
    ),
    r!(
        Get,
        "/finance/chargebee/customers",
        Scoped,
        Authority,
        "Members may read Chargebee customer data."
    ),
    r!(
        Post,
        "/finance/chargebee/test",
        Scoped,
        Credential,
        "Members may probe Chargebee with the stored credential."
    ),
    r!(
        Get,
        "/finance/paypal/balance",
        Scoped,
        Authority,
        "Members may read the PayPal balance."
    ),
    r!(
        Get,
        "/finance/paypal/transactions",
        Scoped,
        Authority,
        "Members may read PayPal transactions."
    ),
    r!(
        Post,
        "/finance/paypal/test",
        Scoped,
        Credential,
        "Members may probe PayPal with the stored credential."
    ),
    r!(Get, "/finances", Scoped, Ordinary, ""),
    r!(Get, "/harnesses", Scoped, Ordinary, ""),
    r!(Get, "/hosting", Scoped, Ordinary, ""),
    r!(Put, "/hosting", Admin, Credential, ""),
    r!(Delete, "/hosting/key", Admin, Credential, ""),
    r!(Get, "/inference", Scoped, Ordinary, ""),
    r!(Put, "/inference", Admin, Credential, ""),
    r!(Delete, "/inference", Admin, Credential, ""),
    r!(Get, "/inference/models", Scoped, Ordinary, ""),
    r!(
        Post,
        "/inference/test",
        Scoped,
        Credential,
        "Members may probe inference with the stored credential."
    ),
    r!(Post, "/inference/restart", Admin, Authority, ""),
    r!(Get, "/ledgers", Scoped, Ordinary, ""),
    red!(Post, "/ledgers", Authority, LedgerFix),
    r!(Get, "/ledgers/{slug}", Scoped, Ordinary, ""),
    red!(Delete, "/ledgers/{slug}", Destructive, LedgerFix),
    r!(Get, "/ledgers/{slug}/rendered", Scoped, Ordinary, ""),
    r!(Post, "/ledgers/{slug}/entries", Scoped, Ordinary, ""),
    r!(Get, "/ledgers/{slug}/entries", Scoped, Ordinary, ""),
    r!(
        Delete,
        "/ledgers/{slug}/entries/{entry_id}",
        Scoped,
        Destructive,
        "Members may delete ledger entries."
    ),
    r!(Get, "/inboxes", Scoped, Ordinary, ""),
    r!(Get, "/inboxes/{key}/messages", Scoped, Ordinary, ""),
    r!(Post, "/inboxes/{key}/read", Scoped, Ordinary, ""),
    r!(Post, "/mcp/servers", Admin, Credential, ""),
    r!(Get, "/mcp/servers", Scoped, Ordinary, ""),
    r!(Put, "/mcp/servers/{name}", Admin, Credential, ""),
    r!(Delete, "/mcp/servers/{name}", Admin, Credential, ""),
    r!(
        Get,
        "/mcp/servers/{name}/tools",
        Scoped,
        Credential,
        "Members may discover tools using the stored server credential."
    ),
    r!(
        Post,
        "/mcp/servers/{name}/test",
        Scoped,
        Credential,
        "Members may connect using the stored server credential."
    ),
    r!(
        Post,
        "/mcp/servers/{name}/oauth/start",
        Admin,
        Credential,
        ""
    ),
    r!(Get, "/mcp/config", Scoped, Ordinary, ""),
    r!(Put, "/mcp/config", Admin, Credential, ""),
    r!(Get, "/mcp/registry/search", Scoped, Ordinary, ""),
    r!(Get, "/mcp/registry/entry", Scoped, Ordinary, ""),
    r!(Post, "/mcp/registry/install", Admin, Credential, ""),
    r!(
        Post,
        "/mcp/registry/{server_id}/connect",
        Admin,
        Credential,
        ""
    ),
    r!(
        Post,
        "/mcp/registry/{server_id}/disconnect",
        Admin,
        Credential,
        ""
    ),
    r!(Put, "/mcp/registry/{server_id}/env", Admin, Credential, ""),
    r!(Delete, "/mcp/registry/{server_id}", Admin, Credential, ""),
    r!(Post, "/memory", Scoped, Ordinary, ""),
    r!(Get, "/memory", Scoped, Ordinary, ""),
    r!(Get, "/memory/traces", Scoped, Ordinary, ""),
    r!(Get, "/memory/stats", Scoped, Ordinary, ""),
    r!(Get, "/memory/archives", Scoped, Ordinary, ""),
    r!(
        Delete,
        "/memory/{fact_id}",
        Scoped,
        Destructive,
        "Members may delete company memory facts."
    ),
    r!(Get, "/memory/engine", Admin, Authority, ""),
    r!(Put, "/memory/engine", Admin, Credential, ""),
    r!(Post, "/memory/engine/test", Admin, Credential, ""),
    r!(Post, "/memory/ingest", Scoped, Ordinary, ""),
    r!(
        Post,
        "/memory/ingest/links",
        Scoped,
        Authority,
        "Members may submit URLs for server-side ingestion."
    ),
    r!(
        Delete,
        "/memory/document/{source}",
        Scoped,
        Destructive,
        "Members may forget an ingested document."
    ),
    r!(Get, "/chat/mentionables", Person, Ordinary, ""),
    r!(Get, "/notifications", Person, Ordinary, ""),
    r!(Put, "/notifications", Person, Ordinary, ""),
    r!(Get, "/pages", Scoped, Ordinary, ""),
    r!(Get, "/pages/{slug}", Scoped, Ordinary, ""),
    Route {
        probe: Probe::Capability,
        access: Access::Capability,
        ..r!(Get, "/pages/{slug}/bootstrap.mjs", Scoped, Ordinary, "")
    },
    Route {
        probe: Probe::Capability,
        access: Access::Capability,
        ..r!(Get, "/pages/{slug}/bundle.mjs", Scoped, Ordinary, "")
    },
    r!(Get, "/policy", Scoped, Ordinary, ""),
    body_admin!(rj!(Put, "/policy", Admin, Authority, "{}", "")),
    body_admin!(r!(Delete, "/policy", Admin, Authority, "")),
    r!(Get, "/presence", Person, Ordinary, ""),
    rj!(
        Put,
        "/presence",
        Person,
        Ordinary,
        r#"{"status":"online"}"#,
        ""
    ),
    r!(Delete, "/presence", Person, Ordinary, ""),
    rj!(
        Post,
        "/chat/typing",
        Person,
        Ordinary,
        r#"{"chatId":"__matrix_absent__"}"#,
        ""
    ),
    r!(Get, "/chat/read-state", Person, Ordinary, ""),
    rj!(
        Put,
        "/chat/read-state",
        Person,
        Ordinary,
        r#"{"channelId":"__matrix_absent__","lastReadAt":0}"#,
        ""
    ),
    r!(Get, "/runs", Scoped, Ordinary, ""),
    r!(Get, "/runs/{run_id}", Scoped, Ordinary, ""),
    r!(Get, "/search", Scoped, Ordinary, ""),
    r!(Put, "/search", Admin, Credential, ""),
    r!(Delete, "/search/key", Admin, Credential, ""),
    r!(Post, "/setup/roster", Scoped, Ordinary, ""),
    r!(Post, "/skills/{slug}/install", Admin, Authority, ""),
    r!(Post, "/skills/{slug}/uninstall", Admin, Destructive, ""),
    r!(Get, "/skills/registry", Scoped, Ordinary, ""),
    r!(Put, "/skills/{slug}", Admin, Authority, ""),
    r!(Post, "/skills", Admin, Authority, ""),
    r!(Get, "/skills", Scoped, Ordinary, ""),
    r!(Get, "/smtp", Scoped, Ordinary, ""),
    r!(Put, "/smtp", Admin, Credential, ""),
    r!(Post, "/smtp/test", Admin, Credential, ""),
    r!(Get, "/tasks/{task_id}/export", Scoped, Ordinary, ""),
    r!(Post, "/tasks", Scoped, Ordinary, ""),
    r!(Get, "/tasks", Scoped, Ordinary, ""),
    r!(Get, "/tasks/inflight", Scoped, Ordinary, ""),
    r!(Get, "/tasks/{task_id}", Scoped, Ordinary, ""),
    r!(Patch, "/tasks/{task_id}", Scoped, Ordinary, ""),
    r!(
        Delete,
        "/tasks/{task_id}",
        Scoped,
        Destructive,
        "Members may delete task cards."
    ),
    r!(
        Post,
        "/tasks/{task_id}/steer",
        Scoped,
        Authority,
        "Members may steer or cancel active task work."
    ),
    r!(
        Post,
        "/tasks/{task_id}/workflow-proposal/apply",
        Scoped,
        Authority,
        "Members may accept a task-authored workflow proposal."
    ),
    r!(
        Post,
        "/tasks/{task_id}/workflow-proposal/reject",
        Scoped,
        Authority,
        "Members may reject a task-authored workflow proposal."
    ),
    r!(Post, "/tasks/{task_id}/discussion", Scoped, Ordinary, ""),
    r!(
        Delete,
        "/tasks/{task_id}/discussion/{seq}",
        Scoped,
        Destructive,
        "Members may redact task discussion messages."
    ),
    r!(Get, "/team", Scoped, Ordinary, ""),
    r!(
        Post,
        "/team",
        Scoped,
        Authority,
        "Members may add teammates unless privileged budget or tool fields are requested."
    ),
    r!(Get, "/team/{agent_id}", Scoped, Ordinary, ""),
    r!(
        Patch,
        "/team/{agent_id}",
        Scoped,
        Authority,
        "Members may edit non-sensitive teammate fields; tools, model, and harness require admin."
    ),
    red!(Delete, "/team/{agent_id}", Destructive, TeamFix),
    r!(Post, "/team/draft", Scoped, Ordinary, ""),
    r!(Post, "/team/design", Scoped, Ordinary, ""),
    r!(Post, "/team/{agent_id}/draft", Scoped, Ordinary, ""),
    r!(
        Put,
        "/team/{agent_id}/inbox",
        Scoped,
        Authority,
        "Members may enable or disable a teammate inbox."
    ),
    body_admin!(rj!(
        Put,
        "/team/{agent_id}/budget",
        Admin,
        Authority,
        r#"{"budgetUsdDaily":null}"#,
        ""
    )),
    body_admin!(r!(Delete, "/team/{agent_id}/budget", Admin, Authority, "")),
    r!(Get, "/tools/catalog", Scoped, Ordinary, ""),
    r!(Get, "/tools/grants", Scoped, Ordinary, ""),
    body_admin!(rj!(
        Put,
        "/tools/grants",
        Admin,
        Authority,
        r#"{"namespace":"__matrix_absent__"}"#,
        ""
    )),
    body_admin!(r!(Delete, "/tools/grants", Admin, Authority, "")),
    r!(Get, "/usage", Scoped, Ordinary, ""),
    red!(Post, "/workflows", Authority, WorkflowFix),
    r!(Get, "/workflows", Scoped, Ordinary, ""),
    r!(Get, "/workflows/runs", Scoped, Ordinary, ""),
    r!(Post, "/workflows/cron/preview", Scoped, Ordinary, ""),
    r!(Post, "/workflows/validate", Scoped, Ordinary, ""),
    r!(
        Post,
        "/workflows/draft-from-description",
        Scoped,
        Ordinary,
        ""
    ),
    r!(Get, "/workflows/tool-slugs", Scoped, Ordinary, ""),
    r!(Get, "/workflows/wired-channels", Scoped, Ordinary, ""),
    r!(
        Post,
        "/workflows/runs/{rid}/cancel",
        Scoped,
        Destructive,
        "Members may cancel an active workflow run."
    ),
    r!(Get, "/workflows/runs/{rid}/output", Scoped, Ordinary, ""),
    r!(Get, "/workflows/runs/{rid}/artifacts", Scoped, Ordinary, ""),
    r!(Get, "/workflows/{wid}", Scoped, Ordinary, ""),
    red!(Put, "/workflows/{wid}", Authority, WorkflowFix),
    red!(Delete, "/workflows/{wid}", Destructive, WorkflowFix),
    red!(Post, "/workflows/{wid}/run", Authority, WorkflowFix),
    r!(Post, "/workflows/{wid}/fix-from-run", Scoped, Ordinary, ""),
    r!(
        Put,
        "/workflows/{wid}/enabled",
        Scoped,
        Authority,
        "Members may enable or disable workflow scheduling."
    ),
    r!(Get, "/workflows/{wid}/revisions", Scoped, Ordinary, ""),
    r!(
        Post,
        "/workflows/{wid}/revisions/{rev}/restore",
        Scoped,
        Authority,
        "Members may restore workflow revisions."
    ),
    r!(Post, "/workspace", Scoped, Ordinary, ""),
    r!(Get, "/workspace", Scoped, Ordinary, ""),
    r!(Put, "/workspace/file/{node_id}", Scoped, Ordinary, ""),
    r!(Get, "/workspace/file/{node_id}", Scoped, Ordinary, ""),
    r!(Get, "/workspace/search", Scoped, Ordinary, ""),
    r!(
        Post,
        "/workspace/sweep-empty-agent-folders",
        Scoped,
        Destructive,
        "Members may remove empty agent folders."
    ),
    r!(
        Post,
        "/workspace/merge-duplicate-folders",
        Scoped,
        Destructive,
        "Members may merge and remove duplicate folders."
    ),
    r!(Get, "/workspace/blob/{node_id}", Scoped, Ordinary, ""),
    r!(Post, "/workspace/upload", Scoped, Ordinary, ""),
    r!(Patch, "/workspace/{node_id}", Scoped, Ordinary, ""),
    r!(
        Delete,
        "/workspace/{node_id}",
        Scoped,
        Destructive,
        "Members may delete workspace nodes."
    ),
    r!(Post, "/chat/upload", Scoped, Ordinary, ""),
];

const OPS_EXACT_ROUTES: &[Route] = &[
    Route {
        method: Verb::Get,
        path: "/api/v1/oauth/callback",
        address: Address::Exact,
        source: Source::Ops,
        access: Access::PublicGone,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Post,
        path: "/api/v1/companies/{id}/inboxes/ingest",
        address: Address::Exact,
        source: Source::Ops,
        access: Access::Hmac,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Post,
        path: "/api/v1/company/inboxes/ingest",
        address: Address::Exact,
        source: Source::Ops,
        access: Access::Hmac,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
];

const EXTERNAL_AUTHORITY_ROUTES: &[Route] = &[
    external_admin(
        Verb::Post,
        "/api/v1/companies/{id}/pause",
        Probe::Empty,
        RedCells::None,
    ),
    external_admin(
        Verb::Post,
        "/api/v1/companies/{id}/resume",
        Probe::Empty,
        RedCells::None,
    ),
    external_admin(
        Verb::Post,
        "/api/v1/companies/{id}/emergency-pause",
        Probe::Empty,
        RedCells::None,
    ),
    external_admin(
        Verb::Post,
        "/api/v1/companies/{id}/emergency-resume",
        Probe::Empty,
        RedCells::None,
    ),
];

// The empty scoped suffix shares this concrete path with the operator status
// route. It is represented so the runtime method-set gate remains exact.
const OVERLAPPING_EXTERNAL_ROUTES: &[Route] = &[Route {
    method: Verb::Get,
    path: "/api/v1/companies/{id}",
    address: Address::Exact,
    source: Source::ExternalAuthority,
    access: Access::Addressed,
    features: &["openhuman"],
    blast: Blast::Ordinary,
    probe: Probe::Empty,
    note: "",
    wait: Wait::TempPasswordBoundaryFix,
    red_cells: RedCells::TempPassword,
}];

const fn external_admin(
    method: Verb,
    path: &'static str,
    probe: Probe,
    red_cells: RedCells,
) -> Route {
    Route {
        method,
        path,
        address: Address::Exact,
        source: Source::ExternalAuthority,
        access: Access::Admin,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe,
        note: "",
        wait: Wait::None,
        red_cells,
    }
}

// Every `scoped(...)` route `operator.rs` registers: `ScopedCompany` governs
// all of them exactly as it governs the ops inventory, so a member may list
// or revoke a grant, staff a desk, or settle an in-review card — not just an
// admin. Checked against the `operator.rs` scan on its own in
// `source_path_set_equals_the_ops_matrix_path_set`, so a `scoped(...)` call
// added there without a row here fails that assertion.
const OPERATOR_AUTHORITY_ROUTES: &[Route] = &[
    Route {
        method: Verb::Post,
        path: "/approvals/{aid}",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Admin,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Json(r#"{"verdict":"deny","amended_payload":{}}"#),
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Post,
        path: "/approvals/{aid}/extend",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Admin,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Get,
        path: "/desks",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Post,
        path: "/desks",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Json(r#"{"name":"__matrix_absent__"}"#),
        note: "Members may create desks (group chats) for the company.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Delete,
        path: "/desks/{desk_id}",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Destructive,
        probe: Probe::Empty,
        note: "Members may delete operator-created desks.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Post,
        path: "/desks/{desk_id}/members",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Json(r#"{"agent_id":"__matrix_absent__"}"#),
        note: "Members may add a teammate to a desk.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Delete,
        path: "/desks/{desk_id}/members/{agent_id}",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Empty,
        note: "Members may remove an operator-added desk member.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Get,
        path: "/desks/{desk_id}/hive",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Empty,
        note: "Members may read the move grammar in force on a desk.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Put,
        path: "/desks/{desk_id}/hive",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Json(r#"{}"#),
        note: "Members may install or replace a desk's move grammar.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Delete,
        path: "/desks/{desk_id}/hive",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Empty,
        note: "Members may drop a desk's installed grammar and fall back to the manifest.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Put,
        path: "/desks/{desk_id}/order",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Json(r#"{"ordered_member_ids":[]}"#),
        note: "Members may reorder a desk's members.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Get,
        path: "/operator-channel",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Get,
        path: "/events",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Sse,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Get,
        path: "/grants",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Empty,
        note: "Lists every standing permission open on the company, including who granted it and what it admits.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Delete,
        path: "/grants/{gid}",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Admin,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Empty,
        note: "Admins revoke a standing permission; 404 when there is nothing to revoke rather than reporting success over a no-op.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Post,
        path: "/chat/review",
        address: Address::Dual,
        source: Source::Operator,
        access: Access::Scoped,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Json(
            r#"{"chatId":"__matrix_absent__","taskId":"__matrix_absent__","decision":"approve"}"#,
        ),
        note: "Members may approve or revise a task's in-review dispatch card.",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
];

// Direct (non-`scoped`) `operator.rs` routes: dual-address writes/reads
// registered as two separate `.route(...)` calls (one per addressing form)
// rather than through the `scoped(...)` helper, because the two forms resolve
// the company differently enough that they are two handler functions, not
// one — the same reason `EXTERNAL_AUTHORITY_ROUTES` and
// `OVERLAPPING_EXTERNAL_ROUTES` model their own operator.rs routes
// (`resolve_approval`, `company_status`, …) the same way instead of through
// `scoped`. Closed the same way as the scoped set: asserted in
// `source_path_set_equals_the_ops_matrix_path_set` against operator.rs's
// scanned direct-call set, combined with the operator-sourced subset of
// `EXTERNAL_AUTHORITY_ROUTES`/`OVERLAPPING_EXTERNAL_ROUTES`.
const OPERATOR_DIRECT_ROUTES: &[Route] = &[
    // No address to check ownership against — `list_companies` filters the
    // registry to what each principal may see rather than refusing anyone
    // merely authenticated. See `Access::Visible`.
    Route {
        method: Verb::Get,
        path: "/api/v1/companies",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Visible,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::TempPasswordBoundaryFix,
        red_cells: RedCells::TempPassword,
    },
    Route {
        method: Verb::Post,
        path: "/api/v1/companies/{id}/chat",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Json(r#"{"text":"__matrix_absent__","detach":true}"#),
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Post,
        path: "/api/v1/company/chat",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Json(r#"{"text":"__matrix_absent__","detach":true}"#),
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Get,
        path: "/api/v1/companies/{id}/chat/history",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Get,
        path: "/api/v1/company/chat/history",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    // `GET {scope}/agents/{agent_id}/session` — the same read as
    // `chat/history` above, narrowed to one teammate and widened to every
    // channel it can reach. It resolves its principal through the *same*
    // `history_viewer` gate, so it carries the same access class: anything
    // stricter here would claim an authority the handler does not enforce,
    // and anything looser would understate it.
    //
    // Both scope forms carry a row because both are registered in
    // `operator.rs`, and this matrix is closed against the source path set —
    // listing one would fail `source_path_set_equals_the_ops_matrix_path_set`
    // exactly as omitting both just did.
    Route {
        method: Verb::Get,
        path: "/api/v1/companies/{id}/agents/{agent_id}/session",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Get,
        path: "/api/v1/company/agents/{agent_id}/session",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Get,
        path: "/api/v1/companies/{id}/chat/attribution-audit",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Get,
        path: "/api/v1/company/chat/attribution-audit",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Empty,
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Post,
        path: "/api/v1/companies/{id}/chat/messages/{seq}/reactions",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Json(r#"{"emoji":"👍","on":true}"#),
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    Route {
        method: Verb::Post,
        path: "/api/v1/company/chat/messages/{seq}/reactions",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Ordinary,
        probe: Probe::Json(r#"{"emoji":"👍","on":true}"#),
        note: "",
        wait: Wait::None,
        red_cells: RedCells::None,
    },
    // `list_approvals`/`list_approvals_single` check `authorize_address` but,
    // like `company_status`, never call `refuse_until_password_changed` — the
    // same known temp-password gap `OVERLAPPING_EXTERNAL_ROUTES` already
    // tracks, found here on two more routes rather than a new one; pinned
    // through the same wait branch rather than opening a second issue for an
    // identical defect.
    Route {
        method: Verb::Get,
        path: "/api/v1/companies/{id}/approvals",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Empty,
        note: "Members may read pending approvals, filtered to what their role may see.",
        wait: Wait::TempPasswordBoundaryFix,
        red_cells: RedCells::TempPassword,
    },
    Route {
        method: Verb::Get,
        path: "/api/v1/company/approvals",
        address: Address::Exact,
        source: Source::Operator,
        access: Access::Addressed,
        features: &["openhuman"],
        blast: Blast::Authority,
        probe: Probe::Empty,
        note: "Members may read pending approvals, filtered to what their role may see.",
        wait: Wait::TempPasswordBoundaryFix,
        red_cells: RedCells::TempPassword,
    },
];

fn all_routes() -> impl Iterator<Item = &'static Route> {
    OPS_SCOPED_ROUTES
        .iter()
        .chain(OPS_EXACT_ROUTES)
        .chain(EXTERNAL_AUTHORITY_ROUTES)
        .chain(OVERLAPPING_EXTERNAL_ROUTES)
        .chain(OPERATOR_DIRECT_ROUTES)
        .chain(OPERATOR_AUTHORITY_ROUTES)
}

fn routes() -> impl Iterator<Item = &'static Route> {
    all_routes().filter(|route| {
        route
            .features
            .iter()
            .all(|feature| feature_enabled(feature))
    })
}

fn feature_enabled(feature: &str) -> bool {
    match feature {
        "openhuman" => cfg!(feature = "openhuman"),
        "mcp" => cfg!(feature = "mcp"),
        "composio" => cfg!(feature = "composio"),
        "acp" => cfg!(feature = "acp"),
        "documents" => cfg!(feature = "documents"),
        "webhooks" => cfg!(feature = "webhooks"),
        "tinyplace" => cfg!(feature = "tinyplace"),
        unknown => panic!("matrix row names unknown Cargo feature {unknown:?}"),
    }
}

struct Harness {
    _home: TempDir,
    app: Router,
}

impl Harness {
    async fn build() -> Self {
        let home = tempfile::Builder::new()
            .prefix("opencompany-auth-matrix-")
            .tempdir()
            .expect("matrix tempdir");
        let manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Matrix Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .expect("minimal matrix manifest");
        let id = CompanyId::new(COMPANY);
        let runtime = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .expect("matrix runtime");
        let state = AppState::new(opencompany::AppConfig::default())
            .with_home(home.path().to_path_buf())
            .with_platform_auth(test_support::fixed_principal_platform_auth());
        state.registry().insert(id.clone(), Arc::new(runtime));
        state.set_owner(id, "tenant:matrix-owner");
        test_support::seed_fixed_member(&state, COMPANY).await;
        test_support::seed_fixed_admin(&state, COMPANY).await;
        test_support::seed_fixed_temp_password_admin(&state, COMPANY).await;
        let app = server::router(state);
        Self { _home: home, app }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        principal: Principal,
        probe: Probe,
    ) -> Observed {
        let mut request = Request::builder().method(method).uri(uri);
        request = match principal {
            Principal::Anonymous => request,
            Principal::Member => request.header("cookie", test_support::member_cookie(COMPANY)),
            Principal::Admin => request.header("cookie", test_support::fixed_cookie(COMPANY)),
            Principal::MustChangePasswordAdmin => {
                request.header("cookie", test_support::temp_password_cookie(COMPANY))
            }
            Principal::TenantOwner => request.header(
                "authorization",
                format!("Bearer {}", test_support::FIXED_TENANT_OWNER_TEST_TOKEN),
            ),
            Principal::TenantNonOwner => request.header(
                "authorization",
                format!("Bearer {}", test_support::FIXED_TENANT_NON_OWNER_TEST_TOKEN),
            ),
            Principal::Platform => request.header(
                "authorization",
                format!("Bearer {}", test_support::FIXED_PLATFORM_TEST_TOKEN),
            ),
        };
        let body = match probe {
            Probe::Empty | Probe::Capability | Probe::Sse => Body::empty(),
            Probe::Json(json) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(json)
            }
        };
        let response = self
            .app
            .clone()
            .oneshot(request.body(body).expect("matrix request"))
            .await
            .expect("infallible router response");
        let status = response.status();
        let headers = response.headers().clone();
        // A permitted SSE response never ends its body on its own (a live
        // subscription plus a periodic keep-alive), so draining it would hang
        // the harness rather than time out cleanly. A refused SSE request
        // still gets a normal, finite JSON error body from the extractor
        // rejection, so only the success path skips the drain.
        let json = if probe == Probe::Sse && status.is_success() {
            None
        } else {
            let body = to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("bounded matrix body");
            serde_json::from_slice::<Value>(&body).ok()
        };
        Observed {
            status,
            headers,
            json,
        }
    }
}

#[derive(Debug)]
struct Observed {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    json: Option<Value>,
}

impl Observed {
    fn code(&self) -> Option<&str> {
        self.json.as_ref()?.get("code")?.as_str()
    }
}

fn route_patterns(route: &Route) -> Vec<String> {
    match route.address {
        Address::Dual => vec![
            format!("/api/v1/companies/{{id}}{}", route.path),
            format!("/api/v1/company{}", route.path),
        ],
        Address::Exact => vec![route.path.to_string()],
    }
}

fn request_uri(route: &Route, pattern: &str) -> String {
    let mut uri = materialize_path(pattern);
    if route.probe == Probe::Capability {
        uri.push_str("?oc_cap=__matrix_absent__");
    }
    uri
}

fn materialize_path(pattern: &str) -> String {
    let mut result = pattern.replace("{id}", COMPANY);
    while let Some(open) = result.find('{') {
        let close = result[open..]
            .find('}')
            .map(|offset| open + offset)
            .expect("balanced path parameter");
        result.replace_range(open..=close, ABSENT_ID);
    }
    result
}

fn check_verdict(
    route: &Route,
    pattern: &str,
    principal: Principal,
    observed: &Observed,
) -> Result<(), String> {
    let expected = route.access.expected(principal);
    let context = format!(
        "{} {} for {} expected {} but observed {observed:?}",
        route.method.label(),
        pattern,
        principal.label(),
        expected.snapshot(),
    );
    match expected {
        Verdict::Refused(status, code) => {
            if observed.status == status && observed.code() == Some(code) {
                Ok(())
            } else {
                Err(context)
            }
        }
        Verdict::Permitted => {
            let refused_status = matches!(
                observed.status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            );
            let refused_code = matches!(
                observed.code(),
                Some("unauthorized" | "forbidden" | "password_change_required")
            );
            if refused_status || refused_code {
                Err(context)
            } else {
                Ok(())
            }
        }
        Verdict::Exact(status, code) => {
            if observed.status == status && code.is_none_or(|value| observed.code() == Some(value))
            {
                Ok(())
            } else {
                Err(context)
            }
        }
    }
}

async fn check_cells(wait: Option<Wait>) {
    let harness = Harness::build().await;
    let mut failures = Vec::new();
    for route in routes() {
        for pattern in route_patterns(route) {
            for principal in Principal::ALL {
                let selected = match wait {
                    None => !route.red_cells.contains(principal),
                    Some(group) => route.wait == group && route.red_cells.contains(principal),
                };
                if !selected {
                    continue;
                }
                let observed = harness
                    .request(
                        route.method.method(),
                        &request_uri(route, &pattern),
                        principal,
                        route.probe,
                    )
                    .await;
                if let Err(failure) = check_verdict(route, &pattern, principal, &observed) {
                    failures.push(failure);
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn declared_non_defect_cells_hold_the_seven_principal_boundary() {
    check_cells(None).await;
}

#[tokio::test]
#[ignore = "waits on a body-admin signature branch; none assigned in handoff section 7"]
async fn body_admin_machine_principals_wait_for_an_assigned_branch() {
    check_cells(Some(Wait::BodyAdminFix)).await;
}

#[tokio::test]
#[ignore = "waits on a ledger-authority branch; none assigned in handoff section 7"]
async fn ledger_authority_waits_for_an_assigned_branch() {
    check_cells(Some(Wait::LedgerFix)).await;
}

#[tokio::test]
#[ignore = "waits on a team-delete-authority branch; none assigned in handoff section 7"]
async fn team_delete_authority_waits_for_an_assigned_branch() {
    check_cells(Some(Wait::TeamFix)).await;
}

#[tokio::test]
#[ignore = "waits on a workflow-authority branch; none assigned in handoff section 7"]
async fn workflow_authority_waits_for_an_assigned_branch() {
    check_cells(Some(Wait::WorkflowFix)).await;
}

#[tokio::test]
#[ignore = "waits on a temp-password-boundary branch; none assigned in handoff section 7"]
async fn company_status_temp_password_boundary_waits_for_an_assigned_branch() {
    check_cells(Some(Wait::TempPasswordBoundaryFix)).await;
}

#[test]
fn table_counts_and_intentional_widenings_are_explicit() {
    assert_eq!(OPS_SCOPED_ROUTES.len(), 188);
    assert_eq!(
        OPS_SCOPED_ROUTES
            .iter()
            .map(|route| route.path)
            .collect::<BTreeSet<_>>()
            .len(),
        146,
    );
    assert_eq!(OPS_EXACT_ROUTES.len(), 3);
    assert_eq!(
        OPS_SCOPED_ROUTES.len() * 2,
        376,
        "dual-address ops route-method rows",
    );
    assert_eq!(
        OPS_SCOPED_ROUTES.len() * 2 + OPS_EXACT_ROUTES.len(),
        379,
        "complete ops route-method rows",
    );
    assert_eq!(EXTERNAL_AUTHORITY_ROUTES.len(), 4);
    assert_eq!(OVERLAPPING_EXTERNAL_ROUTES.len(), 1);
    assert_eq!(OPERATOR_AUTHORITY_ROUTES.len(), 16);
    assert_eq!(OPERATOR_DIRECT_ROUTES.len(), 13);
    assert_eq!(
        all_routes()
            .map(|route| route_patterns(route).len())
            .sum::<usize>(),
        429,
        "concrete route-method rows",
    );
    assert_eq!(
        all_routes()
            .flat_map(route_patterns)
            .collect::<BTreeSet<_>>()
            .len(),
        338,
        "concrete paths",
    );
    assert_eq!(render_snapshot().lines().count(), 3_003);
    assert_eq!(
        all_routes()
            .map(|route| {
                route_patterns(route).len()
                    * Principal::ALL
                        .into_iter()
                        .filter(|principal| route.red_cells.contains(*principal))
                        .count()
            })
            .sum::<usize>(),
        46,
        "ignored red principal cells",
    );
    assert_eq!(
        OPS_SCOPED_ROUTES
            .iter()
            .filter(|route| route.access == Access::Admin)
            .count(),
        60,
        "45 signature-admin, seven body-admin, and eight aspirational authority rows",
    );
    assert_eq!(
        OPS_SCOPED_ROUTES
            .iter()
            .filter(|route| route.red_cells == RedCells::TenantOwnerAndPlatform)
            .count(),
        7,
        "seven source routes still put require_admin in the handler body",
    );

    let mut failures = Vec::new();
    for route in all_routes() {
        assert!(
            !route.features.is_empty(),
            "{} {} has no owning feature lane",
            route.method.label(),
            route.path,
        );
        for feature in route.features {
            let _ = feature_enabled(feature);
        }
        if route.access.expected(Principal::Member) == Verdict::Permitted
            && route.blast != Blast::Ordinary
            && route.note.trim().is_empty()
        {
            failures.push(format!(
                "{} {} permits members at {} blast radius without a note",
                route.method.label(),
                route.path,
                route.blast.label(),
            ));
        }
        if route.wait == Wait::None && route.red_cells != RedCells::None {
            failures.push(format!(
                "{} {} has red cells without a named wait",
                route.method.label(),
                route.path,
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn committed_snapshot_pins_every_expected_cell() {
    let actual = render_snapshot();
    let snapshot = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/auth-matrix.txt");
    if std::env::var_os("BLESS_AUTH_MATRIX").is_some() {
        std::fs::write(&snapshot, &actual).expect("write auth matrix snapshot");
        return;
    }
    assert_eq!(actual, include_str!("snapshots/auth-matrix.txt"));
}

fn render_snapshot() -> String {
    let mut lines = Vec::new();
    for route in all_routes() {
        for path in route_patterns(route) {
            for principal in Principal::ALL {
                let wait = if route.red_cells.contains(principal) {
                    route.wait.label()
                } else {
                    "-"
                };
                lines.push(format!(
                    "{} {} {} {} source={} access={} features={} blast={} note={:?} wait={}",
                    route.method.label(),
                    path,
                    principal.label(),
                    route.access.expected(principal).snapshot(),
                    route.source.label(),
                    route.access.label(),
                    route.features.join("+"),
                    route.blast.label(),
                    route.note,
                    wait,
                ));
            }
        }
    }
    lines.sort();
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[test]
fn source_path_set_equals_the_ops_matrix_path_set() {
    let server_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server");
    let ops_root = server_root.join("ops");
    let scanned = scan_ops_routes(&ops_root).unwrap_or_else(|error| panic!("{error}"));

    // `operator.rs` registers `scoped(...)` routes too (issue #2148 part 2),
    // through the identical helper the ops directory uses — but the ops scan
    // only walks `src/server/ops`, so its `scoped(...)` calls need their own
    // pass rather than silently joining the ops directory's tree walk.
    let operator_file = server_root.join("operator.rs");
    let operator_scan =
        scan_file_route_literals(&operator_file).unwrap_or_else(|error| panic!("{error}"));

    // Each scan is checked against its own matrix constant, not a union of
    // both. A route whose registration moves from the ops directory to
    // `operator.rs` (or back) keeps the same suffix, so a unioned check
    // stays green on that move alone while the matrix still labels the
    // route under its old source and the wrong table declares it. Comparing
    // per-source is what turns that provenance drift into a failure instead
    // of a set membership that never notices which side lost a suffix and
    // which side gained one.
    let ops_expected_suffixes: BTreeSet<_> = OPS_SCOPED_ROUTES
        .iter()
        .map(|route| route.path.to_string())
        .collect();
    assert_set_eq("ops scoped suffix", &ops_expected_suffixes, &scanned.scoped);

    let operator_expected_suffixes: BTreeSet<_> = OPERATOR_AUTHORITY_ROUTES
        .iter()
        .map(|route| route.path.to_string())
        .collect();
    assert_set_eq(
        "operator scoped suffix",
        &operator_expected_suffixes,
        &operator_scan.scoped,
    );

    let expected_direct: BTreeSet<_> = OPS_EXACT_ROUTES
        .iter()
        .map(|route| route.path.to_string())
        .collect();
    assert_set_eq("direct ops path", &expected_direct, &scanned.direct);
    assert_eq!(
        scanned.allowed_nonliteral,
        BTreeMap::from([
            ("connections_read.rs:self.route(stored)", 1),
            ("scope.rs:company-id-format", 1),
            ("scope.rs:single-company-format", 1),
        ]),
        "non-Axum/dynamic route allowlist drifted"
    );

    // `operator.rs`'s direct (non-`scoped`) routes are declared two ways: the
    // ones this file also owns the authority story for
    // (`OPERATOR_DIRECT_ROUTES`), and the ones `EXTERNAL_AUTHORITY_ROUTES` /
    // `OVERLAPPING_EXTERNAL_ROUTES` already declare — those two constants also
    // hold `provision.rs`'s pause/resume/emergency-* routes, so the
    // operator-sourced subset is whatever is left after subtracting
    // `provision.rs`'s own scanned direct set, rather than a second literal
    // path list that could drift from the first.
    let provision_scan = scan_file_route_literals(&server_root.join("provision.rs"))
        .unwrap_or_else(|error| panic!("{error}"));
    let external_and_overlapping_direct: BTreeSet<String> = EXTERNAL_AUTHORITY_ROUTES
        .iter()
        .chain(OVERLAPPING_EXTERNAL_ROUTES.iter())
        .map(|route| route.path.to_string())
        .collect();
    let operator_sourced_external: BTreeSet<String> = external_and_overlapping_direct
        .difference(&provision_scan.direct)
        .cloned()
        .collect();
    let expected_operator_direct: BTreeSet<String> = OPERATOR_DIRECT_ROUTES
        .iter()
        .map(|route| route.path.to_string())
        .chain(operator_sourced_external)
        .collect();
    assert_set_eq(
        "operator direct path",
        &expected_operator_direct,
        &operator_scan.direct,
    );
}

#[test]
fn external_authority_router_files_have_no_unclassified_paths() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server");
    let provision = scan_file_route_literals(&root.join("provision.rs"))
        .unwrap_or_else(|error| panic!("{error}"));
    assert_set_eq(
        "provision direct path",
        &string_set(&[
            "/api/v1/companies",
            "/api/v1/companies/provisioning",
            "/api/v1/companies/{id}/pause",
            "/api/v1/companies/{id}/resume",
            "/api/v1/companies/{id}/emergency-pause",
            "/api/v1/companies/{id}/emergency-resume",
            "/api/v1/companies/{id}/suspend",
            "/api/v1/companies/{id}/archive",
        ]),
        &provision.direct,
    );

    // Knowing a path exists is not the same as exercising it. These four are
    // `PlatformScope` and the matrix has no platform access class to express
    // them, so they carry no row and no principal ever probes them. Naming
    // them here is what stops that being silent: a ninth provisioning route,
    // or a fix that gives these rows, fails this assertion rather than
    // quietly joining a set nobody checks.
    let unexercised: BTreeSet<String> = provision
        .direct_methods
        .iter()
        .filter(|entry| {
            !EXTERNAL_AUTHORITY_ROUTES
                .iter()
                .any(|route| *entry == &format!("{} {}", route.method.label(), route.path))
        })
        .cloned()
        .collect();
    assert_set_eq(
        "provisioning routes no principal probes",
        &string_set(&[
            "POST /api/v1/companies",
            "GET /api/v1/companies/provisioning",
            "POST /api/v1/companies/{id}/suspend",
            "POST /api/v1/companies/{id}/archive",
        ]),
        &unexercised,
    );

    assert!(provision.scoped.is_empty());

    // `operator.rs`'s own direct and scoped path sets are closed against the
    // matrix in `source_path_set_equals_the_ops_matrix_path_set`, not here —
    // this test stays about `provision.rs`, the one file left with no
    // matrix-derived closure.
}

fn string_set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn assert_set_eq(label: &str, expected: &BTreeSet<String>, actual: &BTreeSet<String>) {
    let missing: Vec<_> = expected.difference(actual).collect();
    let unexpected: Vec<_> = actual.difference(expected).collect();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "{label} set drifted; missing={missing:?}; unexpected={unexpected:?}",
    );
}

#[tokio::test]
async fn runtime_allow_headers_equal_the_declared_method_sets() {
    let harness = Harness::build().await;
    let expected = declared_method_sets();
    let mut failures = Vec::new();
    for (pattern, methods) in expected {
        let uri = materialize_path(&pattern);
        let observed = harness
            .request(Method::TRACE, &uri, Principal::Anonymous, Probe::Empty)
            .await;
        if observed.status != StatusCode::METHOD_NOT_ALLOWED {
            failures.push(format!(
                "TRACE {pattern}: expected 405, observed {:?}",
                observed.status
            ));
            continue;
        }
        let Some(allow) = observed
            .headers
            .get(header::ALLOW)
            .and_then(|value| value.to_str().ok())
        else {
            failures.push(format!("TRACE {pattern}: 405 response had no Allow header"));
            continue;
        };
        let mut actual: BTreeSet<String> = allow
            .split(',')
            .map(str::trim)
            .filter(|method| !method.is_empty())
            .map(str::to_string)
            .collect();
        // Axum adds HEAD implicitly to every GET router. On a non-GET path,
        // HEAD remains significant so an explicit addition cannot hide here.
        if methods.contains("GET") {
            actual.remove("HEAD");
        }
        if actual != methods {
            failures.push(format!(
                "{pattern}: declared methods {methods:?}, runtime Allow methods {actual:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn declared_method_sets() -> BTreeMap<String, BTreeSet<String>> {
    let mut sets = BTreeMap::<String, BTreeSet<String>>::new();
    for route in routes() {
        for pattern in route_patterns(route) {
            sets.entry(pattern)
                .or_default()
                .insert(route.method.label().to_string());
        }
    }
    // `provision.rs` registers `POST /api/v1/companies` (provisioning) on the
    // exact path `list_companies` answers `GET` on. That `POST` carries no row
    // of its own — it is one of the four provisioning routes the matrix has no
    // `PlatformScope`-shaped access class to express (see
    // `external_authority_router_files_have_no_unclassified_paths`'s
    // "provisioning routes no principal probes" set) — so without this the
    // runtime's real `{GET, POST}` Allow header would fail this gate on a
    // route this change was never asked to widen.
    if let Some(methods) = sets.get_mut("/api/v1/companies") {
        methods.insert("POST".to_string());
    }
    sets
}

#[derive(Debug)]
struct Scan {
    scoped: BTreeSet<String>,
    direct: BTreeSet<String>,
    /// `"POST /api/v1/companies"` — the method matters as much as the path.
    /// A path already in the inventory can gain a second method, and a
    /// path-only set stays green while that new method goes unprobed.
    direct_methods: BTreeSet<String>,
    allowed_nonliteral: BTreeMap<&'static str, usize>,
}

/// Every HTTP verb a `.route("/p", …)` call wires, including Axum's chained
/// form `post(handler).delete(other)`. Reading only the first identifier
/// records `POST` and silently drops the `DELETE`, which is the same
/// path-shaped blindness this check exists to remove.
fn route_verbs(tokens: &[Token], open_paren: usize) -> BTreeSet<String> {
    const VERBS: [&str; 5] = ["get", "post", "put", "patch", "delete"];
    let mut verbs = BTreeSet::new();
    let mut depth = 0usize;
    for index in open_paren..tokens.len() {
        match punct_at(tokens, index) {
            Some('(') => depth += 1,
            Some(')') => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        if depth == 1
            && punct_at(tokens, index + 1) == Some('(')
            && let Some(name) = ident_at(tokens, index)
            && VERBS.contains(&name)
        {
            verbs.insert(name.to_ascii_uppercase());
        }
    }
    if verbs.is_empty() {
        verbs.insert("?".to_string());
    }
    verbs
}

fn scan_file_route_literals(file: &Path) -> Result<Scan, String> {
    let source = std::fs::read_to_string(file).map_err(|error| error.to_string())?;
    let tokens = lex(&source).map_err(|error| format!("{}: {error}", file.display()))?;
    let skipped = test_only_token_indexes(&tokens);
    let mut scoped = BTreeSet::new();
    let mut direct = BTreeSet::new();
    let mut direct_methods = BTreeSet::new();
    for index in 0..tokens.len() {
        if skipped.contains(&index) {
            continue;
        }
        if ident_at(&tokens, index) == Some("scoped") && punct_at(&tokens, index + 1) == Some('(') {
            match tokens.get(index + 2).map(|token| &token.kind) {
                Some(TokenKind::String(path)) => {
                    scoped.insert(path.clone());
                }
                _ => {
                    return Err(format!(
                        "{}:{}: external scoped(...) argument is not a parseable literal",
                        file.display(),
                        tokens[index].line,
                    ));
                }
            }
        }
        if punct_at(&tokens, index) == Some('.')
            && ident_at(&tokens, index + 1) == Some("route")
            && punct_at(&tokens, index + 2) == Some('(')
        {
            match tokens.get(index + 3).map(|token| &token.kind) {
                Some(TokenKind::String(path)) => {
                    direct.insert(path.clone());
                    // `.route("/p", post(h))` — the verb is the identifier
                    // after the comma. An unreadable one is recorded as
                    // `?` rather than skipped, so it cannot vanish quietly.
                    for verb in route_verbs(&tokens, index + 2) {
                        direct_methods.insert(format!("{verb} {path}"));
                    }
                }
                _ => {
                    return Err(format!(
                        "{}:{}: external .route(...) argument is not a parseable literal",
                        file.display(),
                        tokens[index].line,
                    ));
                }
            }
        }
    }
    Ok(Scan {
        scoped,
        direct,
        direct_methods,
        allowed_nonliteral: BTreeMap::new(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TokenKind {
    Ident(String),
    String(String),
    Punct(char),
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    line: usize,
}

fn scan_ops_routes(root: &Path) -> Result<Scan, String> {
    let mut files = Vec::new();
    collect_rust_files(root, &mut files).map_err(|error| error.to_string())?;
    files.sort();
    let mut scoped = BTreeSet::new();
    let mut direct = BTreeSet::new();
    let mut direct_methods = BTreeSet::new();
    let mut allowed_nonliteral = BTreeMap::new();
    for file in files {
        let relative = file.strip_prefix(root).expect("collected under root");
        if is_external_test_file(relative) {
            continue;
        }
        let source = std::fs::read_to_string(&file).map_err(|error| error.to_string())?;
        let tokens = lex(&source).map_err(|error| format!("{}: {error}", file.display()))?;
        let skipped = test_only_token_indexes(&tokens);
        for index in 0..tokens.len() {
            if skipped.contains(&index) {
                continue;
            }
            if ident_at(&tokens, index) == Some("scoped")
                && punct_at(&tokens, index + 1) == Some('(')
                && ident_at(&tokens, index.wrapping_sub(1)) != Some("fn")
            {
                match tokens.get(index + 2).map(|token| &token.kind) {
                    Some(TokenKind::String(path)) => {
                        scoped.insert(path.clone());
                    }
                    _ => {
                        return Err(format!(
                            "{}:{}: production scoped(...) argument is not a parseable literal",
                            relative.display(),
                            tokens[index].line,
                        ));
                    }
                }
            }
            if punct_at(&tokens, index) == Some('.')
                && ident_at(&tokens, index + 1) == Some("route")
                && punct_at(&tokens, index + 2) == Some('(')
            {
                match tokens.get(index + 3).map(|token| &token.kind) {
                    Some(TokenKind::String(path)) => {
                        direct.insert(path.clone());
                        for verb in route_verbs(&tokens, index + 2) {
                            direct_methods.insert(format!("{verb} {path}"));
                        }
                    }
                    _ => {
                        let Some(fingerprint) =
                            allowed_nonliteral_route(relative, &tokens, index + 3)
                        else {
                            return Err(format!(
                                "{}:{}: production .route(...) argument is not a parseable literal",
                                relative.display(),
                                tokens[index].line,
                            ));
                        };
                        *allowed_nonliteral.entry(fingerprint).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    Ok(Scan {
        scoped,
        direct,
        direct_methods,
        allowed_nonliteral,
    })
}

fn collect_rust_files(root: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_rust_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn is_external_test_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == "test.rs"
        || name == "tests.rs"
        || name.ends_with("_test.rs")
        || name.ends_with("_tests.rs")
}

fn allowed_nonliteral_route(
    relative: &Path,
    tokens: &[Token],
    argument: usize,
) -> Option<&'static str> {
    let file = relative.to_string_lossy();
    if file == "connections_read.rs"
        && ident_at(tokens, argument.wrapping_sub(4)) == Some("self")
        && ident_at(tokens, argument) == Some("stored")
        && punct_at(tokens, argument + 1) == Some(')')
    {
        return Some("connections_read.rs:self.route(stored)");
    }
    if file != "scope.rs"
        || punct_at(tokens, argument) != Some('&')
        || ident_at(tokens, argument + 1) != Some("format")
        || punct_at(tokens, argument + 2) != Some('!')
        || punct_at(tokens, argument + 3) != Some('(')
    {
        return None;
    }
    match tokens.get(argument + 4).map(|token| &token.kind) {
        Some(TokenKind::String(value)) if value == "/api/v1/companies/{{id}}{suffix}" => {
            Some("scope.rs:company-id-format")
        }
        Some(TokenKind::String(value)) if value == "/api/v1/company{suffix}" => {
            Some("scope.rs:single-company-format")
        }
        _ => None,
    }
}

fn ident_at(tokens: &[Token], index: usize) -> Option<&str> {
    match tokens.get(index)?.kind {
        TokenKind::Ident(ref value) => Some(value),
        _ => None,
    }
}

fn punct_at(tokens: &[Token], index: usize) -> Option<char> {
    match tokens.get(index)?.kind {
        TokenKind::Punct(value) => Some(value),
        _ => None,
    }
}

fn test_only_token_indexes(tokens: &[Token]) -> BTreeSet<usize> {
    let mut skipped = BTreeSet::new();
    let mut index = 0;
    while index + 5 < tokens.len() {
        if punct_at(tokens, index) != Some('#') || punct_at(tokens, index + 1) != Some('[') {
            index += 1;
            continue;
        }
        let Some(close) = matching_delimiter(tokens, index + 1, '[', ']') else {
            index += 1;
            continue;
        };
        let cfg_cannot_run_in_production = ident_at(tokens, index + 2) == Some("cfg")
            && punct_at(tokens, index + 3) == Some('(')
            && cfg_can_be_true_without_test(tokens, index + 4, close - 1) == Some(false);
        if !cfg_cannot_run_in_production {
            index = close + 1;
            continue;
        }
        let item_start = close + 1;
        let Some(item_end) = attributed_item_end(tokens, item_start) else {
            index = close + 1;
            continue;
        };
        skipped.extend(index..=item_end);
        index = item_end + 1;
    }
    skipped
}

// Evaluates whether a cfg expression can be true when `test=false`. Every
// other predicate is treated as independently selectable, which retains
// `cfg(any(test, feature = "..."))` production items.
fn cfg_can_be_true_without_test(tokens: &[Token], start: usize, end: usize) -> Option<bool> {
    if start >= end {
        return None;
    }
    if ident_at(tokens, start) == Some("test") {
        return Some(false);
    }
    let function = ident_at(tokens, start)?;
    if punct_at(tokens, start + 1) != Some('(') {
        return Some(true);
    }
    let close = matching_delimiter(tokens, start + 1, '(', ')')?;
    if close > end {
        return None;
    }
    let mut values = Vec::new();
    let mut cursor = start + 2;
    while cursor < close {
        let next = cfg_expression_end(tokens, cursor, close);
        values.push(cfg_can_be_true_without_test(tokens, cursor, next)?);
        cursor = next;
        if punct_at(tokens, cursor) == Some(',') {
            cursor += 1;
        }
    }
    match function {
        "all" => Some(values.into_iter().all(|value| value)),
        "any" => Some(values.into_iter().any(|value| value)),
        // A non-test predicate may be either true or false, so a negation can
        // still be true. The only exact false case needed here is not(any())
        // over constants; retaining an item is the safe direction.
        "not" => Some(true),
        _ => Some(true),
    }
}

fn cfg_expression_end(tokens: &[Token], start: usize, limit: usize) -> usize {
    let mut cursor = start;
    let mut depth = 0usize;
    while cursor < limit {
        match punct_at(tokens, cursor) {
            Some('(' | '[' | '{') => depth += 1,
            Some(')' | ']' | '}') if depth > 0 => depth -= 1,
            Some(',') if depth == 0 => break,
            _ => {}
        }
        cursor += 1;
    }
    cursor
}

fn attributed_item_end(tokens: &[Token], start: usize) -> Option<usize> {
    let mut cursor = start;
    while cursor < tokens.len() {
        match punct_at(tokens, cursor) {
            Some('{') => return matching_delimiter(tokens, cursor, '{', '}'),
            Some(';') => return Some(cursor),
            _ => cursor += 1,
        }
    }
    None
}

fn matching_delimiter(tokens: &[Token], open: usize, left: char, right: char) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            TokenKind::Punct(value) if value == left => depth += 1,
            TokenKind::Punct(value) if value == right => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn lex(source: &str) -> Result<Vec<Token>, String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                line += 1;
                index += 1;
            }
            byte if byte.is_ascii_whitespace() => index += 1,
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index, &mut line)?;
            }
            b'"' => {
                let string_line = line;
                let (value, next) = normal_string(bytes, index, &mut line)?;
                tokens.push(Token {
                    kind: TokenKind::String(value),
                    line: string_line,
                });
                index = next;
            }
            b'r' if raw_string_start(bytes, index).is_some() => {
                let string_line = line;
                let (value, next) = raw_string(bytes, index, &mut line)?;
                tokens.push(Token {
                    kind: TokenKind::String(value),
                    line: string_line,
                });
                index = next;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Ident(source[start..index].to_string()),
                    line,
                });
            }
            b'\'' => {
                index = skip_char_or_lifetime(bytes, index, &mut line)?;
            }
            byte => {
                tokens.push(Token {
                    kind: TokenKind::Punct(char::from(byte)),
                    line,
                });
                index += 1;
            }
        }
    }
    Ok(tokens)
}

fn skip_block_comment(bytes: &[u8], mut index: usize, line: &mut usize) -> Result<usize, String> {
    let mut depth = 0usize;
    while index + 1 < bytes.len() {
        match (bytes[index], bytes[index + 1]) {
            (b'/', b'*') => {
                depth += 1;
                index += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                index += 2;
                if depth == 0 {
                    return Ok(index);
                }
            }
            (b'\n', _) => {
                *line += 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    Err("unterminated block comment".to_string())
}

fn normal_string(
    bytes: &[u8],
    mut index: usize,
    line: &mut usize,
) -> Result<(String, usize), String> {
    index += 1;
    let mut value = String::new();
    while index < bytes.len() {
        match bytes[index] {
            b'"' => return Ok((value, index + 1)),
            b'\\' => {
                let escaped = *bytes.get(index + 1).ok_or("unterminated string escape")?;
                if escaped == b'\n' {
                    *line += 1;
                    index += 2;
                    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                        if bytes[index] == b'\n' {
                            *line += 1;
                        }
                        index += 1;
                    }
                    continue;
                }
                let decoded = match escaped {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'0' => '\0',
                    // Route literals are plain ASCII. Other strings only need
                    // to remain one token, so retaining the escaped byte is
                    // sufficient for scanner correctness.
                    _ => char::from(escaped),
                };
                value.push(decoded);
                index += 2;
            }
            b'\n' => {
                *line += 1;
                value.push('\n');
                index += 1;
            }
            byte => {
                value.push(char::from(byte));
                index += 1;
            }
        }
    }
    Err("unterminated string".to_string())
}

fn raw_string_start(bytes: &[u8], index: usize) -> Option<usize> {
    let mut cursor = index + 1;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'"')).then_some(cursor - index - 1)
}

fn raw_string(bytes: &[u8], index: usize, line: &mut usize) -> Result<(String, usize), String> {
    let hashes = raw_string_start(bytes, index).expect("checked raw string");
    let content_start = index + hashes + 2;
    let mut cursor = content_start;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\n' {
            *line += 1;
        }
        if bytes[cursor] == b'"'
            && bytes.get(cursor + 1..cursor + 1 + hashes) == Some(&vec![b'#'; hashes][..])
        {
            let value = String::from_utf8(bytes[content_start..cursor].to_vec())
                .map_err(|error| error.to_string())?;
            return Ok((value, cursor + 1 + hashes));
        }
        cursor += 1;
    }
    Err("unterminated raw string".to_string())
}

fn skip_char_or_lifetime(bytes: &[u8], index: usize, line: &mut usize) -> Result<usize, String> {
    let Some(next) = bytes.get(index + 1).copied() else {
        return Err("dangling apostrophe".to_string());
    };
    if next.is_ascii_alphabetic() || next == b'_' {
        let mut cursor = index + 2;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'\'') {
            return Ok(cursor);
        }
    }
    let mut cursor = index + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'\'' => return Ok(cursor + 1),
            b'\n' => {
                *line += 1;
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    Err("unterminated character literal".to_string())
}

#[cfg(test)]
mod scanner_tests {
    use super::*;

    #[test]
    fn lexer_ignores_calls_in_comments_and_strings() {
        let source = r#"
            // scoped("/comment", get(handler))
            let _ = "scoped(\"/string\", get(handler))";
            scoped("/live", get(handler));
        "#;
        let tokens = lex(source).expect("lex fixture");
        let calls = tokens
            .windows(3)
            .filter_map(
                |window| match (&window[0].kind, &window[1].kind, &window[2].kind) {
                    (TokenKind::Ident(name), TokenKind::Punct('('), TokenKind::String(path))
                        if name == "scoped" =>
                    {
                        Some(path.clone())
                    }
                    _ => None,
                },
            )
            .collect::<Vec<_>>();
        assert_eq!(calls, ["/live"]);
    }

    #[test]
    fn cfg_test_items_are_skipped_without_truncating_the_file() {
        let tokens = lex(r#"
                #[cfg(test)]
                fn only_test() { scoped("/test", get(handler)); }
                fn production() { scoped("/production", get(handler)); }
            "#)
        .expect("lex fixture");
        let skipped = test_only_token_indexes(&tokens);
        let visible = tokens
            .iter()
            .enumerate()
            .filter(|(index, _)| !skipped.contains(index))
            .any(|(_, token)| token.kind == TokenKind::String("/production".to_string()));
        let hidden = tokens
            .iter()
            .enumerate()
            .filter(|(index, _)| !skipped.contains(index))
            .any(|(_, token)| token.kind == TokenKind::String("/test".to_string()));
        assert!(visible);
        assert!(!hidden);
    }
}

#[test]
fn chained_route_verbs_are_all_recorded() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server");
    let setup = scan_file_route_literals(&root.join("setup.rs")).expect("scan");
    assert!(
        setup.direct_methods.contains("GET /api/v1/setup")
            && setup.direct_methods.contains("POST /api/v1/setup"),
        "chained get(read).post(apply) must yield both verbs, got {:?}",
        setup.direct_methods
    );
}

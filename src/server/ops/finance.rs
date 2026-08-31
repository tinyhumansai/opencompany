//! The finance **read plane**: the console's path to live Chargebee and PayPal
//! data (issue #788, #789; console design in
//! `docs/spec/runtime/finance-console.md`).
//!
//! # Why this module exists at all
//!
//! `crate::chargebee::api` and `crate::paypal::api` were reachable only from a
//! harness turn. Every operation the console's Finance section needs already
//! existed and had no HTTP address, so an operator could ask an agent "has Alan
//! paid?" and could not answer it themselves.
//!
//! # What it is not
//!
//! A **thin adapter**, deliberately: resolve the company's credentials, build
//! the provider client, call the existing `api` function, serialize its existing
//! projection. No new money types, no arithmetic, no caching, and no second
//! opinion about what a field means. If a projection is wrong for the console it
//! is wrong for the agent too, and the fix belongs in `api` where both callers
//! get it.
//!
//! The projections therefore serialize in **snake_case**, unlike the camelCase
//! DTOs the rest of the write plane returns. That is the point rather than an
//! oversight: an operator reading a `chargebee_get_invoice` result in chat and
//! the same invoice in the console sees one shape with one set of field names,
//! and `total_in_minor_units` keeps saying what unit it is in on both.
//!
//! # Three failures, three remedies
//!
//! [`FinanceError`] keeps them apart, for the reason `BillingStatus` reports
//! four flags instead of one `connected` boolean: the fixes are in three
//! different places.
//!
//! - `501 not_in_build` — the running host was compiled without the feature. No
//!   amount of configuring will help; the operator needs a different build.
//! - `409 not_configured` — no credential in this company's secret store. The
//!   fix is the connection panel on the page they are already looking at.
//! - `502 provider_error` — the credential reached the provider and the provider
//!   said no. The fix is at the provider, and its own `code` and `message` ride
//!   along so the console can say which.
//!
//! A locally rejected argument — an empty invoice id, a missing date window —
//! never reaches the network and is a `400`, not a `502`. The `api` functions
//! already mark these with `status: 0`, so the distinction is carried rather
//! than re-derived.
//!
//! # Scope
//!
//! Reads are [`ScopedCompany`]: any signed-in member. An invoice list is not
//! more sensitive than the ledger projection Finance's own Overview page
//! already shows them.
//!
//! `POST …/invoices` is [`AdminScopedCompany`], matching `PUT …/billing/chargebee`
//! — it bills a real customer real money, and there is no route here that undoes
//! it. A member may read every invoice and raise none.
//!
//! `POST …/test` is a read wearing a POST. It is not idempotent only in that it
//! costs a provider round-trip; the verb is what keeps a link preview, a
//! prefetch or a bookmarked URL from firing it.

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::server::error::ApiError;
use crate::server::ops::scope::{AdminScopedCompany, ScopedCompany, scoped};

/// Builds the finance read routes.
///
/// Every route is registered on **every** build, including one compiled without
/// `chargebee` or `paypal`. A missing feature answers `501 not_in_build` rather
/// than `404`, so the console can say "this host has no PayPal support" instead
/// of rendering a broken page — the same reasoning that keeps
/// `crate::company::paypal` always compiled.
pub fn router() -> Router<AppState> {
    scoped(
        "/finance/chargebee/invoices",
        get(list_invoices).post(create_invoice),
    )
    .merge(scoped(
        "/finance/chargebee/invoices/{invoice_id}",
        get(get_invoice),
    ))
    .merge(scoped("/finance/chargebee/customers", get(get_customer)))
    .merge(scoped("/finance/chargebee/test", post(test_chargebee)))
    .merge(scoped("/finance/paypal/balance", get(paypal_balance)))
    .merge(scoped(
        "/finance/paypal/transactions",
        get(paypal_transactions),
    ))
    .merge(scoped("/finance/paypal/test", post(test_paypal)))
}

/// What a `POST …/test` reports when the credential worked.
///
/// There is no `ok: false`. A failed check is the provider's own failure and
/// renders as a `502` carrying the provider's code and message — collapsing that
/// into a `200 {ok: false}` would throw away the only part of the answer an
/// operator can act on.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    /// Always `true`. Present so the body is self-describing rather than `{}`.
    pub ok: bool,
    /// What was actually verified, in the operator's terms — including which
    /// site or environment answered, because "Connected ✓" against the wrong
    /// world is the confusion this whole surface exists to avoid.
    pub detail: String,
}

/// Why a finance call could not be answered.
///
/// A module-local rejection rather than an [`ApiError`] arm, because the three
/// codes below are specific to this surface and the crate error should not grow
/// a variant per console page. Renders the same `{ "error", "code" }` envelope
/// every other route does, so a client needs no special case.
#[derive(Debug)]
pub enum FinanceError {
    /// The feature is not compiled into this build.
    NotInBuild {
        /// `chargebee` or `paypal`.
        provider: &'static str,
    },
    /// No credential is stored for this company.
    NotConfigured {
        /// `chargebee` or `paypal`.
        provider: &'static str,
    },
    /// The provider was reached and refused, or the call never left the process
    /// because an argument was rejected (`status == 0`).
    Provider {
        /// `chargebee` or `paypal`.
        provider: &'static str,
        /// The provider's HTTP status, or `0` for a locally rejected argument.
        status: u16,
        /// The provider's own error token.
        code: String,
        /// The provider's own description.
        message: String,
    },
    /// Anything else — a secret store that would not answer, a company that
    /// could not be resolved. Mapped by [`ApiError`] as it always was.
    Api(ApiError),
}

impl From<ApiError> for FinanceError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl FinanceError {
    /// Classifies an error out of a provider `api` call.
    ///
    /// Compiled only alongside a provider, since nothing calls it on a build
    /// with neither — the tests reach it directly, which is why this is a `cfg`
    /// rather than an `allow(dead_code)` that would also hide a real orphan.
    ///
    /// Only the provider's own variants become a [`FinanceError::Provider`].
    /// Anything else — a store failure that surfaced mid-call — keeps whatever
    /// status [`ApiError`] already gives it, rather than being relabelled as the
    /// provider's fault.
    #[cfg(any(feature = "chargebee", feature = "paypal", test))]
    fn from_provider(provider: &'static str, error: crate::error::OpenCompanyError) -> Self {
        match error {
            crate::error::OpenCompanyError::Chargebee {
                status,
                code,
                message,
            }
            | crate::error::OpenCompanyError::Paypal {
                status,
                code,
                message,
            } => Self::Provider {
                provider,
                status,
                code,
                message,
            },
            other => Self::Api(ApiError(other)),
        }
    }
}

impl IntoResponse for FinanceError {
    fn into_response(self) -> Response {
        let (status, code, message, extra) = match self {
            Self::NotInBuild { provider } => (
                StatusCode::NOT_IMPLEMENTED,
                "not_in_build",
                format!(
                    "This host was built without {provider} support, so there is nothing to \
                     configure. A build with the `{provider}` feature is needed."
                ),
                json!({ "provider": provider }),
            ),
            Self::NotConfigured { provider } => (
                StatusCode::CONFLICT,
                "not_configured",
                format!(
                    "No {provider} credentials are stored for this company. Connect the account \
                     first."
                ),
                json!({ "provider": provider }),
            ),
            // `status: 0` means the call never reached the network — the
            // argument was rejected here. That is the caller's bad request, not
            // an upstream failure, and calling it a 502 would send an operator
            // to check the provider's status page over a blank invoice id.
            Self::Provider {
                provider,
                status: 0,
                code,
                message,
            } => (
                StatusCode::BAD_REQUEST,
                "invalid_arguments",
                message,
                json!({ "provider": provider, "providerCode": code, "providerStatus": 0 }),
            ),
            Self::Provider {
                provider,
                status,
                code,
                message,
            } => (
                StatusCode::BAD_GATEWAY,
                "provider_error",
                message,
                json!({
                    "provider": provider,
                    "providerCode": code,
                    "providerStatus": status,
                }),
            ),
            Self::Api(error) => return error.into_response(),
        };

        let mut body = json!({ "error": message, "code": code });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        (status, Json(body)).into_response()
    }
}

/// Reads a stored secret, treating empty as absent.
///
/// The same rule as `billing::read`, and deliberately the same one: a key
/// written as `""` by a clear is *absent*, and a surface that treated it as
/// present would build a client that authenticates as nobody.
///
/// Only a provider half calls this, so it is compiled only when one is present.
#[cfg(any(feature = "chargebee", feature = "paypal"))]
async fn read(runtime: &CompanyRuntime, key: &str) -> Result<Option<String>, ApiError> {
    Ok(runtime
        .secrets()
        .get(runtime.id(), key)
        .await?
        .map(|value| value.expose().to_string())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

/// `?status=&customerEmail=&limit=` on the invoice list.
///
/// Still deserialized on a build without Chargebee — the route exists there and
/// answers `501` — so the fields are written and never read. That is the
/// `501`-not-`404` decision showing through, not an unused struct.
#[cfg_attr(not(feature = "chargebee"), allow(dead_code))]
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InvoiceQuery {
    /// Chargebee invoice status: `paid`, `posted`, `payment_due`, …
    #[serde(default)]
    status: Option<String>,
    /// Narrow to one customer, by email. Resolved to a customer id by `api`.
    #[serde(default)]
    customer_email: Option<String>,
    /// Page size, 1-100.
    #[serde(default)]
    limit: Option<i64>,
}

/// `?email=` on the customer lookup.
#[derive(Debug, Default, Deserialize)]
struct CustomerQuery {
    #[serde(default)]
    email: Option<String>,
}

/// `?since=&until=&limit=` on the transaction list.
///
/// Both instants are **required** and passed to PayPal verbatim. Defaulting them
/// here would mean a clock in this module and a second opinion about PayPal's
/// three-hour publication lag; the console owns the window because the console
/// is what has to explain it to the operator (see `finance-console.md`).
#[cfg_attr(not(feature = "paypal"), allow(dead_code))]
#[derive(Debug, Default, Deserialize)]
struct TransactionQuery {
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

// --- Chargebee ------------------------------------------------------------

/// `GET …/finance/chargebee/invoices`
async fn list_invoices(
    company: ScopedCompany,
    Query(query): Query<InvoiceQuery>,
) -> Result<Response, FinanceError> {
    chargebee::list_invoices(&company.runtime, query).await
}

/// `GET …/finance/chargebee/invoices/{invoice_id}`
async fn get_invoice(
    company: ScopedCompany,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> Result<Response, FinanceError> {
    // Both scope forms route here, and they carry different path params (`id` +
    // `invoice_id` versus `invoice_id` alone), so the invoice is read by name
    // rather than by tuple position.
    let invoice_id = params.get("invoice_id").cloned().unwrap_or_default();
    chargebee::get_invoice(&company.runtime, invoice_id).await
}

/// `GET …/finance/chargebee/customers?email=`
async fn get_customer(
    company: ScopedCompany,
    Query(query): Query<CustomerQuery>,
) -> Result<Response, FinanceError> {
    chargebee::get_customer(&company.runtime, query.email.unwrap_or_default()).await
}

/// `POST …/finance/chargebee/test`
async fn test_chargebee(company: ScopedCompany) -> Result<Json<TestResult>, FinanceError> {
    chargebee::test(&company.runtime).await
}

/// `POST …/finance/chargebee/invoices` — admin only. Bills a real customer.
async fn create_invoice(
    company: AdminScopedCompany,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, FinanceError> {
    chargebee::create_invoice(&company.runtime, body).await
}

// --- PayPal ---------------------------------------------------------------

/// `GET …/finance/paypal/balance`
async fn paypal_balance(company: ScopedCompany) -> Result<Response, FinanceError> {
    paypal::balance(&company.runtime).await
}

/// `GET …/finance/paypal/transactions?since=&until=&limit=`
async fn paypal_transactions(
    company: ScopedCompany,
    Query(query): Query<TransactionQuery>,
) -> Result<Response, FinanceError> {
    paypal::transactions(&company.runtime, query).await
}

/// `POST …/finance/paypal/test`
async fn test_paypal(company: ScopedCompany) -> Result<Json<TestResult>, FinanceError> {
    paypal::test(&company.runtime).await
}

// --- The provider halves, each present only when its feature is -----------
//
// Two modules per provider with one signature set, so the handlers above are
// written once and neither reads `cfg!`. The absent half answers
// `501 not_in_build` for every route, which is what makes a build without the
// feature a page that explains itself rather than a 404.

#[cfg(not(feature = "chargebee"))]
mod chargebee {
    use super::*;

    fn absent<T>() -> Result<T, FinanceError> {
        Err(FinanceError::NotInBuild {
            provider: "chargebee",
        })
    }

    pub(super) async fn list_invoices(
        _runtime: &CompanyRuntime,
        _query: InvoiceQuery,
    ) -> Result<Response, FinanceError> {
        absent()
    }
    pub(super) async fn get_invoice(
        _runtime: &CompanyRuntime,
        _invoice_id: String,
    ) -> Result<Response, FinanceError> {
        absent()
    }
    pub(super) async fn get_customer(
        _runtime: &CompanyRuntime,
        _email: String,
    ) -> Result<Response, FinanceError> {
        absent()
    }
    pub(super) async fn create_invoice(
        _runtime: &CompanyRuntime,
        _body: serde_json::Value,
    ) -> Result<Response, FinanceError> {
        absent()
    }
    pub(super) async fn test(_runtime: &CompanyRuntime) -> Result<Json<TestResult>, FinanceError> {
        absent()
    }
}

#[cfg(feature = "chargebee")]
mod chargebee {
    use super::*;

    use crate::chargebee::client::ChargebeeClient;
    use crate::chargebee::types::{
        API_KEY_SECRET, ChargebeeConfig, GetInvoiceArgs, ListInvoicesArgs, SITE_SECRET,
        SendInvoiceArgs,
    };

    /// Builds a client from the company's stored credentials.
    ///
    /// Both halves or neither, for the reason `TenantChargebee::resolve` gives:
    /// a site with no key cannot be called, and a key pointed at the wrong site
    /// fails in a way that reads like a bad key.
    async fn client(runtime: &CompanyRuntime) -> Result<(ChargebeeClient, String), FinanceError> {
        let (Some(site), Some(api_key)) = (
            read(runtime, SITE_SECRET).await?,
            read(runtime, API_KEY_SECRET).await?,
        ) else {
            return Err(FinanceError::NotConfigured {
                provider: "chargebee",
            });
        };
        let client = ChargebeeClient::new(ChargebeeConfig {
            site: site.clone(),
            api_key,
        })
        .map_err(|e| FinanceError::from_provider("chargebee", e))?;
        Ok((client, site))
    }

    /// Serializes a projection verbatim. See the module header on snake_case.
    fn ok<T: serde::Serialize>(value: T) -> Result<Response, FinanceError> {
        Ok(Json(serde_json::to_value(value).map_err(ApiError::from)?).into_response())
    }

    // Each operation is a pair: the route half, which resolves the company's
    // credentials, and an `_on` half taking a client. The split is what lets a
    // test drive the real serialization and the real error classification
    // against a stub transport (`ChargebeeClient::with_base_url`) without a
    // secret store standing in the way — and it keeps credential resolution in
    // exactly one place rather than at the top of five functions.

    pub(super) async fn list_invoices(
        runtime: &CompanyRuntime,
        query: InvoiceQuery,
    ) -> Result<Response, FinanceError> {
        let (client, _) = client(runtime).await?;
        list_invoices_on(&client, query).await
    }

    pub(super) async fn list_invoices_on(
        client: &ChargebeeClient,
        query: InvoiceQuery,
    ) -> Result<Response, FinanceError> {
        let invoices = crate::chargebee::api::list_invoices(
            client,
            ListInvoicesArgs {
                customer_email: query.customer_email,
                status: query.status,
                limit: query.limit,
            },
        )
        .await
        .map_err(|e| FinanceError::from_provider("chargebee", e))?;
        ok(invoices)
    }

    pub(super) async fn get_invoice(
        runtime: &CompanyRuntime,
        invoice_id: String,
    ) -> Result<Response, FinanceError> {
        let (client, _) = client(runtime).await?;
        get_invoice_on(&client, invoice_id).await
    }

    pub(super) async fn get_invoice_on(
        client: &ChargebeeClient,
        invoice_id: String,
    ) -> Result<Response, FinanceError> {
        let invoice = crate::chargebee::api::get_invoice(client, GetInvoiceArgs { invoice_id })
            .await
            .map_err(|e| FinanceError::from_provider("chargebee", e))?;
        ok(invoice)
    }

    pub(super) async fn get_customer(
        runtime: &CompanyRuntime,
        email: String,
    ) -> Result<Response, FinanceError> {
        let (client, _) = client(runtime).await?;
        let customer = crate::chargebee::api::get_customer(&client, &email)
            .await
            .map_err(|e| FinanceError::from_provider("chargebee", e))?;
        // `None` is a real answer — "nobody with that email" — not a 404. The
        // console renders it as an empty result on a filter, and a 404 would
        // read as a broken route.
        ok(customer)
    }

    pub(super) async fn create_invoice(
        runtime: &CompanyRuntime,
        body: serde_json::Value,
    ) -> Result<Response, FinanceError> {
        // Deserialized here rather than in the handler signature so a malformed
        // body is a `400` in this surface's own envelope, next to every other
        // rejection, instead of axum's bare rejection text.
        let args: SendInvoiceArgs =
            serde_json::from_value(body).map_err(|e| FinanceError::Provider {
                provider: "chargebee",
                status: 0,
                code: "invalid_arguments".to_string(),
                message: e.to_string(),
            })?;
        let (client, _) = client(runtime).await?;
        let invoice = crate::chargebee::api::send_invoice(&client, args)
            .await
            .map_err(|e| FinanceError::from_provider("chargebee", e))?;
        ok(invoice)
    }

    pub(super) async fn test(runtime: &CompanyRuntime) -> Result<Json<TestResult>, FinanceError> {
        let (client, site) = client(runtime).await?;
        test_on(&client, &site).await
    }

    pub(super) async fn test_on(
        client: &ChargebeeClient,
        site: &str,
    ) -> Result<Json<TestResult>, FinanceError> {
        // The cheapest authenticated call the API has: one invoice, which
        // proves the key, the site and the network in a single round trip
        // without creating anything.
        let invoices = crate::chargebee::api::list_invoices(
            client,
            ListInvoicesArgs {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| FinanceError::from_provider("chargebee", e))?;
        Ok(Json(TestResult {
            ok: true,
            detail: format!(
                "Authenticated against the {site} Chargebee site and listed {} invoice(s).",
                invoices.len()
            ),
        }))
    }
}

#[cfg(not(feature = "paypal"))]
mod paypal {
    use super::*;

    fn absent<T>() -> Result<T, FinanceError> {
        Err(FinanceError::NotInBuild { provider: "paypal" })
    }

    pub(super) async fn balance(_runtime: &CompanyRuntime) -> Result<Response, FinanceError> {
        absent()
    }
    pub(super) async fn transactions(
        _runtime: &CompanyRuntime,
        _query: TransactionQuery,
    ) -> Result<Response, FinanceError> {
        absent()
    }
    pub(super) async fn test(_runtime: &CompanyRuntime) -> Result<Json<TestResult>, FinanceError> {
        absent()
    }
}

#[cfg(feature = "paypal")]
mod paypal {
    use super::*;

    use crate::company::paypal::{
        CLIENT_ID_SECRET, CLIENT_SECRET_SECRET, ENVIRONMENT_SECRET, PaypalEnvironment,
    };
    use crate::paypal::client::{PaypalClient, PaypalConfig};

    /// Builds a client from the company's stored credentials.
    async fn client(
        runtime: &CompanyRuntime,
    ) -> Result<(PaypalClient, PaypalEnvironment), FinanceError> {
        let (Some(client_id), Some(client_secret)) = (
            read(runtime, CLIENT_ID_SECRET).await?,
            read(runtime, CLIENT_SECRET_SECRET).await?,
        ) else {
            return Err(FinanceError::NotConfigured { provider: "paypal" });
        };
        // Absent reads as sandbox, never live — the same default `PaypalEnvironment`
        // has, for the same reason: guessing wrong must read fake money rather
        // than point at real money.
        let environment = read(runtime, ENVIRONMENT_SECRET)
            .await?
            .map(|raw| PaypalEnvironment::parse(&raw))
            .unwrap_or_default();
        let client = PaypalClient::new(PaypalConfig {
            client_id,
            client_secret,
            environment,
        })
        .map_err(|e| FinanceError::from_provider("paypal", e))?;
        Ok((client, environment))
    }

    fn ok<T: serde::Serialize>(value: T) -> Result<Response, FinanceError> {
        Ok(Json(serde_json::to_value(value).map_err(ApiError::from)?).into_response())
    }

    pub(super) async fn balance(runtime: &CompanyRuntime) -> Result<Response, FinanceError> {
        let (client, _) = client(runtime).await?;
        let balances = crate::paypal::api::get_wallet_balance(&client)
            .await
            .map_err(|e| FinanceError::from_provider("paypal", e))?;
        ok(balances)
    }

    pub(super) async fn transactions(
        runtime: &CompanyRuntime,
        query: TransactionQuery,
    ) -> Result<Response, FinanceError> {
        let (client, _) = client(runtime).await?;
        // Passed through empty when absent: `api::list_transactions` already
        // rejects an empty window with the message that names PayPal's 31-day
        // cap and 3-hour lag, and duplicating that check here would give the
        // same mistake two different wordings.
        let transactions = crate::paypal::api::list_transactions(
            &client,
            query.since.as_deref().unwrap_or_default(),
            query.until.as_deref().unwrap_or_default(),
            query.limit,
        )
        .await
        .map_err(|e| FinanceError::from_provider("paypal", e))?;
        ok(transactions)
    }

    pub(super) async fn test(runtime: &CompanyRuntime) -> Result<Json<TestResult>, FinanceError> {
        let (client, environment) = client(runtime).await?;
        // A balance read exercises the whole chain: the token exchange with the
        // client id and secret, then a bearer call. A token fetch alone would
        // pass with a credential that has no reporting scope.
        let balances = crate::paypal::api::get_wallet_balance(&client)
            .await
            .map_err(|e| FinanceError::from_provider("paypal", e))?;
        Ok(Json(TestResult {
            ok: true,
            detail: format!(
                "Authenticated against PayPal {} and read {} currency balance(s).",
                environment.as_str(),
                balances.len()
            ),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::ports::types::CompanyId;

    async fn state_with_company(home: &std::path::Path) -> AppState {
        use crate::ports::CompanyStore;
        use crate::ports::types::CompanyRecord;

        let id = CompanyId::new("acme");
        let manifest: crate::company::CompanyManifest = ::toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .expect("manifest");
        crate::store::FsCompanyStore::new(home.to_path_buf())
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .expect("save");

        let runtime = crate::runtime::RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .expect("runtime");
        let state = AppState::new(crate::AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        state
    }

    async fn call(
        state: &AppState,
        method: &str,
        uri: &str,
        cookie: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie);
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .expect("request");
        let response = crate::server::router(state.clone())
            .oneshot(request)
            .await
            .expect("routed");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Every read route, so a new one cannot be added without deciding what it
    /// says on an unconfigured company.
    const READS: [&str; 4] = [
        "/api/v1/companies/acme/finance/chargebee/invoices",
        "/api/v1/companies/acme/finance/chargebee/customers?email=alan@example.com",
        "/api/v1/companies/acme/finance/paypal/balance",
        "/api/v1/companies/acme/finance/paypal/transactions?since=2026-01-01T00:00:00Z&until=2026-01-08T00:00:00Z",
    ];

    // --- The three failures stay three failures ---------------------------

    #[tokio::test]
    async fn an_unconfigured_company_is_a_409_naming_the_provider_not_a_500() {
        // The remedy is the connection panel on the page the operator is
        // already looking at, and a 500 sends them to the host logs instead.
        // Skipped on a build without the feature, where 501 is the right answer
        // and is asserted by its own test below.
        if !cfg!(feature = "chargebee") && !cfg!(feature = "paypal") {
            return;
        }
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path()).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        for uri in READS {
            let provider = if uri.contains("paypal") {
                "paypal"
            } else {
                "chargebee"
            };
            if (provider == "chargebee" && !cfg!(feature = "chargebee"))
                || (provider == "paypal" && !cfg!(feature = "paypal"))
            {
                continue;
            }
            let (status, answer) = call(&state, "GET", uri, &admin, None).await;
            assert_eq!(status, StatusCode::CONFLICT, "GET {uri}: {answer}");
            assert_eq!(answer["code"], "not_configured", "{uri}: {answer}");
            assert_eq!(answer["provider"], provider, "{uri}: {answer}");
        }
    }

    #[cfg(not(any(feature = "chargebee", feature = "paypal")))]
    #[tokio::test]
    async fn a_build_without_the_feature_answers_501_rather_than_404() {
        // A 404 reads as "wrong URL" and sends an operator looking for a typo.
        // 501 with `not_in_build` is what lets the console say "this host has no
        // PayPal support" — the reason every route is registered on every build.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path()).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        for uri in READS {
            let (status, answer) = call(&state, "GET", uri, &admin, None).await;
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "GET {uri}: {answer}");
            assert_eq!(answer["code"], "not_in_build", "{uri}: {answer}");
        }
    }

    #[tokio::test]
    async fn a_member_may_read_every_invoice_and_raise_none() {
        // The asymmetry this surface is built around: reading is not more
        // sensitive than the ledger Finance already shows a member, and raising
        // an invoice bills a real customer with no route here that undoes it.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path()).await;
        let member = crate::server::test_support::seed_session(
            &state,
            "acme",
            crate::ports::users::UserRole::Member,
        )
        .await;

        // A member reaching the handler is what matters, not what the handler
        // then says: unconfigured (409) or not-in-build (501) both mean the
        // scope let them through. A 401/403 would mean it did not.
        for uri in READS {
            let (status, answer) = call(&state, "GET", uri, &member, None).await;
            assert!(
                status != StatusCode::FORBIDDEN && status != StatusCode::UNAUTHORIZED,
                "GET {uri} refused a member: {status} {answer}"
            );
        }

        let (status, answer) = call(
            &state,
            "POST",
            "/api/v1/companies/acme/finance/chargebee/invoices",
            &member,
            Some(json!({
                "customer_email": "alan@example.com",
                "currency_code": "USD",
                "line_items": [{"description": "Consulting", "amount_in_minor_units": 125000}],
            })),
        )
        .await;
        assert!(
            status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED,
            "a member raised an invoice: {status} {answer}"
        );
    }

    // --- Error classification ---------------------------------------------
    //
    // Unit tests on the rendering, because the three-way split is the whole
    // contract of this module and a route test can only reach one arm at a time.

    async fn rendered(error: FinanceError) -> (StatusCode, Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn a_provider_refusal_is_a_502_carrying_the_providers_own_code() {
        // The operator's next step is at Chargebee, and `configuration_incompatible`
        // is what tells them which setting. A generic 502 would hide it.
        let (status, body) = rendered(FinanceError::from_provider(
            "chargebee",
            crate::error::OpenCompanyError::Chargebee {
                status: 400,
                code: "configuration_incompatible".to_string(),
                message: "Currency GBP is not enabled for this site".to_string(),
            },
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["code"], "provider_error");
        assert_eq!(body["providerCode"], "configuration_incompatible");
        assert_eq!(body["providerStatus"], 400);
        assert_eq!(body["provider"], "chargebee");
        assert!(
            body["error"].as_str().expect("error").contains("GBP"),
            "the provider's own message must survive: {body}"
        );
    }

    #[tokio::test]
    async fn an_argument_rejected_before_the_network_is_a_400_not_a_502() {
        // `status: 0` is the `api` layer saying the call never left the process.
        // Rendering that as a 502 would send an operator to check PayPal's
        // status page over a date range they can fix themselves — and PayPal's
        // rewritten window message is exactly the text they need to see.
        let (status, body) = rendered(FinanceError::from_provider(
            "paypal",
            crate::error::OpenCompanyError::Paypal {
                status: 0,
                code: "invalid_arguments".to_string(),
                message: "`start_date` and `end_date` are both required".to_string(),
            },
        ))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_arguments");
        assert_eq!(body["providerStatus"], 0);
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("start_date")
        );
    }

    #[tokio::test]
    async fn a_store_failure_is_not_relabelled_as_the_providers_fault() {
        // A secret store that will not answer is a 500 here, as it is
        // everywhere else. Calling it `provider_error` would blame Chargebee for
        // a local fault and send the operator to the wrong status page.
        let (status, body) = rendered(FinanceError::from_provider(
            "chargebee",
            crate::error::OpenCompanyError::Store("disk full".to_string()),
        ))
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_ne!(body["code"], "provider_error");
    }

    #[tokio::test]
    async fn not_in_build_and_not_configured_are_different_answers() {
        // One means "get a different build", the other "fill in this form".
        // A surface that merged them would offer the form on a host that cannot
        // use it.
        let (status, body) = rendered(FinanceError::NotInBuild { provider: "paypal" }).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["code"], "not_in_build");

        let (status, body) = rendered(FinanceError::NotConfigured { provider: "paypal" }).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "not_configured");
    }

    // --- The projection reaches the wire unchanged ------------------------

    /// A one-route Chargebee stub, so the happy path exercises the real client,
    /// the real `api` projection and the real serialization.
    #[cfg(feature = "chargebee")]
    async fn stub_chargebee(path: &'static str, reply: Value) -> crate::chargebee::ChargebeeClient {
        use crate::chargebee::types::ChargebeeConfig;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = axum::Router::new().route(
            path,
            axum::routing::get(move || async move { axum::Json(reply) }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        crate::chargebee::ChargebeeClient::with_base_url(
            ChargebeeConfig {
                site: "acme-test".to_string(),
                api_key: "cb_test_key".to_string(),
            },
            format!("http://{addr}"),
        )
        .expect("client")
    }

    #[cfg(feature = "chargebee")]
    #[tokio::test]
    async fn an_invoice_list_reaches_the_console_in_minor_units_and_snake_case() {
        // Both halves matter. The unit, because a console that received a field
        // called plain `total` would render $1,250.00 as $125,000.00 or the
        // reverse and nothing on the wire would say which. The case, because it
        // is the same shape the agent sees from `chargebee_list_invoices` — one
        // invoice, one vocabulary, whether it is read in chat or in the console.
        let client = stub_chargebee(
            "/invoices",
            json!({
                "list": [{
                    "invoice": {
                        "id": "INV-0042",
                        "customer_id": "cus_7",
                        "status": "paid",
                        "currency_code": "USD",
                        "total": 125000,
                        "amount_due": 0,
                        "amount_paid": 125000,
                        "line_items": [{"description": "Consulting"}],
                    }
                }]
            }),
        )
        .await;

        let response = chargebee::list_invoices_on(&client, InvoiceQuery::default())
            .await
            .expect("listed");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let body: Value = serde_json::from_slice(&bytes).expect("json");

        assert_eq!(body[0]["id"], "INV-0042");
        assert_eq!(body[0]["total_in_minor_units"], 125000);
        assert_eq!(body[0]["amount_due_in_minor_units"], 0);
        assert_eq!(body[0]["status"], "paid");
        assert!(
            body[0].get("total").is_none(),
            "a bare `total` would hide the unit: {body}"
        );
    }

    #[cfg(feature = "chargebee")]
    #[tokio::test]
    async fn a_connection_test_names_the_site_that_actually_answered() {
        // "Connected ✓" against the wrong site is the confusion the whole
        // status surface exists to avoid, and a test result that said only
        // "OK" would reintroduce it here.
        let client = stub_chargebee("/invoices", json!({ "list": [] })).await;
        let result = chargebee::test_on(&client, "acme-test").await.expect("ok");
        assert!(result.ok);
        assert!(
            result.detail.contains("acme-test"),
            "the site must be named: {}",
            result.detail
        );
    }
}

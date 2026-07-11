//! The [`TinyplaceClient`] REST seam: DTOs, an offline mock, and the reqwest
//! transport.
//!
//! [`TinyplaceEconomy`](super::adapter::TinyplaceEconomy) speaks to tiny.place
//! only through this trait, so its behaviour is exercised offline against
//! [`MockTinyplaceClient`] — a pure in-memory double with scripted responses and
//! a call log. [`HttpTinyplaceClient`] is the real transport over
//! [`reqwest`], attaching a SIWX `Authorization` header to every request; it is
//! a compilable scaffold reconciled against the live server in one place (the
//! wire DTOs and [`sha256_hex`] body digest).
//!
//! Wire camelCase reconciliation is isolated per-DTO here via `#[serde(rename)]`;
//! the [`AgentCard`](crate::ports::types::AgentCard) and
//! [`X402Authorization`](super::x402::X402Authorization) shapes are reused
//! verbatim.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::Result;
use crate::economy::signer::LocalSigner;
use crate::economy::siwx;
use crate::economy::x402::{X402Authorization, X402Challenge};
use crate::error::OpenCompanyError;
use crate::ports::generate_id;
use crate::ports::now_millis;
use crate::ports::types::{AgentAddr, AgentCard};

/// Current wall-clock time as epoch **seconds** (the SIWX/x402 timestamp unit).
pub(crate) fn now_secs() -> i64 {
    (now_millis() / 1000) as i64
}

// ---------------------------------------------------------------------------
// Wire DTOs
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request, the A2A `tasks/send` envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// The request id (echoed in the response).
    pub id: String,
    /// The RPC method, e.g. `"tasks/send"`.
    pub method: String,
    /// The method params.
    pub params: Value,
}

impl JsonRpcRequest {
    /// Builds a `2.0` request with a fresh id.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: generate_id(),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Echoes the request id.
    pub id: String,
    /// The successful result payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The error payload, if the call failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

impl JsonRpcResponse {
    /// Builds a success response echoing `id`.
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: id.into(),
            result: Some(result),
            error: None,
        }
    }
}

/// A directory search filter (`skill` and/or free-form `tag`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DirectoryQuery {
    /// Filter by advertised skill id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// Filter by discovery tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// One row of a `/directory/skills` search.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectorySkill {
    /// The advertising agent's id.
    #[serde(rename = "agentId", alias = "agent_id")]
    pub agent_id: String,
    /// The advertised skill id.
    #[serde(rename = "skillId", alias = "skill_id")]
    pub skill_id: String,
    /// The decimal price string, e.g. `"25.00"`.
    pub price: String,
}

/// The outcome of a paid mutating call: either it completed, or the server
/// answered `402` with a payment challenge.
#[derive(Clone, Debug, PartialEq)]
pub enum PaidOutcome<T> {
    /// The call completed and returned `T`.
    Done(T),
    /// The server requires payment; here is the challenge.
    PaymentRequired(X402Challenge),
}

/// A receipt for a name registration or renewal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegistryReceipt {
    /// The registry record id.
    pub id: String,
    /// The registered agent address.
    pub addr: AgentAddr,
    /// The fee charged, in USD.
    #[serde(default, rename = "feeUsd", alias = "fee_usd")]
    pub fee_usd: f64,
}

/// The result of a `/payments/verify` call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyOutcome {
    /// Whether the authorization verified.
    pub ok: bool,
    /// Why it failed, when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The receipt from a `/payments/settle` call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettleReceipt {
    /// The settlement receipt id.
    #[serde(rename = "receiptId", alias = "receipt_id")]
    pub receipt_id: String,
    /// The settled amount, as a decimal string.
    pub amount: String,
}

// ---------------------------------------------------------------------------
// The transport seam
// ---------------------------------------------------------------------------

/// The tiny.place REST transport seam.
///
/// One method per endpoint. Mutating calls that may be gated behind payment
/// return [`PaidOutcome`] so the adapter can catch a `402`, budget-check, and
/// resubmit with an [`X402Authorization`].
#[async_trait]
pub trait TinyplaceClient: Send + Sync {
    /// `POST /registry/names` — claim a `@handle`.
    async fn register_name(&self, label: &str) -> Result<PaidOutcome<RegistryReceipt>>;
    /// `POST /registry/names` with a payment authorization attached.
    async fn register_name_paid(
        &self,
        label: &str,
        auth: &X402Authorization,
    ) -> Result<RegistryReceipt>;
    /// `POST /registry/names/{id}/renew` with a payment authorization.
    async fn renew_name(&self, id: &str, auth: &X402Authorization) -> Result<RegistryReceipt>;
    /// `PUT /directory/agents/{id}` — publish or refresh an Agent Card.
    async fn put_agent(&self, agent_id: &str, card: &AgentCard) -> Result<()>;
    /// `GET /directory/agents` — list matching Agent Cards.
    async fn list_agents(&self, query: &DirectoryQuery) -> Result<Vec<AgentCard>>;
    /// `GET /directory/skills` — list matching priced skills.
    async fn list_skills(&self, query: &DirectoryQuery) -> Result<Vec<DirectorySkill>>;
    /// `GET /directory/resolve/{name}` — resolve a `@handle` to an address.
    async fn resolve(&self, name: &str) -> Result<AgentAddr>;
    /// `POST /a2a/{id}` — send a JSON-RPC task.
    async fn send_task(
        &self,
        agent_id: &str,
        rpc: JsonRpcRequest,
    ) -> Result<PaidOutcome<JsonRpcResponse>>;
    /// `POST /a2a/{id}` with a payment authorization attached.
    async fn send_task_paid(
        &self,
        agent_id: &str,
        rpc: JsonRpcRequest,
        auth: &X402Authorization,
    ) -> Result<JsonRpcResponse>;
    /// `POST /payments/verify` — verify a payment authorization.
    async fn payments_verify(&self, auth: &X402Authorization) -> Result<VerifyOutcome>;
    /// `POST /payments/settle` — settle a verified authorization.
    async fn payments_settle(&self, auth: &X402Authorization) -> Result<SettleReceipt>;
}

// ---------------------------------------------------------------------------
// The offline mock
// ---------------------------------------------------------------------------

/// A network-free [`TinyplaceClient`] double for offline tests.
///
/// Every response is scripted via the `with_*` setters; a call log and per-call
/// counters record what the adapter did. Setting [`Self::set_reachable`] to
/// `false` makes every method fail with `tinyplace("unreachable", …)`, driving
/// the outbox path.
pub struct MockTinyplaceClient {
    state: Mutex<MockState>,
    settle_calls: AtomicUsize,
    verify_calls: AtomicUsize,
}

struct MockState {
    reachable: bool,
    resolve: Option<AgentAddr>,
    register_name: PaidOutcome<RegistryReceipt>,
    register_paid: RegistryReceipt,
    renew: RegistryReceipt,
    send_task: PaidOutcome<JsonRpcResponse>,
    send_task_paid: JsonRpcResponse,
    verify_ok: bool,
    verify_reason: Option<String>,
    settle: SettleReceipt,
    agents: Vec<AgentCard>,
    skills: Vec<DirectorySkill>,
    log: Vec<String>,
}

impl Default for MockTinyplaceClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTinyplaceClient {
    /// Builds a reachable mock whose scripted responses all succeed with
    /// placeholder values. Override any of them with the `with_*` setters.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MockState {
                reachable: true,
                resolve: None,
                register_name: PaidOutcome::Done(RegistryReceipt {
                    id: "reg-1".into(),
                    addr: AgentAddr("MockAddr".into()),
                    fee_usd: 0.0,
                }),
                register_paid: RegistryReceipt {
                    id: "reg-1".into(),
                    addr: AgentAddr("MockAddr".into()),
                    fee_usd: 0.0,
                },
                renew: RegistryReceipt {
                    id: "reg-1".into(),
                    addr: AgentAddr("MockAddr".into()),
                    fee_usd: 0.0,
                },
                send_task: PaidOutcome::Done(JsonRpcResponse::ok(
                    "t1",
                    serde_json::json!({ "id": "task-1", "status": "accepted" }),
                )),
                send_task_paid: JsonRpcResponse::ok(
                    "t1",
                    serde_json::json!({ "id": "task-1", "status": "accepted" }),
                ),
                verify_ok: true,
                verify_reason: None,
                settle: SettleReceipt {
                    receipt_id: "settle-1".into(),
                    amount: "0.00".into(),
                },
                agents: Vec::new(),
                skills: Vec::new(),
                log: Vec::new(),
            }),
            settle_calls: AtomicUsize::new(0),
            verify_calls: AtomicUsize::new(0),
        }
    }

    /// Sets whether the mock is reachable. When `false`, every call fails with
    /// `tinyplace("unreachable", …)`.
    pub fn set_reachable(&self, reachable: bool) {
        self.lock().reachable = reachable;
    }

    /// Scripts what `resolve` returns. `None` (the default) resolves to a
    /// `not_found` error.
    pub fn with_resolve(self, addr: Option<AgentAddr>) -> Self {
        self.lock().resolve = addr;
        self
    }

    /// Scripts the `register_name` outcome (a `Done` receipt or a `402`
    /// `PaymentRequired` challenge).
    pub fn with_register_name(self, outcome: PaidOutcome<RegistryReceipt>) -> Self {
        self.lock().register_name = outcome;
        self
    }

    /// Scripts the receipt returned by `register_name_paid`.
    pub fn with_register_paid(self, receipt: RegistryReceipt) -> Self {
        self.lock().register_paid = receipt;
        self
    }

    /// Scripts the `send_task` outcome (a `Done` response or a `402` challenge).
    pub fn with_send_task(self, outcome: PaidOutcome<JsonRpcResponse>) -> Self {
        self.lock().send_task = outcome;
        self
    }

    /// Scripts the response returned by `send_task_paid`.
    pub fn with_send_task_paid(self, resp: JsonRpcResponse) -> Self {
        self.lock().send_task_paid = resp;
        self
    }

    /// Scripts the `payments_verify` outcome.
    pub fn with_verify(self, ok: bool, reason: Option<String>) -> Self {
        {
            let mut state = self.lock();
            state.verify_ok = ok;
            state.verify_reason = reason;
        }
        self
    }

    /// Scripts the `list_agents` result.
    pub fn with_agents(self, agents: Vec<AgentCard>) -> Self {
        self.lock().agents = agents;
        self
    }

    /// Scripts the `list_skills` result.
    pub fn with_skills(self, skills: Vec<DirectorySkill>) -> Self {
        self.lock().skills = skills;
        self
    }

    /// How many times `payments_settle` was called.
    pub fn settle_calls(&self) -> usize {
        self.settle_calls.load(Ordering::Relaxed)
    }

    /// How many times `payments_verify` was called.
    pub fn verify_calls(&self) -> usize {
        self.verify_calls.load(Ordering::Relaxed)
    }

    /// The recorded call log (method names, in call order).
    pub fn calls(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    /// How many times a method matching `needle` appears in the call log.
    pub fn count(&self, needle: &str) -> usize {
        self.lock().log.iter().filter(|c| *c == needle).count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().expect("mock poisoned")
    }

    fn record(&self, method: &str) -> Result<()> {
        let mut state = self.lock();
        state.log.push(method.to_string());
        if state.reachable {
            Ok(())
        } else {
            Err(OpenCompanyError::tinyplace(
                "unreachable",
                format!("tiny.place is unreachable ({method})"),
            ))
        }
    }
}

#[async_trait]
impl TinyplaceClient for MockTinyplaceClient {
    async fn register_name(&self, _label: &str) -> Result<PaidOutcome<RegistryReceipt>> {
        self.record("register_name")?;
        Ok(self.lock().register_name.clone())
    }

    async fn register_name_paid(
        &self,
        _label: &str,
        _auth: &X402Authorization,
    ) -> Result<RegistryReceipt> {
        self.record("register_name_paid")?;
        Ok(self.lock().register_paid.clone())
    }

    async fn renew_name(&self, _id: &str, _auth: &X402Authorization) -> Result<RegistryReceipt> {
        self.record("renew_name")?;
        Ok(self.lock().renew.clone())
    }

    async fn put_agent(&self, _agent_id: &str, _card: &AgentCard) -> Result<()> {
        self.record("put_agent")
    }

    async fn list_agents(&self, _query: &DirectoryQuery) -> Result<Vec<AgentCard>> {
        self.record("list_agents")?;
        Ok(self.lock().agents.clone())
    }

    async fn list_skills(&self, _query: &DirectoryQuery) -> Result<Vec<DirectorySkill>> {
        self.record("list_skills")?;
        Ok(self.lock().skills.clone())
    }

    async fn resolve(&self, name: &str) -> Result<AgentAddr> {
        self.record("resolve")?;
        self.lock().resolve.clone().ok_or_else(|| {
            OpenCompanyError::tinyplace("not_found", format!("no registration for @{name}"))
        })
    }

    async fn send_task(
        &self,
        _agent_id: &str,
        _rpc: JsonRpcRequest,
    ) -> Result<PaidOutcome<JsonRpcResponse>> {
        self.record("send_task")?;
        Ok(self.lock().send_task.clone())
    }

    async fn send_task_paid(
        &self,
        _agent_id: &str,
        _rpc: JsonRpcRequest,
        _auth: &X402Authorization,
    ) -> Result<JsonRpcResponse> {
        self.record("send_task_paid")?;
        Ok(self.lock().send_task_paid.clone())
    }

    async fn payments_verify(&self, _auth: &X402Authorization) -> Result<VerifyOutcome> {
        self.verify_calls.fetch_add(1, Ordering::Relaxed);
        self.record("payments_verify")?;
        let state = self.lock();
        Ok(VerifyOutcome {
            ok: state.verify_ok,
            reason: state.verify_reason.clone(),
        })
    }

    async fn payments_settle(&self, _auth: &X402Authorization) -> Result<SettleReceipt> {
        self.settle_calls.fetch_add(1, Ordering::Relaxed);
        self.record("payments_settle")?;
        Ok(self.lock().settle.clone())
    }
}

// ---------------------------------------------------------------------------
// The networked transport
// ---------------------------------------------------------------------------

/// The reqwest-backed [`TinyplaceClient`], gated behind the `tinyplace` feature.
///
/// Attaches a SIWX `Authorization` header (signed over method, path, and a
/// [`sha256_hex`] body digest) to every request. A compilable scaffold: the DTO
/// shapes and digest are the single reconciliation point against the live
/// server. All behavioural coverage runs against [`MockTinyplaceClient`].
pub struct HttpTinyplaceClient {
    client: reqwest::Client,
    base_url: String,
    signer: std::sync::Arc<LocalSigner>,
}

impl HttpTinyplaceClient {
    /// Builds a client against `base_url` (e.g. `https://api.tiny.place`)
    /// signing with `signer`.
    pub fn new(base_url: impl Into<String>, signer: std::sync::Arc<LocalSigner>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            signer,
        }
    }

    /// The company's base58 `agentId`.
    pub fn agent_id(&self) -> String {
        self.signer.agent_id()
    }

    /// Sends a request with a SIWX header, returning the status and decoded body.
    ///
    /// A network failure maps to `tinyplace("unreachable", …)`; a body that will
    /// not decode maps to `tinyplace("decode", …)`.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(reqwest::StatusCode, Value)> {
        let bytes = match body {
            Some(v) => serde_json::to_vec(v)?,
            None => Vec::new(),
        };
        let body_hash = sha256_hex(&bytes);
        let header = siwx::header_value(&siwx::build_header(
            &self.signer,
            &siwx::SiwxPayload {
                method: method.as_str(),
                path,
                timestamp: now_secs(),
                body_hash: &body_hash,
            },
        ));

        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let mut req = self
            .client
            .request(method, &url)
            .header("Authorization", header);
        if !bytes.is_empty() {
            req = req.header("Content-Type", "application/json").body(bytes);
        }

        let resp = req
            .send()
            .await
            .map_err(|err| OpenCompanyError::tinyplace("unreachable", format!("{path}: {err}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|err| OpenCompanyError::tinyplace("decode", format!("{path}: {err}")))?;
        let value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text)
                .map_err(|err| OpenCompanyError::tinyplace("decode", format!("{path}: {err}")))?
        };
        Ok((status, value))
    }

    /// Rejects a non-success, non-402 status.
    fn require_success(status: reqwest::StatusCode, value: &Value, path: &str) -> Result<()> {
        if status.is_success() {
            return Ok(());
        }
        let message = value
            .get("error")
            .and_then(|e| e.as_str())
            .or_else(|| value.get("message").and_then(|m| m.as_str()))
            .unwrap_or("request failed")
            .to_string();
        Err(OpenCompanyError::tinyplace(
            format!("http_{}", status.as_u16()),
            format!("{path}: {message}"),
        ))
    }

    fn decode<T: serde::de::DeserializeOwned>(value: Value, path: &str) -> Result<T> {
        serde_json::from_value(value)
            .map_err(|err| OpenCompanyError::tinyplace("decode", format!("{path}: {err}")))
    }
}

impl std::fmt::Debug for HttpTinyplaceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTinyplaceClient")
            .field("base_url", &self.base_url)
            .field("agent_id", &self.signer.agent_id())
            .field("signer", &"<redacted>")
            .finish()
    }
}

#[async_trait]
impl TinyplaceClient for HttpTinyplaceClient {
    async fn register_name(&self, label: &str) -> Result<PaidOutcome<RegistryReceipt>> {
        let body = serde_json::json!({ "label": label });
        let path = "/registry/names";
        let (status, value) = self.send(reqwest::Method::POST, path, Some(&body)).await?;
        if status.as_u16() == 402 {
            return Ok(PaidOutcome::PaymentRequired(X402Challenge::from_body(
                &value,
            )?));
        }
        Self::require_success(status, &value, path)?;
        Ok(PaidOutcome::Done(Self::decode(value, path)?))
    }

    async fn register_name_paid(
        &self,
        label: &str,
        auth: &X402Authorization,
    ) -> Result<RegistryReceipt> {
        let body = serde_json::json!({ "label": label, "payment": auth });
        let path = "/registry/names";
        let (status, value) = self.send(reqwest::Method::POST, path, Some(&body)).await?;
        Self::require_success(status, &value, path)?;
        Self::decode(value, path)
    }

    async fn renew_name(&self, id: &str, auth: &X402Authorization) -> Result<RegistryReceipt> {
        let body = serde_json::json!({ "payment": auth });
        let path = format!("/registry/names/{id}/renew");
        let (status, value) = self.send(reqwest::Method::POST, &path, Some(&body)).await?;
        Self::require_success(status, &value, &path)?;
        Self::decode(value, &path)
    }

    async fn put_agent(&self, agent_id: &str, card: &AgentCard) -> Result<()> {
        let body = serde_json::to_value(card)?;
        let path = format!("/directory/agents/{agent_id}");
        let (status, value) = self.send(reqwest::Method::PUT, &path, Some(&body)).await?;
        Self::require_success(status, &value, &path)
    }

    async fn list_agents(&self, query: &DirectoryQuery) -> Result<Vec<AgentCard>> {
        let path = format!("/directory/agents{}", query_string(query));
        let (status, value) = self.send(reqwest::Method::GET, &path, None).await?;
        Self::require_success(status, &value, &path)?;
        Self::decode(value, &path)
    }

    async fn list_skills(&self, query: &DirectoryQuery) -> Result<Vec<DirectorySkill>> {
        let path = format!("/directory/skills{}", query_string(query));
        let (status, value) = self.send(reqwest::Method::GET, &path, None).await?;
        Self::require_success(status, &value, &path)?;
        Self::decode(value, &path)
    }

    async fn resolve(&self, name: &str) -> Result<AgentAddr> {
        let path = format!("/directory/resolve/{name}");
        let (status, value) = self.send(reqwest::Method::GET, &path, None).await?;
        Self::require_success(status, &value, &path)?;
        // Accept either a bare string or `{ "addr": "…" }`.
        if let Some(addr) = value.as_str() {
            return Ok(AgentAddr(addr.to_string()));
        }
        if let Some(addr) = value.get("addr").and_then(|a| a.as_str()) {
            return Ok(AgentAddr(addr.to_string()));
        }
        Self::decode(value, &path)
    }

    async fn send_task(
        &self,
        agent_id: &str,
        rpc: JsonRpcRequest,
    ) -> Result<PaidOutcome<JsonRpcResponse>> {
        let body = serde_json::to_value(&rpc)?;
        let path = format!("/a2a/{agent_id}");
        let (status, value) = self.send(reqwest::Method::POST, &path, Some(&body)).await?;
        if status.as_u16() == 402 {
            return Ok(PaidOutcome::PaymentRequired(X402Challenge::from_body(
                &value,
            )?));
        }
        Self::require_success(status, &value, &path)?;
        Ok(PaidOutcome::Done(Self::decode(value, &path)?))
    }

    async fn send_task_paid(
        &self,
        agent_id: &str,
        rpc: JsonRpcRequest,
        auth: &X402Authorization,
    ) -> Result<JsonRpcResponse> {
        let mut body = serde_json::to_value(&rpc)?;
        if let Value::Object(map) = &mut body {
            map.insert("payment".to_string(), serde_json::to_value(auth)?);
        }
        let path = format!("/a2a/{agent_id}");
        let (status, value) = self.send(reqwest::Method::POST, &path, Some(&body)).await?;
        Self::require_success(status, &value, &path)?;
        Self::decode(value, &path)
    }

    async fn payments_verify(&self, auth: &X402Authorization) -> Result<VerifyOutcome> {
        let body = serde_json::to_value(auth)?;
        let path = "/payments/verify";
        let (status, value) = self.send(reqwest::Method::POST, path, Some(&body)).await?;
        Self::require_success(status, &value, path)?;
        Self::decode(value, path)
    }

    async fn payments_settle(&self, auth: &X402Authorization) -> Result<SettleReceipt> {
        let body = serde_json::to_value(auth)?;
        let path = "/payments/settle";
        let (status, value) = self.send(reqwest::Method::POST, path, Some(&body)).await?;
        Self::require_success(status, &value, path)?;
        Self::decode(value, path)
    }
}

/// Renders a `DirectoryQuery` as a URL query suffix (`""` when both are unset).
fn query_string(query: &DirectoryQuery) -> String {
    let mut parts = Vec::new();
    if let Some(skill) = &query.skill {
        parts.push(format!("skill={skill}"));
    }
    if let Some(tag) = &query.tag {
        parts.push(format!("tag={tag}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

// ---------------------------------------------------------------------------
// Dependency-free SHA-256 (matches the `signer.rs` "avoid pulling a crate"
// precedent). Pins the SIWX body digest to sha256-hex in one isolated spot.
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The lowercase hex SHA-256 digest of `data`.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, slot) in w.iter_mut().enumerate().take(16) {
            let b = i * 4;
            *slot = u32::from_be_bytes([chunk[b], chunk[b + 1], chunk[b + 2], chunk[b + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        for byte in word.to_be_bytes() {
            let _ = write!(out, "{byte:02x}");
        }
    }
    out
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn json_rpc_request_round_trips_with_version() {
        let rpc = JsonRpcRequest::new("tasks/send", serde_json::json!({ "skill": "seo.audit" }));
        assert_eq!(rpc.jsonrpc, "2.0");
        let json = serde_json::to_string(&rpc).expect("serialize");
        let back: JsonRpcRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, rpc);
    }

    #[test]
    fn directory_skill_decodes_camel_and_snake_case() {
        let camel = serde_json::json!({
            "agentId": "AgentX", "skillId": "seo.audit", "price": "25.00"
        });
        let snake = serde_json::json!({
            "agent_id": "AgentX", "skill_id": "seo.audit", "price": "25.00"
        });
        let a: DirectorySkill = serde_json::from_value(camel).expect("camel");
        let b: DirectorySkill = serde_json::from_value(snake).expect("snake");
        assert_eq!(a, b);
        assert_eq!(a.agent_id, "AgentX");
    }

    #[test]
    fn paid_outcome_decodes_402_accepts_envelope() {
        // The x402 `{ accepts: [ … ] }` envelope decodes into a challenge.
        let body = serde_json::json!({
            "accepts": [ { "maxAmountRequired": "25.00", "payTo": "Recipient" } ]
        });
        let challenge = X402Challenge::from_body(&body).expect("challenge");
        let outcome: PaidOutcome<RegistryReceipt> = PaidOutcome::PaymentRequired(challenge.clone());
        assert_eq!(outcome, PaidOutcome::PaymentRequired(challenge));
    }

    #[test]
    fn registry_receipt_decodes_from_wire() {
        let value = serde_json::json!({ "id": "r1", "addr": "AddrX", "feeUsd": 25.0 });
        let receipt: RegistryReceipt = serde_json::from_value(value).expect("decode");
        assert_eq!(receipt.addr, AgentAddr("AddrX".into()));
        assert_eq!(receipt.fee_usd, 25.0);
    }

    #[tokio::test]
    async fn mock_records_calls_and_honors_reachable() {
        let mock = MockTinyplaceClient::new().with_resolve(Some(AgentAddr("Me".into())));
        assert_eq!(mock.resolve("acme").await.unwrap(), AgentAddr("Me".into()));
        assert_eq!(mock.count("resolve"), 1);

        mock.set_reachable(false);
        let err = mock.resolve("acme").await.unwrap_err();
        assert_eq!(err.code(), "tinyplace_unreachable");
        // Both attempts are logged even though the second failed.
        assert_eq!(mock.count("resolve"), 2);
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn query_string_builds_suffix() {
        assert_eq!(query_string(&DirectoryQuery::default()), "");
        assert_eq!(
            query_string(&DirectoryQuery {
                skill: Some("seo.audit".into()),
                tag: None,
            }),
            "?skill=seo.audit"
        );
    }
}

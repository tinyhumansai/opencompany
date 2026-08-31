//! [`TinyplaceEconomy`]: the [`AgentEconomy`] adapter over a [`TinyplaceClient`].
//!
//! This is the commerce brain of the tiny.place seam. It:
//!
//! - claims a `@handle` only after the operator opts in (the `going_public`
//!   flag standing in for the Identity approval checkpoint) and funding covers
//!   the registry fee — catching the `402` challenge, budget-checking, then
//!   completing the paid registration;
//! - publishes the Agent Card, parking it in the [`Outbox`] for the attached
//!   replayer when tiny.place is unreachable — and erroring when no replayer is
//!   attached, because then nothing would ever send it;
//! - sends outbound A2A tasks, paying an x402 challenge under budget and
//!   journaling the spend, and **failing** rather than queuing when offline;
//! - quotes and pays firm requirements, **failing closed** the instant a
//!   payment would exceed either the caller's [`BudgetScope`] or the company's
//!   monthly ceiling, and journaling every in/out movement to the ledger.
//!
//! Every spend path is budget-fail-closed and ledger-journaled, so budget and
//! audit are self-contained and unit-testable offline against
//! [`MockTinyplaceClient`](super::client::MockTinyplaceClient).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use crate::Result;
use crate::economy::client::{JsonRpcRequest, PaidOutcome, TinyplaceClient, now_secs};
use crate::economy::outbox::{Outbox, OutboxAction};
use crate::economy::signer::LocalSigner;
use crate::economy::x402::{self, X402Challenge};
use crate::error::OpenCompanyError;
use crate::ports::AgentEconomy;
use crate::ports::store::CompanyStore;
use crate::ports::types::{
    A2aTask, A2aTaskHandle, AgentAddr, AgentCard, BudgetScope, CompanyId, CompanyIdentity,
    LedgerEntry, PaymentReceipt, PaymentRequirement, Quote, RegistrationState,
};
use crate::ports::{generate_id, now_millis};

/// The settlement asset used when a firm quote is paid.
const PAY_ASSET: &str = "USDC";
/// The settlement network used when a firm quote is paid.
const PAY_NETWORK: &str = "solana";

/// How often an attached replayer retries the queued Agent Card.
///
/// There is no connectivity signal in the [`TinyplaceClient`] seam — no
/// reconnect event, no health stream — so the first tick after the network
/// returns *is* the reconnect drain. Inventing a listener to sharpen that is out
/// of scope for #454; a card that is at most one interval stale is the whole
/// point of a degrade path.
pub const OUTBOX_REPLAY_INTERVAL: Duration = Duration::from_secs(30);

/// The [`AgentEconomy`] over a [`TinyplaceClient`].
pub struct TinyplaceEconomy {
    client: Arc<dyn TinyplaceClient>,
    signer: Arc<LocalSigner>,
    store: Arc<dyn CompanyStore>,
    company: CompanyId,
    monthly_cap: Option<f64>,
    going_public: bool,
    outbox: Arc<Outbox>,
    /// Whether a background replayer is attached to this economy — i.e. whether
    /// anything will ever send what [`Self::publish_card`] queues.
    ///
    /// Set by exactly one function, [`spawn_outbox_replayer`], and never
    /// unset. That is what lets the offline publish path answer *"is my degrade
    /// honest?"* instead of assuming it. A constructor that forgets to attach a
    /// replayer leaves this `false`, and the publish then errors in the caller's
    /// face rather than dropping the card in silence.
    replayer: AtomicBool,
}

impl TinyplaceEconomy {
    /// Builds an economy for `company`. `going_public` starts `false`: the
    /// adapter never spends the master key on registration until the operator
    /// opts in via [`Self::going_public`].
    pub fn new(
        client: Arc<dyn TinyplaceClient>,
        signer: Arc<LocalSigner>,
        store: Arc<dyn CompanyStore>,
        company: CompanyId,
        monthly_cap: Option<f64>,
    ) -> Self {
        Self {
            client,
            signer,
            store,
            company,
            monthly_cap,
            going_public: false,
            outbox: Arc::new(Outbox::new()),
            replayer: AtomicBool::new(false),
        }
    }

    /// Sets the going-public flag. `true` encodes the Identity approval
    /// checkpoint plus funding: only then will [`Self::ensure_registered`]
    /// claim (and pay for) the `@handle`.
    pub fn going_public(mut self, approved: bool) -> Self {
        self.going_public = approved;
        self
    }

    /// The outbox holding the card deferred while tiny.place was unreachable.
    pub fn outbox(&self) -> &Arc<Outbox> {
        &self.outbox
    }

    /// Whether a background replayer is attached — see [`spawn_outbox_replayer`].
    pub fn has_replayer(&self) -> bool {
        self.replayer.load(Ordering::SeqCst)
    }

    /// Replays the queued Agent Card, if there is one.
    ///
    /// Empty outbox is a silent success — the replayer calls this on a timer, so
    /// "nothing to do" is the normal case. On continued unreachability the card
    /// goes back into the slot (unless a newer one landed meanwhile: see
    /// [`Outbox::requeue`]) and the error is returned, so the next tick tries
    /// again.
    ///
    /// A **rejection** — a `4xx` the server actually answered — is not requeued.
    /// Retrying it every interval forever would be a hot loop against a card the
    /// directory has already refused; the error is surfaced, and the next real
    /// `publish_card` queues a fresh card if one is warranted.
    pub async fn flush_outbox(&self) -> Result<()> {
        let Some(OutboxAction::PublishCard(card)) = self.outbox.take() else {
            return Ok(());
        };
        match self.client.put_agent(&self.signer.agent_id(), &card).await {
            Ok(()) => Ok(()),
            Err(err) => {
                if matches!(&err, OpenCompanyError::Tinyplace { code, .. } if code == "unreachable")
                {
                    self.outbox.requeue(OutboxAction::PublishCard(card));
                }
                Err(err)
            }
        }
    }

    /// Journals a negative (outflow) ledger movement.
    async fn ledger_out(&self, kind: &str, amount: f64, memo: String) -> Result<()> {
        self.store
            .append_ledger(
                &self.company,
                LedgerEntry {
                    at_millis: now_millis(),
                    kind: kind.to_string(),
                    amount_usd: -amount,
                    memo,
                },
            )
            .await
    }

    /// The remaining monthly budget: the cap minus the sum of ledger outflows.
    /// Fails open to `+∞` when no cap is set or no record exists yet.
    async fn remaining_budget(&self) -> Result<f64> {
        let Some(cap) = self.monthly_cap else {
            return Ok(f64::INFINITY);
        };
        let spent: f64 = match self.store.load(&self.company).await? {
            Some(record) => record
                .ledger
                .iter()
                .filter(|entry| entry.amount_usd < 0.0)
                .map(|entry| -entry.amount_usd)
                .sum(),
            None => 0.0,
        };
        Ok(cap - spent)
    }

    /// Parses a decimal challenge amount, rejecting a malformed string.
    fn parse_amount(raw: &str) -> Result<f64> {
        raw.trim().parse::<f64>().map_err(|_| {
            OpenCompanyError::tinyplace(
                "bad_amount",
                format!("challenge amount `{raw}` is not a number"),
            )
        })
    }

    /// Enforces both the monthly ceiling for `amount`, returning
    /// [`OpenCompanyError::BudgetExceeded`] when it would be crossed.
    async fn enforce_monthly(&self, amount: f64, what: &str) -> Result<()> {
        let remaining = self.remaining_budget().await?;
        if amount > remaining {
            return Err(OpenCompanyError::BudgetExceeded(format!(
                "{what} needs ${amount:.2} but only ${remaining:.2} remains this month"
            )));
        }
        Ok(())
    }
}

impl std::fmt::Debug for TinyplaceEconomy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TinyplaceEconomy")
            .field("company", &self.company)
            .field("agent_id", &self.signer.agent_id())
            .field("monthly_cap", &self.monthly_cap)
            .field("going_public", &self.going_public)
            .field("outbox_len", &self.outbox.len())
            .field("replayer", &self.has_replayer())
            .finish_non_exhaustive()
    }
}

/// Attaches the background Agent-Card replayer to a freshly built economy, and
/// with it the promise that a queued card will actually be sent (issue #454).
///
/// **This is the only thing that sets the "a replayer is attached" flag**, which
/// is what makes the flag mean what it says. [`TinyplaceEconomy::publish_card`]
/// reads it to decide whether an offline publish may honestly report success, so
/// a construction path that does not come through here degrades to a visible
/// error instead of a silent drop. Call it on the concrete economy **before** it
/// is type-erased into `Arc<dyn AgentEconomy>` — after that the flush surface is
/// unreachable, which is exactly how the queue ended up with no drain.
///
/// # Why a weak reference
///
/// The task holds a [`Weak`](std::sync::Weak), not an `Arc`, and exits the first
/// time the upgrade fails. A runtime rebuild (issue #290) constructs a fresh
/// economy and drops the old one; with a strong reference the old economy — and
/// its timer — would live forever, so every rebuild would leak another flusher
/// publishing an ever-staler card over the live one. Weak makes the replayer's
/// lifetime exactly the economy's.
///
/// `every` is the retry period; production passes [`OUTBOX_REPLAY_INTERVAL`].
/// It is a parameter rather than a constant read inside so a test can exercise
/// *this* function — the real attachment path, flag and all — without waiting
/// out a production interval.
pub fn spawn_outbox_replayer(economy: &Arc<TinyplaceEconomy>, every: Duration) {
    economy.replayer.store(true, Ordering::SeqCst);
    let company = economy.company.clone();
    let weak = Arc::downgrade(economy);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        loop {
            ticker.tick().await;
            let Some(economy) = weak.upgrade() else {
                // The economy is gone (a rebuild, or shutdown): so is its queue.
                break;
            };
            if economy.outbox.is_empty() {
                continue;
            }
            match economy.flush_outbox().await {
                Ok(()) => tracing::info!(
                    company = %company,
                    "tiny.place: replayed the queued Agent Card; the directory entry is current"
                ),
                Err(err) => tracing::warn!(
                    company = %company,
                    error = %err,
                    "tiny.place: replaying the queued Agent Card failed"
                ),
            }
        }
    });
}

#[async_trait]
impl AgentEconomy for TinyplaceEconomy {
    async fn ensure_registered(&self, identity: &CompanyIdentity) -> Result<RegistrationState> {
        // If the handle already resolves to us, we are done.
        if let Ok(addr) = self.client.resolve(&identity.handle).await
            && addr.0 == self.signer.agent_id()
        {
            return Ok(RegistrationState::Registered { addr });
        }

        // A private company never spends its master key at boot.
        if !self.going_public {
            return Ok(RegistrationState::Unregistered);
        }

        match self.client.register_name(&identity.handle).await? {
            PaidOutcome::Done(receipt) => Ok(RegistrationState::Registered { addr: receipt.addr }),
            PaidOutcome::PaymentRequired(challenge) => {
                let fee = Self::parse_amount(&challenge.amount)?;
                self.enforce_monthly(fee, "registering a handle").await?;
                let auth = x402::authorize(&self.signer, &challenge, now_secs());
                let receipt = self
                    .client
                    .register_name_paid(&identity.handle, &auth)
                    .await?;
                self.ledger_out(
                    "registry.fee",
                    fee,
                    format!(
                        "claimed @{} (signer {})",
                        identity.handle,
                        self.signer.agent_id()
                    ),
                )
                .await?;
                Ok(RegistrationState::Registered { addr: receipt.addr })
            }
        }
    }

    /// Publishes the Agent Card, degrading to the outbox when tiny.place is
    /// unreachable — **but only when something will actually drain it**
    /// (issue #454).
    ///
    /// This is the inversion the whole issue turns on. The old arm queued
    /// unconditionally and returned `Ok(())`, and nothing in the tree ever
    /// drained that queue: `drain()`'s only caller lived in its own test module,
    /// and the concrete economy is type-erased behind [`AgentEconomy`] the moment
    /// it is built, so no production code could reach it even in principle. Every
    /// card published during an outage was therefore dropped while the caller was
    /// told it had succeeded.
    ///
    /// So the `Ok(())` is now *earned*: it is returned only when
    /// [`spawn_outbox_replayer`] has attached a replayer, which makes the
    /// sentence "queued, and it will go out" true. With no replayer the original
    /// unreachable error propagates and nothing is queued — a constructor path
    /// written later that forgets to attach one inherits a visible error rather
    /// than the silent drop this issue is about. Fail-safe by construction, the
    /// same direction as `PublishDestination::Unclaimed` in the harness publish
    /// queue (issue #445).
    async fn publish_card(&self, _identity: &CompanyIdentity, card: &AgentCard) -> Result<()> {
        match self.client.put_agent(&self.signer.agent_id(), card).await {
            Ok(()) => Ok(()),
            Err(OpenCompanyError::Tinyplace { code, message }) if code == "unreachable" => {
                if !self.has_replayer() {
                    return Err(OpenCompanyError::tinyplace("unreachable", message));
                }
                // Offline, and a replayer is listening: park the newest card and
                // let it go stale rather than erroring.
                self.outbox.enqueue(OutboxAction::PublishCard(card.clone()));
                tracing::warn!(
                    company = %self.company,
                    handle = %card.handle,
                    "tiny.place is unreachable; the Agent Card is queued for replay and the \
                     directory entry is stale until it lands"
                );
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// Sends an outbound A2A task, and **fails** when tiny.place is unreachable
    /// rather than deferring it.
    ///
    /// This is the deliberate other half of [`publish_card`](Self::publish_card)'s
    /// contract, and the split is about money (issue #454). A card publish
    /// degrades and replays: it is idempotent, it costs nothing, and the newest
    /// card is always the right one to send whenever the network returns. A task
    /// send does neither. It may carry an x402 payment, the budget scope that
    /// authorised it belongs to the caller's cycle and is gone by flush time, and
    /// a replay that lands after the caller already retried is a double-send —
    /// which here means a double-spend. So the error goes back to the caller, who
    /// is the only party holding the context to decide whether to retry it.
    ///
    /// Before #454 this arm did *both*: it pushed a copy onto the outbox **and**
    /// returned the error. Nothing ever drained that copy, so it was pure
    /// unreachable state; had anything drained it, it would have been a
    /// background double-send with no budget behind it.
    async fn send_a2a_task(&self, to: &AgentAddr, task: A2aTask) -> Result<A2aTaskHandle> {
        let params = serde_json::json!({
            "id": generate_id(),
            "skill": task.skill,
            "input": task.input,
        });
        let rpc = JsonRpcRequest::new("tasks/send", params);

        match self.client.send_task(&to.0, rpc.clone()).await {
            Ok(PaidOutcome::Done(response)) => Ok(handle_from_response(&response, &rpc.id)),
            Ok(PaidOutcome::PaymentRequired(challenge)) => {
                let amount = Self::parse_amount(&challenge.amount)?;
                self.enforce_monthly(amount, "hiring").await?;
                let auth = x402::authorize(&self.signer, &challenge, now_secs());
                let response = self
                    .client
                    .send_task_paid(&to.0, rpc.clone(), &auth)
                    .await?;
                self.ledger_out(
                    "x402.out",
                    amount,
                    format!(
                        "a2a tasks/send to {} for `{}` (signer {})",
                        to.0,
                        task.skill,
                        self.signer.agent_id()
                    ),
                )
                .await?;
                Ok(handle_from_response(&response, &rpc.id))
            }
            Err(OpenCompanyError::Tinyplace { code, message }) if code == "unreachable" => {
                // Offline: surface the error and queue nothing. The caller owns
                // the retry decision for a task that may cost money.
                Err(OpenCompanyError::tinyplace("unreachable", message))
            }
            Err(err) => Err(err),
        }
    }

    async fn quote(&self, requirement: &PaymentRequirement) -> Result<Quote> {
        // A firm quote equal to the requirement; no wire round-trip needed.
        Ok(Quote {
            quote_id: generate_id(),
            to: requirement.to.clone(),
            amount_usd: requirement.amount_usd,
        })
    }

    async fn pay(&self, quote: &Quote, budget: &BudgetScope) -> Result<PaymentReceipt> {
        // Fail closed against the caller's scope first — before any wire call.
        if quote.amount_usd > budget.remaining_usd {
            return Err(OpenCompanyError::BudgetExceeded(format!(
                "paying ${:.2} exceeds the {} scope's ${:.2}",
                quote.amount_usd, budget.label, budget.remaining_usd
            )));
        }
        // Then clamp against the monthly ceiling.
        self.enforce_monthly(quote.amount_usd, "paying").await?;

        let challenge = X402Challenge {
            amount: format!("{:.2}", quote.amount_usd),
            recipient: quote.to.0.clone(),
            asset: PAY_ASSET.to_string(),
            network: PAY_NETWORK.to_string(),
        };
        let auth = x402::authorize(&self.signer, &challenge, now_secs());

        let verified = self.client.payments_verify(&auth).await?;
        if !verified.ok {
            return Err(OpenCompanyError::tinyplace(
                "verify_failed",
                verified
                    .reason
                    .unwrap_or_else(|| "payment authorization did not verify".to_string()),
            ));
        }
        self.client.payments_settle(&auth).await?;

        self.ledger_out(
            "x402.out",
            quote.amount_usd,
            format!("paid quote {} to {}", quote.quote_id, quote.to.0),
        )
        .await?;

        Ok(PaymentReceipt {
            quote_id: quote.quote_id.clone(),
            amount_usd: quote.amount_usd,
            at_millis: now_millis(),
        })
    }
}

/// Extracts an [`A2aTaskHandle`] from a response, falling back to the request id.
fn handle_from_response(
    response: &crate::economy::client::JsonRpcResponse,
    fallback_id: &str,
) -> A2aTaskHandle {
    let id = response
        .result
        .as_ref()
        .and_then(|r| r.get("id").or_else(|| r.get("taskId")))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| fallback_id.to_string());
    A2aTaskHandle(id)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::economy::client::{JsonRpcResponse, MockTinyplaceClient, RegistryReceipt};
    use crate::ports::types::CompanyRecord;
    use crate::store::FsCompanyStore;

    fn signer() -> Arc<LocalSigner> {
        Arc::new(LocalSigner::generate())
    }

    /// A store rooted at a fresh tempdir, seeded with an empty-ledger record so
    /// `remaining_budget` can read it back.
    async fn seeded_store(company: &CompanyId) -> (tempfile::TempDir, Arc<dyn CompanyStore>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FsCompanyStore::new(dir.path().to_path_buf());
        let manifest =
            toml::from_str("[company]\nname = \"Acme\"\nhandle = \"acme\"\n").expect("manifest");
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                id: company.clone(),
                manifest,
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_agent_edits: Vec::new(),
                overlay_policy: None,
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .expect("save");
        (dir, Arc::new(store))
    }

    fn challenge(amount: &str) -> X402Challenge {
        X402Challenge {
            amount: amount.to_string(),
            recipient: "Recipient".into(),
            asset: "USDC".into(),
            network: "solana".into(),
        }
    }

    fn identity(company: &CompanyId) -> CompanyIdentity {
        CompanyIdentity {
            company: company.clone(),
            handle: "acme".to_string(),
        }
    }

    async fn ledger_of(store: &Arc<dyn CompanyStore>, company: &CompanyId) -> Vec<LedgerEntry> {
        store.load(company).await.unwrap().unwrap().ledger
    }

    #[tokio::test]
    async fn registration_402_then_budget_check_then_complete() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let sk = signer();
        let mock = Arc::new(
            MockTinyplaceClient::new()
                .with_register_name(PaidOutcome::PaymentRequired(challenge("25.00")))
                .with_register_paid(RegistryReceipt {
                    id: "reg-1".into(),
                    addr: AgentAddr("acme.addr".into()),
                    fee_usd: 25.0,
                }),
        );
        let economy = TinyplaceEconomy::new(
            mock.clone(),
            sk,
            store.clone(),
            company.clone(),
            Some(200.0),
        )
        .going_public(true);

        let state = economy
            .ensure_registered(&identity(&company))
            .await
            .unwrap();
        assert_eq!(
            state,
            RegistrationState::Registered {
                addr: AgentAddr("acme.addr".into())
            }
        );

        let ledger = ledger_of(&store, &company).await;
        assert_eq!(ledger.len(), 1, "one registry.fee row");
        assert_eq!(ledger[0].kind, "registry.fee");
        assert_eq!(ledger[0].amount_usd, -25.0);
        assert_eq!(mock.count("register_name_paid"), 1);
    }

    #[tokio::test]
    async fn registration_over_budget_rejected() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(
            MockTinyplaceClient::new()
                .with_register_name(PaidOutcome::PaymentRequired(challenge("25.00"))),
        );
        let economy = TinyplaceEconomy::new(
            mock.clone(),
            signer(),
            store.clone(),
            company.clone(),
            Some(10.0),
        )
        .going_public(true);

        let err = economy
            .ensure_registered(&identity(&company))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "budget_exceeded");
        assert!(
            ledger_of(&store, &company).await.is_empty(),
            "ledger untouched"
        );
        assert_eq!(
            mock.count("register_name_paid"),
            0,
            "never completed the paid call"
        );
    }

    #[tokio::test]
    async fn ensure_registered_private_returns_unregistered() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        // resolve returns not_found; going_public is left false.
        let mock = Arc::new(MockTinyplaceClient::new());
        let economy =
            TinyplaceEconomy::new(mock.clone(), signer(), store, company.clone(), Some(200.0));

        let state = economy
            .ensure_registered(&identity(&company))
            .await
            .unwrap();
        assert_eq!(state, RegistrationState::Unregistered);
        assert_eq!(
            mock.count("register_name"),
            0,
            "private company never claims"
        );
    }

    #[tokio::test]
    async fn pay_fails_closed_when_over_scope() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(MockTinyplaceClient::new());
        let economy = TinyplaceEconomy::new(mock.clone(), signer(), store, company, None);

        let quote = Quote {
            quote_id: "q1".into(),
            to: AgentAddr("Vendor".into()),
            amount_usd: 30.0,
        };
        let budget = BudgetScope {
            remaining_usd: 20.0,
            label: "vendor-scope".into(),
        };
        let err = economy.pay(&quote, &budget).await.unwrap_err();
        assert_eq!(err.code(), "budget_exceeded");
        assert_eq!(mock.settle_calls(), 0, "no settle before the budget check");
        assert_eq!(mock.verify_calls(), 0, "no verify before the budget check");
    }

    #[tokio::test]
    async fn pay_success_journals_receipt() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(MockTinyplaceClient::new().with_verify(true, None));
        let economy = TinyplaceEconomy::new(
            mock.clone(),
            signer(),
            store.clone(),
            company.clone(),
            Some(100.0),
        );

        let quote = Quote {
            quote_id: "q1".into(),
            to: AgentAddr("Vendor".into()),
            amount_usd: 15.0,
        };
        let budget = BudgetScope {
            remaining_usd: 50.0,
            label: "vendor-scope".into(),
        };
        let receipt = economy.pay(&quote, &budget).await.unwrap();
        assert_eq!(receipt.quote_id, "q1");
        assert_eq!(receipt.amount_usd, 15.0);
        assert_eq!(mock.settle_calls(), 1);

        let ledger = ledger_of(&store, &company).await;
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].kind, "x402.out");
        assert_eq!(ledger[0].amount_usd, -15.0);
    }

    #[tokio::test]
    async fn pay_rejects_when_verification_fails() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(MockTinyplaceClient::new().with_verify(false, Some("bad sig".into())));
        let economy =
            TinyplaceEconomy::new(mock.clone(), signer(), store.clone(), company.clone(), None);

        let quote = Quote {
            quote_id: "q1".into(),
            to: AgentAddr("Vendor".into()),
            amount_usd: 15.0,
        };
        let budget = BudgetScope {
            remaining_usd: 50.0,
            label: "s".into(),
        };
        let err = economy.pay(&quote, &budget).await.unwrap_err();
        assert_eq!(err.code(), "tinyplace_verify_failed");
        assert_eq!(mock.settle_calls(), 0, "never settle an unverified auth");
        assert!(ledger_of(&store, &company).await.is_empty());
    }

    #[tokio::test]
    async fn send_task_402_pays_under_budget() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(
            MockTinyplaceClient::new()
                .with_send_task(PaidOutcome::PaymentRequired(challenge("12.00")))
                .with_send_task_paid(JsonRpcResponse::ok(
                    "t1",
                    serde_json::json!({ "id": "task-9" }),
                )),
        );
        let economy = TinyplaceEconomy::new(
            mock.clone(),
            signer(),
            store.clone(),
            company.clone(),
            Some(100.0),
        );

        let handle = economy
            .send_a2a_task(
                &AgentAddr("Vendor".into()),
                A2aTask {
                    skill: "seo.audit".into(),
                    input: serde_json::json!({ "site": "x" }),
                },
            )
            .await
            .unwrap();
        assert_eq!(handle, A2aTaskHandle("task-9".into()));

        let ledger = ledger_of(&store, &company).await;
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].kind, "x402.out");
        assert_eq!(ledger[0].amount_usd, -12.0);
    }

    #[tokio::test]
    async fn send_task_402_over_budget_rejected() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(
            MockTinyplaceClient::new()
                .with_send_task(PaidOutcome::PaymentRequired(challenge("80.00"))),
        );
        let economy = TinyplaceEconomy::new(
            mock.clone(),
            signer(),
            store.clone(),
            company.clone(),
            Some(50.0),
        );

        let err = economy
            .send_a2a_task(
                &AgentAddr("Vendor".into()),
                A2aTask {
                    skill: "seo.audit".into(),
                    input: serde_json::json!({}),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "budget_exceeded");
        assert_eq!(mock.count("send_task_paid"), 0);
        assert!(ledger_of(&store, &company).await.is_empty());
    }

    fn card(handle: &str) -> AgentCard {
        AgentCard {
            handle: handle.to_string(),
            ..Default::default()
        }
    }

    /// Polls until the outbox drains, bounded so a broken replayer fails the
    /// test instead of hanging it.
    async fn drained_within(outbox: &Arc<Outbox>, budget: Duration) -> bool {
        let step = Duration::from_millis(10);
        for _ in 0..(budget.as_millis() / step.as_millis()).max(1) {
            if outbox.is_empty() {
                return true;
            }
            tokio::time::sleep(step).await;
        }
        outbox.is_empty()
    }

    /// Issue #454, the reachability test that matters: an economy built through
    /// the **production spawn path** may degrade offline, and the card it queued
    /// is genuinely sent once the network returns.
    ///
    /// A test that calls `flush_outbox` by hand would prove the drain works. It
    /// would not prove the drain is ever *reached* — which is the entire defect,
    /// since the pre-#454 drain worked fine and had no caller outside its own
    /// test module. So nothing here touches the flush surface: the replayer's
    /// own timer is what has to do it.
    #[tokio::test]
    async fn a_spawned_replayer_queues_offline_then_actually_sends() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(MockTinyplaceClient::new());
        mock.set_reachable(false);
        let economy = Arc::new(TinyplaceEconomy::new(
            mock.clone(),
            signer(),
            store,
            company.clone(),
            None,
        ));
        spawn_outbox_replayer(&economy, Duration::from_millis(20));

        economy
            .publish_card(&identity(&company), &card("acme"))
            .await
            .expect("an offline publish degrades once a replayer is attached");
        assert_eq!(economy.outbox().len(), 1, "the card is queued");
        assert_eq!(mock.count("put_agent"), 1, "one refused attempt so far");

        // The network comes back. There is no reconnect signal in the client
        // seam, so the next interval tick is the drain.
        mock.set_reachable(true);
        assert!(
            drained_within(economy.outbox(), Duration::from_secs(5)).await,
            "the replayer never drained the outbox"
        );
        assert!(
            mock.count("put_agent") >= 2,
            "the queued card was replayed onto the wire, not merely dropped"
        );
    }

    /// The other side of the same invariant: with no replayer attached, an
    /// offline publish is an **error** and queues nothing. This is what a
    /// constructor path that forgets to spawn the replayer inherits — a visible
    /// failure instead of a card dropped behind an `Ok(())`.
    #[tokio::test]
    async fn offline_publish_without_a_replayer_errors_and_queues_nothing() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(MockTinyplaceClient::new());
        mock.set_reachable(false);
        // The bare constructor: no `spawn_outbox_replayer`.
        let economy = TinyplaceEconomy::new(mock.clone(), signer(), store, company.clone(), None);
        assert!(!economy.has_replayer());

        let err = economy
            .publish_card(&identity(&company), &card("acme"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "tinyplace_unreachable");
        assert!(
            economy.outbox().is_empty(),
            "nothing is queued when nothing would drain it"
        );
    }

    /// Newest wins: replay only ever needs the current card, so a second offline
    /// publish replaces the first rather than stacking behind it.
    #[tokio::test]
    async fn a_newer_offline_card_replaces_the_queued_one() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(MockTinyplaceClient::new());
        mock.set_reachable(false);
        let economy = Arc::new(TinyplaceEconomy::new(
            mock.clone(),
            signer(),
            store,
            company.clone(),
            None,
        ));
        // The production interval, so the replayer's timer cannot fire inside
        // this test: what is asserted is the queue, not the drain.
        spawn_outbox_replayer(&economy, OUTBOX_REPLAY_INTERVAL);

        let identity = identity(&company);
        economy.publish_card(&identity, &card("old")).await.unwrap();
        economy.publish_card(&identity, &card("new")).await.unwrap();

        assert_eq!(economy.outbox().len(), 1, "the outbox stays bounded");
        assert_eq!(
            economy.outbox().take(),
            Some(OutboxAction::PublishCard(card("new"))),
            "the newest card is what replay would send"
        );
    }

    /// Issue #454: an offline task send errors and queues **nothing**. The ghost
    /// copy it used to push was unreachable state at best and a budget-less
    /// background double-send at worst.
    #[tokio::test]
    async fn unreachable_send_task_errors_without_queueing() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(MockTinyplaceClient::new());
        mock.set_reachable(false);
        let economy = TinyplaceEconomy::new(mock.clone(), signer(), store, company, None);

        let err = economy
            .send_a2a_task(
                &AgentAddr("Vendor".into()),
                A2aTask {
                    skill: "seo.audit".into(),
                    input: serde_json::json!({}),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "tinyplace_unreachable");
        assert!(
            economy.outbox().is_empty(),
            "a paid task is never deferred for background replay"
        );
    }

    #[tokio::test]
    async fn ensure_registered_already_ours_short_circuits() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let sk = signer();
        let mine = AgentAddr(sk.agent_id());
        let mock = Arc::new(MockTinyplaceClient::new().with_resolve(Some(mine.clone())));
        let economy = TinyplaceEconomy::new(mock.clone(), sk, store, company.clone(), Some(200.0))
            .going_public(true);

        let state = economy
            .ensure_registered(&identity(&company))
            .await
            .unwrap();
        assert_eq!(state, RegistrationState::Registered { addr: mine });
        assert_eq!(mock.count("register_name"), 0, "no claim when already ours");
    }
}

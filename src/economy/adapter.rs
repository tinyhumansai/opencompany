//! [`TinyplaceEconomy`]: the [`AgentEconomy`] adapter over a [`TinyplaceClient`].
//!
//! This is the commerce brain of the tiny.place seam. It:
//!
//! - claims a `@handle` only after the operator opts in (the `going_public`
//!   flag standing in for the Identity approval checkpoint) and funding covers
//!   the registry fee — catching the `402` challenge, budget-checking, then
//!   completing the paid registration;
//! - publishes the Agent Card, queuing it to the [`Outbox`] (never erroring)
//!   when tiny.place is unreachable;
//! - sends outbound A2A tasks, paying an x402 challenge under budget and
//!   journaling the spend, or queuing the task when offline;
//! - quotes and pays firm requirements, **failing closed** the instant a
//!   payment would exceed either the caller's [`BudgetScope`] or the company's
//!   monthly ceiling, and journaling every in/out movement to the ledger.
//!
//! Every spend path is budget-fail-closed and ledger-journaled, so budget and
//! audit are self-contained and unit-testable offline against
//! [`MockTinyplaceClient`](super::client::MockTinyplaceClient).

use std::sync::Arc;

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

/// The [`AgentEconomy`] over a [`TinyplaceClient`].
pub struct TinyplaceEconomy {
    client: Arc<dyn TinyplaceClient>,
    signer: Arc<LocalSigner>,
    store: Arc<dyn CompanyStore>,
    company: CompanyId,
    monthly_cap: Option<f64>,
    going_public: bool,
    outbox: Arc<Outbox>,
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
        }
    }

    /// Sets the going-public flag. `true` encodes the Identity approval
    /// checkpoint plus funding: only then will [`Self::ensure_registered`]
    /// claim (and pay for) the `@handle`.
    pub fn going_public(mut self, approved: bool) -> Self {
        self.going_public = approved;
        self
    }

    /// The outbox holding actions deferred while tiny.place was unreachable.
    pub fn outbox(&self) -> &Arc<Outbox> {
        &self.outbox
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
            .finish_non_exhaustive()
    }
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

    async fn publish_card(&self, _identity: &CompanyIdentity, card: &AgentCard) -> Result<()> {
        match self.client.put_agent(&self.signer.agent_id(), card).await {
            Ok(()) => Ok(()),
            Err(OpenCompanyError::Tinyplace { code, .. }) if code == "unreachable" => {
                // Offline: queue the card so it goes stale rather than erroring.
                self.outbox.enqueue(OutboxAction::PublishCard(card.clone()));
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

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
                // Offline: queue the task and surface the error so the caller
                // decides whether to retry.
                self.outbox.enqueue(OutboxAction::SendTask {
                    to: to.clone(),
                    task,
                });
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
                id: company.clone(),
                manifest,
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
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

    #[tokio::test]
    async fn unreachable_publish_card_enqueues_outbox_not_error() {
        let company = CompanyId::new("acme");
        let (_dir, store) = seeded_store(&company).await;
        let mock = Arc::new(MockTinyplaceClient::new());
        mock.set_reachable(false);
        let economy = TinyplaceEconomy::new(mock.clone(), signer(), store, company.clone(), None);

        let card = AgentCard {
            handle: "acme".into(),
            ..Default::default()
        };
        economy
            .publish_card(&identity(&company), &card)
            .await
            .expect("publish never errors offline");
        assert_eq!(economy.outbox().len(), 1, "card queued to the outbox");
    }

    #[tokio::test]
    async fn unreachable_send_task_enqueues_and_errors() {
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
        assert_eq!(economy.outbox().len(), 1, "task queued despite the error");
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

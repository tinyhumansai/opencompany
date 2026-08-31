//! Handing a company's turn to a runner on someone else's machine.
//!
//! ## The spike, and why `Proxy`/`Conductor` is not used
//!
//! The ACP Rust SDK ships a proxy layer, and the obvious question was whether
//! it could carry this. It cannot, and the reason is structural rather than a
//! missing feature.
//!
//! `agent-client-protocol-conductor` takes `components: Vec<String>` — "a list
//! of commands to chain together; **the final command must be the agent**" —
//! and spawns that chain when `initialize` arrives. The topology is a fixed
//! linear chain to exactly **one** upstream, fixed per connection. It is built
//! for *adding capabilities* to one agent (inject an MCP server, prepend a
//! preamble), and it does that well.
//!
//! What dispatch needs is the other shape entirely: many downstream clients,
//! many upstream runners, and the upstream chosen **per session** at
//! `session/new` from whichever runner currently holds the scope. A fixed chain
//! cannot express "this session goes to Ada's laptop and that one to Bob's",
//! and there is no point in the conductor's lifecycle where that choice could
//! be made.
//!
//! So the routing is ours. That costs less than it sounds, because the pieces
//! already exist: [`AcpRunTurn`](crate::harness::acp_run_turn) folds an ACP
//! turn into a [`TurnOutcome`], and it takes an
//! [`AcpAgent`](crate::harness::acp_run_turn::AcpAgent) port. A runner is just
//! another implementation of that port.
//!
//! ## Session ids are rewritten, not forwarded
//!
//! A runner mints its own session ids, and so does this host. Forwarding either
//! side's id to the other would mean two namespaces sharing a keyspace — and
//! the first collision between two runners' `sess-1` would silently cross two
//! companies' turns. [`SessionMap`] keeps them apart.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::harness::acp_run_turn::{AcpAgent, AcpTurn};
use crate::ports::types::CompanyId;
use crate::runner::registry::RunnerRegistry;

/// One runner's wire, as dispatch needs it.
///
/// A port for the same reason [`AcpAgent`] is: the socket belongs to the server
/// lane, and dispatch should be testable without one.
#[async_trait]
pub trait RunnerLink: Send + Sync {
    /// Opens a session on the runner, returning **its** session id.
    async fn open_session(&self, runner_id: &str, scope: &str) -> Result<String>;

    /// Runs one turn on an already-open runner session.
    async fn prompt(&self, runner_id: &str, runner_session: &str, message: &str)
    -> Result<AcpTurn>;

    /// Forwards a cancel. Advisory, like every ACP cancel.
    async fn cancel(&self, runner_id: &str, runner_session: &str) -> Result<()>;
}

/// Maps this host's session keys onto runner-side session ids.
#[derive(Debug, Default)]
pub struct SessionMap {
    /// `(runner_id, host session key)` → the runner's own session id.
    inner: Mutex<HashMap<(String, String), String>>,
}

impl SessionMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, runner_id: &str, host_key: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("session map poisoned")
            .get(&(runner_id.to_string(), host_key.to_string()))
            .cloned()
    }

    pub fn insert(&self, runner_id: &str, host_key: &str, runner_session: String) {
        self.inner.lock().expect("session map poisoned").insert(
            (runner_id.to_string(), host_key.to_string()),
            runner_session,
        );
    }

    /// Drops every session a runner held.
    ///
    /// Called when a runner detaches. Without it, a runner that reconnects
    /// would be handed session ids from its previous life — ids its new process
    /// has never heard of, so every turn would fail in a way that looks like a
    /// protocol bug rather than a stale mapping.
    pub fn forget_runner(&self, runner_id: &str) {
        self.inner
            .lock()
            .expect("session map poisoned")
            .retain(|(id, _), _| id != runner_id);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("session map poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An [`AcpAgent`] backed by whichever runner currently holds the scope.
pub struct RunnerDispatch<L: RunnerLink> {
    registry: std::sync::Arc<RunnerRegistry>,
    link: L,
    sessions: SessionMap,
    /// Injected so tests are not at the mercy of the clock.
    now: fn() -> u64,
}

impl<L: RunnerLink> RunnerDispatch<L> {
    pub fn new(registry: std::sync::Arc<RunnerRegistry>, link: L) -> Self {
        Self {
            registry,
            link,
            sessions: SessionMap::new(),
            now: crate::ports::now_millis,
        }
    }

    #[cfg(test)]
    fn with_clock(mut self, now: fn() -> u64) -> Self {
        self.now = now;
        self
    }

    /// Picks a runner for `scope`, or explains why there is none.
    ///
    /// The message distinguishes "nothing is attached" from "something is
    /// attached but cannot work", because those have completely different
    /// answers — start the desktop, versus sign the harness in.
    fn choose(&self, scope: &str) -> Result<String> {
        let now = (self.now)();
        if let Some(runner) = self.registry.available_for(scope, now).into_iter().next() {
            return Ok(runner.runner_id);
        }
        let attached = self
            .registry
            .list()
            .into_iter()
            .filter(|r| r.scopes.iter().any(|s| s == scope))
            .collect::<Vec<_>>();
        Err(OpenCompanyError::InvalidRequest(if attached.is_empty() {
            format!("no runner is attached for {scope}")
        } else if attached.iter().any(|r| r.is_live(now)) {
            format!("the runner for {scope} has no signed-in harness")
        } else {
            format!("the runner for {scope} has stopped reporting")
        }))
    }
}

#[async_trait]
impl<L: RunnerLink> AcpAgent for RunnerDispatch<L> {
    /// `observer` is ignored, and can only be ignored here: [`RunnerLink`]
    /// hands back a whole [`AcpTurn`] when the remote turn is over, so there
    /// is no per-update stream on this side to tee. Making it observable is a
    /// change to the runner *wire* — the socket would have to forward each
    /// `session/update` as it arrives instead of the turn's transcript at the
    /// end — not something this fold can synthesise. Until then an ACP turn on
    /// a runner shows its steps when it finishes, exactly as it did before,
    /// while a local one shows them live.
    async fn prompt(
        &self,
        _company: &CompanyId,
        session_key: &str,
        message: &str,
        _observer: Option<&crate::ports::acp::AcpObserver>,
    ) -> Result<AcpTurn> {
        // Chosen per turn rather than pinned at session start: a runner can go
        // away between turns, and re-choosing is how the next turn lands on
        // whatever replaced it instead of failing against a dead one.
        let runner_id = self.choose(session_key)?;

        let runner_session = match self.sessions.get(&runner_id, session_key) {
            Some(existing) => existing,
            None => {
                let opened = self.link.open_session(&runner_id, session_key).await?;
                self.sessions
                    .insert(&runner_id, session_key, opened.clone());
                opened
            }
        };

        self.link.prompt(&runner_id, &runner_session, message).await
    }

    async fn cancel(&self, _company: &CompanyId, session_key: &str) -> Result<()> {
        let runner_id = self.choose(session_key)?;
        let Some(runner_session) = self.sessions.get(&runner_id, session_key) else {
            // Nothing open to cancel. Not an error: a cancel racing the first
            // turn of a session is ordinary, and failing it would surface as a
            // scary message for a no-op.
            return Ok(());
        };
        self.link.cancel(&runner_id, &runner_session).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::harness::acp_run_turn::AcpUpdate;
    use crate::runner::registry::{HarnessOffer, RunnerCapabilities, RunnerStatus};
    use std::sync::Arc;

    #[derive(Default)]
    struct Recorder {
        opened: Mutex<Vec<(String, String)>>,
        prompted: Mutex<Vec<(String, String, String)>>,
        cancelled: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl RunnerLink for Arc<Recorder> {
        async fn open_session(&self, runner_id: &str, scope: &str) -> Result<String> {
            self.opened
                .lock()
                .unwrap()
                .push((runner_id.to_string(), scope.to_string()));
            // Deliberately the same id from every runner, so a test that
            // conflated namespaces would collide.
            Ok("sess-1".to_string())
        }
        async fn prompt(&self, runner: &str, session: &str, message: &str) -> Result<AcpTurn> {
            self.prompted.lock().unwrap().push((
                runner.to_string(),
                session.to_string(),
                message.to_string(),
            ));
            Ok(AcpTurn {
                updates: vec![AcpUpdate::MessageChunk(format!("ran on {runner}"))],
                stop_reason: "end_turn".to_string(),
            })
        }
        async fn cancel(&self, runner: &str, session: &str) -> Result<()> {
            self.cancelled
                .lock()
                .unwrap()
                .push((runner.to_string(), session.to_string()));
            Ok(())
        }
    }

    fn runner(id: &str, scope: &str, ready: bool, seen: u64) -> RunnerStatus {
        RunnerStatus {
            runner_id: id.to_string(),
            owner: "owner".to_string(),
            scopes: vec![scope.to_string()],
            capabilities: RunnerCapabilities {
                harnesses: vec![HarnessOffer {
                    id: "claude".to_string(),
                    ready,
                }],
                max_parallel: 1,
            },
            last_seen_millis: seen,
            connection: format!("conn-{id}"),
        }
    }

    fn dispatch(registry: Arc<RunnerRegistry>) -> (RunnerDispatch<Arc<Recorder>>, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let dispatch = RunnerDispatch::new(registry, Arc::clone(&recorder)).with_clock(|| 0);
        (dispatch, recorder)
    }

    #[tokio::test]
    async fn a_turn_lands_on_the_runner_holding_the_scope() {
        let registry = Arc::new(RunnerRegistry::new());
        registry.admit(runner("ada", "acme::ceo", true, 0));
        let (dispatch, recorder) = dispatch(registry);

        let turn = dispatch
            .prompt(&CompanyId::new("acme"), "acme::ceo", "do it", None)
            .await
            .unwrap();

        assert_eq!(turn.updates.len(), 1);
        assert_eq!(recorder.prompted.lock().unwrap()[0].0, "ada");
    }

    #[tokio::test]
    async fn a_session_is_opened_once_and_reused() {
        // Opening per turn would give the harness no memory of the previous
        // question — every turn a fresh conversation.
        let registry = Arc::new(RunnerRegistry::new());
        registry.admit(runner("ada", "acme::ceo", true, 0));
        let (dispatch, recorder) = dispatch(registry);

        for _ in 0..3 {
            dispatch
                .prompt(&CompanyId::new("acme"), "acme::ceo", "again", None)
                .await
                .unwrap();
        }
        assert_eq!(recorder.opened.lock().unwrap().len(), 1);
        assert_eq!(recorder.prompted.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn two_runners_minting_the_same_session_id_do_not_collide() {
        // THE reason ids are rewritten rather than forwarded. Both runners here
        // answer `sess-1`; a single namespace would cross two companies' turns.
        let registry = Arc::new(RunnerRegistry::new());
        registry.admit(runner("ada", "acme::ceo", true, 0));
        registry.admit(runner("bob", "globex::cto", true, 0));
        let (dispatch, recorder) = dispatch(registry);

        dispatch
            .prompt(&CompanyId::new("acme"), "acme::ceo", "x", None)
            .await
            .unwrap();
        dispatch
            .prompt(&CompanyId::new("globex"), "globex::cto", "y", None)
            .await
            .unwrap();

        let prompted = recorder.prompted.lock().unwrap();
        assert_eq!(prompted[0].0, "ada");
        assert_eq!(prompted[1].0, "bob");
        // Two entries, one per (runner, scope) — not one shared by both.
        assert_eq!(dispatch.sessions.len(), 2);
    }

    #[tokio::test]
    async fn no_attached_runner_says_exactly_that() {
        let registry = Arc::new(RunnerRegistry::new());
        let (dispatch, _) = dispatch(registry);

        let error = dispatch
            .prompt(&CompanyId::new("acme"), "acme::ceo", "x", None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no runner is attached"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_signed_out_runner_is_reported_differently_from_a_missing_one() {
        // Different answers: start the desktop, versus sign the harness in. One
        // message for both would send someone looking in the wrong place.
        let registry = Arc::new(RunnerRegistry::new());
        registry.admit(runner("ada", "acme::ceo", false, 0));
        let (dispatch, _) = dispatch(registry);

        let error = dispatch
            .prompt(&CompanyId::new("acme"), "acme::ceo", "x", None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no signed-in harness"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_runner_that_stopped_reporting_is_reported_as_such() {
        let registry = Arc::new(RunnerRegistry::new());
        registry.admit(runner("ada", "acme::ceo", true, 0));
        let recorder = Arc::new(Recorder::default());
        // A clock well past the presence TTL.
        let dispatch = RunnerDispatch::new(registry, recorder)
            .with_clock(|| crate::runner::registry::PRESENCE_TTL_MILLIS * 10);

        let error = dispatch
            .prompt(&CompanyId::new("acme"), "acme::ceo", "x", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stopped reporting"), "{error}");
    }

    #[tokio::test]
    async fn a_replacement_runner_takes_the_next_turn() {
        // A runner can go away between turns. Re-choosing per turn is what
        // makes the next one land on whatever replaced it.
        let registry = Arc::new(RunnerRegistry::new());
        registry.admit(runner("ada", "acme::ceo", true, 0));
        let (dispatch, recorder) = dispatch(Arc::clone(&registry));

        dispatch
            .prompt(&CompanyId::new("acme"), "acme::ceo", "first", None)
            .await
            .unwrap();
        // Ada's laptop closes; Bob's takes the scope.
        registry.admit(runner("bob", "acme::ceo", true, 0));
        dispatch
            .prompt(&CompanyId::new("acme"), "acme::ceo", "second", None)
            .await
            .unwrap();

        let prompted = recorder.prompted.lock().unwrap();
        assert_eq!(prompted[0].0, "ada");
        assert_eq!(prompted[1].0, "bob");
        // Bob got his own session rather than inheriting Ada's id.
        assert_eq!(recorder.opened.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cancelling_before_a_session_exists_is_a_no_op() {
        // A cancel racing the first turn is ordinary; failing it would surface
        // an alarming message for nothing.
        let registry = Arc::new(RunnerRegistry::new());
        registry.admit(runner("ada", "acme::ceo", true, 0));
        let (dispatch, recorder) = dispatch(registry);

        dispatch
            .cancel(&CompanyId::new("acme"), "acme::ceo")
            .await
            .expect("a cancel with nothing open must not fail");
        assert!(recorder.cancelled.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cancel_reaches_the_runner_holding_the_session() {
        let registry = Arc::new(RunnerRegistry::new());
        registry.admit(runner("ada", "acme::ceo", true, 0));
        let (dispatch, recorder) = dispatch(registry);

        dispatch
            .prompt(&CompanyId::new("acme"), "acme::ceo", "x", None)
            .await
            .unwrap();
        dispatch
            .cancel(&CompanyId::new("acme"), "acme::ceo")
            .await
            .unwrap();

        assert_eq!(
            recorder.cancelled.lock().unwrap()[0],
            ("ada".to_string(), "sess-1".to_string())
        );
    }

    #[test]
    fn detaching_a_runner_forgets_its_sessions() {
        // A reconnecting runner must not be handed ids from its previous life:
        // its new process has never heard of them, and every turn would fail
        // looking like a protocol bug rather than a stale mapping.
        let map = SessionMap::new();
        map.insert("ada", "acme::ceo", "sess-1".to_string());
        map.insert("bob", "globex::cto", "sess-1".to_string());

        map.forget_runner("ada");
        assert!(map.get("ada", "acme::ceo").is_none());
        assert_eq!(map.get("bob", "globex::cto").as_deref(), Some("sess-1"));
    }
}

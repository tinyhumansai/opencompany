//! Tests for cross-desk referral: what crosses, what does not, and what a
//! host owes the library.
//!
//! Everything here is scripted through [`HiveTurnRunner`] and
//! [`HiveReferralRunner`] — no model, no store, no provider — because the
//! properties under test are this host's, not a model's. Whether a room asks a
//! good question is a model's business; whether the answer lands on the right
//! desk, under an author that cannot be counted as a supporter, and only when
//! the desk opted in, is entirely ours.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::moves_test::Runner;
use super::referral;
use super::test::{MemoryLog, desk_of, record};
use super::*;
use crate::Result;
use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, EventSeq};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A two-desk company: `eng` deliberates, `platform` is the desk it may ask.
///
/// `platform` has two members on purpose, so a `@#platform` mention has a real
/// choice to make and the test asserts the library's pick rather than the only
/// candidate there was.
fn two_desks(hive: &str) -> String {
    format!(
        "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"planner\"\nrole = \"Planner\"\n\
         [[agent]]\nid = \"scout\"\nrole = \"Scout\"\n\
         [[agent]]\nid = \"critic\"\nrole = \"Critic\"\n\
         [[agent]]\nid = \"sre\"\nrole = \"SRE\"\n\
         [[agent]]\nid = \"dba\"\nrole = \"DBA\"\n\
         [[group_chat]]\nid = \"eng\"\nname = \"Engineering\"\n\
         description = \"Ship the rollout\"\n\
         members = [\"planner\", \"scout\", \"critic\"]\n\
         {hive}\n\
         [[group_chat]]\nid = \"platform\"\nname = \"Platform\"\n\
         description = \"Owns the database and the edge\"\n\
         members = [\"sre\", \"dba\"]\n"
    )
}

/// The operator's message on `eng`, and the watermark the episode opens on.
async fn open(log: &MemoryLog) -> EventSeq {
    log.append(
        &MemoryLog::company(),
        CompanyEvent::OperatorMessage {
            text: "Decide the rollout.".into(),
            by: None,
            chat: Some("eng".into()),
            parent: None,
            deliverable: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
        },
    )
    .await
    .expect("the journal accepts the operator's message")
}

/// A far desk that answers every question with one fixed line, and records
/// every question it was asked.
struct FarDesk {
    answer: String,
    asked: Mutex<Vec<(String, String, String)>>,
    broken: bool,
}

impl FarDesk {
    fn answering(answer: &str) -> Self {
        Self {
            answer: answer.to_owned(),
            asked: Mutex::new(Vec::new()),
            broken: false,
        }
    }

    fn broken() -> Self {
        Self {
            answer: String::new(),
            asked: Mutex::new(Vec::new()),
            broken: true,
        }
    }

    /// Every `(desk, agent, prompt)` this runner was handed, in order.
    fn asked(&self) -> Vec<(String, String, String)> {
        self.asked.lock().expect("poisoned").clone()
    }
}

#[async_trait]
impl HiveReferralRunner for FarDesk {
    async fn refer(&self, desk_id: &str, agent_id: &str, prompt: &str) -> Result<String> {
        self.asked.lock().expect("poisoned").push((
            desk_id.to_owned(),
            agent_id.to_owned(),
            prompt.to_owned(),
        ));
        if self.broken {
            return Err(crate::error::OpenCompanyError::Config(
                "turn for 'sre' hit the harness's per-turn wall-clock ceiling after 10m 00s"
                    .to_owned(),
            ));
        }
        Ok(self.answer.clone())
    }
}

/// A far desk that answers as a **room**: `deliberate` returns a conclusion, so
/// the single-seat `refer` seam is never reached.
///
/// Modelled on how the real host behaves rather than on what is convenient to
/// assert: `deliberate` yielding `Some` is a desk that convened, `None` is a
/// desk that cannot hold a room at all (no `[hive]` block, one member, quorum
/// out of reach), and `Err` is a room that was stood up and then broke.
struct Room {
    conclusion: Option<String>,
    broken: bool,
    /// Every `(desk, asker, prompt)` handed to `deliberate`, in order.
    convened: Mutex<Vec<(String, String, String)>>,
    /// And every `(desk, agent, prompt)` that fell through to one seat.
    fell_back: Mutex<Vec<(String, String, String)>>,
}

impl Room {
    fn answering(conclusion: &str) -> Self {
        Self {
            conclusion: Some(conclusion.to_owned()),
            broken: false,
            convened: Mutex::new(Vec::new()),
            fell_back: Mutex::new(Vec::new()),
        }
    }

    /// A desk that cannot deliberate, so the crossing must ask its seat.
    fn cannot() -> Self {
        Self {
            conclusion: None,
            broken: false,
            convened: Mutex::new(Vec::new()),
            fell_back: Mutex::new(Vec::new()),
        }
    }

    fn broken() -> Self {
        Self {
            conclusion: None,
            broken: true,
            convened: Mutex::new(Vec::new()),
            fell_back: Mutex::new(Vec::new()),
        }
    }

    fn convened(&self) -> Vec<(String, String, String)> {
        self.convened.lock().expect("poisoned").clone()
    }

    fn fell_back(&self) -> Vec<(String, String, String)> {
        self.fell_back.lock().expect("poisoned").clone()
    }
}

#[async_trait]
impl HiveReferralRunner for Room {
    async fn refer(&self, desk_id: &str, agent_id: &str, prompt: &str) -> Result<String> {
        self.fell_back.lock().expect("poisoned").push((
            desk_id.to_owned(),
            agent_id.to_owned(),
            prompt.to_owned(),
        ));
        Ok("one seat's opinion".to_owned())
    }

    async fn deliberate(&self, desk_id: &str, asker: &str, prompt: &str) -> Result<Option<String>> {
        self.convened.lock().expect("poisoned").push((
            desk_id.to_owned(),
            asker.to_owned(),
            prompt.to_owned(),
        ));
        if self.broken {
            return Err(crate::error::OpenCompanyError::Config(
                "the referred desk's episode could not be stood up".to_owned(),
            ));
        }
        Ok(self.conclusion.clone())
    }
}

/// The `hive` block a desk that refers is declared with.
const REFERRING: &str = "hive = { turn_budget = 6, quorum = 2, blind_round = false, \
                         referral = { enabled = true } }";

/// The same desk with referral left unsaid, which is every desk by default.
const PLAIN: &str = "hive = { turn_budget = 6, quorum = 2, blind_round = false }";

/// Open a driver over `eng`, with or without the federation wired.
async fn run(
    hive: &str,
    lines: &[(&str, &str)],
    far: Option<&FarDesk>,
) -> (Arc<MemoryLog>, EpisodeOutcome) {
    let log = Arc::new(MemoryLog::default());
    let trigger = open(&log).await;
    let manifest = two_desks(hive);
    let desk = desk_of(&manifest, "eng").expect("a room");
    let runner = Runner::new(lines);
    let federation = desk_federation(&record(&manifest), &desk);
    let mut driver = EpisodeDriver::new(
        MemoryLog::company(),
        desk,
        Arc::clone(&log) as Arc<dyn EventLog>,
        &runner,
        "Decide the rollout.",
    );
    if let (Some(federation), Some(far)) = (federation, far) {
        driver = driver.with_federation(federation, far);
    }
    let outcome = driver.run(trigger).await.expect("the episode runs");
    (log, outcome)
}

/// The same, over any referral runner — `run` is fixed to [`FarDesk`].
async fn run_with(
    hive: &str,
    lines: &[(&str, &str)],
    far: &dyn HiveReferralRunner,
) -> (Arc<MemoryLog>, EpisodeOutcome) {
    let log = Arc::new(MemoryLog::default());
    let trigger = open(&log).await;
    let manifest = two_desks(hive);
    let desk = desk_of(&manifest, "eng").expect("a room");
    let runner = Runner::new(lines);
    let federation = desk_federation(&record(&manifest), &desk).expect("a federation");
    let outcome = EpisodeDriver::new(
        MemoryLog::company(),
        desk,
        Arc::clone(&log) as Arc<dyn EventLog>,
        &runner,
        "Decide the rollout.",
    )
    .with_federation(federation, far)
    .run(trigger)
    .await
    .expect("the episode runs");
    (log, outcome)
}

// ---------------------------------------------------------------------------
// The snapshot
// ---------------------------------------------------------------------------

#[test]
fn a_desk_that_did_not_opt_in_has_no_federation_at_all() {
    let manifest = two_desks(PLAIN);
    let desk = desk_of(&manifest, "eng").expect("a room");
    assert!(
        desk_federation(&record(&manifest), &desk).is_none(),
        "referral is off unless the manifest says otherwise, so the default company \
         deliberates exactly as it did before referral existed"
    );
}

#[test]
fn a_company_with_one_desk_has_nowhere_to_refer_to() {
    // The opt-in is present and the fold would honour it; there is simply no
    // peer. `None` here is what keeps the driver from rendering a peer block
    // listing nobody.
    let manifest = format!(
        "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"planner\"\nrole = \"Planner\"\n\
         [[agent]]\nid = \"scout\"\nrole = \"Scout\"\n\
         [[group_chat]]\nid = \"eng\"\nname = \"Engineering\"\n\
         members = [\"planner\", \"scout\"]\n{REFERRING}\n"
    );
    let desk = desk_of(&manifest, "eng").expect("a room");
    assert!(desk_federation(&record(&manifest), &desk).is_none());
}

#[test]
fn the_federation_carries_every_desk_and_names_the_peers() {
    let manifest = two_desks(REFERRING);
    let desk = desk_of(&manifest, "eng").expect("a room");
    let federation = desk_federation(&record(&manifest), &desk).expect("a federation");

    assert_eq!(
        federation.desks.len(),
        2,
        "both desks are carried — `@#eng` has to resolve to *this* desk for the fold to \
         refuse it as a self-desk mention: {federation:?}"
    );
    let peers = federation.peers_of("eng");
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].id, "platform");
    assert_eq!(peers[0].members, vec!["sre".to_owned(), "dba".to_owned()]);
    assert_eq!(
        federation.agents.len(),
        5,
        "every seated teammate on either desk, so `@sre` resolves from `eng`"
    );
}

/// **A desk created at runtime through the console is a referral peer too.**
///
/// `desk_federation` used to build its desk list from `record.manifest
/// .group_chats` alone, so an `overlay_desks` desk — the console's "create a
/// desk" action — could never be named by `@#id` from another desk, even
/// though [`CompanyRecord::effective_desk_members`] (the single source of
/// truth the REST desk list and the harness both read) already treats
/// manifest and overlay desks as one set.
#[test]
fn the_federation_includes_a_console_created_desk() {
    let manifest = two_desks(REFERRING);
    let desk = desk_of(&manifest, "eng").expect("a room");
    let mut record = record(&manifest);
    record.overlay_desks.push(crate::ports::types::OverlayDesk {
        id: "growth".to_owned(),
        name: "Growth".to_owned(),
        description: Some("Runs experiments".to_owned()),
        members: vec!["planner".to_owned()],
        responder: crate::ports::types::ResponderMode::default(),
        hive: Default::default(),
    });

    let federation = desk_federation(&record, &desk).expect("a federation");
    assert_eq!(
        federation.desks.len(),
        3,
        "the manifest's two desks plus the console-created one: {federation:?}"
    );
    let peers = federation.peers_of("eng");
    assert!(
        peers.iter().any(|peer| peer.id == "growth"),
        "a console-created desk must be a reachable referral peer: {peers:?}"
    );
}

/// **A teammate's overlay-edited label reaches the federation, not the
/// manifest's stale one.**
///
/// `member_of` used to read `record.manifest.agents` directly to build the
/// `(id, label)` pairs in `HiveFederation.agents`, bypassing
/// `CompanyRecord::effective_agent` — the function that already applies a
/// console edit on top of the manifest row everywhere else the roster is
/// read. A teammate renamed after the manifest was authored was therefore
/// served under its stale manifest label to a peer desk asking for it.
#[test]
fn the_federation_serves_an_overlay_edited_label_not_the_stale_manifest_one() {
    let manifest = two_desks(REFERRING);
    let desk = desk_of(&manifest, "eng").expect("a room");
    let mut record = record(&manifest);
    record
        .overlay_agent_edits
        .push(crate::ports::types::AgentOverride {
            provider: None,
            agent_id: "sre".to_owned(),
            name: Some("Senior SRE".to_owned()),
            role: None,
            description: None,
            tools: None,
            instructions: None,
            avatar: None,
            model: None,
            harness: None,
        });

    let federation = desk_federation(&record, &desk).expect("a federation");
    let label = federation
        .agents
        .iter()
        .find(|(id, _)| id == "sre")
        .map(|(_, label)| label.clone())
        .expect("sre is seated on the peer desk");
    assert_eq!(
        label, "Senior SRE",
        "the overlay-edited label must reach the federation, not the manifest's own: \
         {federation:?}"
    );
}

// ---------------------------------------------------------------------------
// The policy
// ---------------------------------------------------------------------------

#[test]
fn an_unopted_desk_derives_a_policy_that_refers_nothing() {
    let config = ReferralConfig::default();
    let policy = config.policy();
    assert!(!policy.enabled);
    assert!(!policy.reach.crosses());
}

#[test]
fn opting_in_and_saying_nothing_else_buys_a_round_trip_across_desks() {
    let config = ReferralConfig {
        enabled: Some(true),
        ..ReferralConfig::default()
    };
    let policy = config.policy();
    assert!(policy.enabled);
    // Two hops, because a round trip is one out and one home. A default of one
    // would ask a question and throw the answer away.
    assert_eq!(policy.max_hops, 2);
    assert!(policy.returns);
    assert!(
        policy.reach.addresses_desks(),
        "a desk that opted in and got `local` would have opted in to nothing it \
         could not already do"
    );
    assert_eq!(config.peer_cap(), 2);
}

#[test]
fn a_named_reach_is_honoured_in_both_narrower_directions() {
    for (word, crosses, desks) in [
        ("local", false, false),
        ("channels", true, false),
        ("desks", true, true),
    ] {
        let config = ReferralConfig {
            enabled: Some(true),
            reach: Some(word.to_owned()),
            ..ReferralConfig::default()
        };
        let reach = config.policy().reach;
        assert_eq!(reach.crosses(), crosses, "{word}");
        assert_eq!(reach.addresses_desks(), desks, "{word}");
    }
}

// ---------------------------------------------------------------------------
// What actually crosses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_desk_mention_runs_one_turn_on_the_far_desk_and_carries_the_answer_home() {
    let far =
        FarDesk::answering("The replica lag budget is 400ms, measured over the last quarter.");
    let (log, outcome) = run(
        REFERRING,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        Some(&far),
    )
    .await;

    // Exactly one question, run on the far desk, by the far desk's own first
    // eligible member. Not a fan-out: `platform` has two members and one
    // answered.
    let asked = far.asked();
    assert_eq!(asked.len(), 1, "one question, one turn: {asked:?}");
    assert_eq!(asked[0].0, "platform", "the turn ran on the far desk");
    assert_eq!(asked[0].1, "sre", "the far desk's first eligible member");
    assert!(
        !asked[0].2.contains("Decide the rollout."),
        "the far teammate is asked a colleague's question, not handed this room's task:\n{}",
        asked[0].2
    );
    assert!(
        asked[0].2.contains("What is the replica lag budget?"),
        "and it is handed the line that asked it:\n{}",
        asked[0].2
    );

    // The far turn is journaled on the far desk, under the teammate that took
    // it, because that is what happened there.
    let far_rows = log.replies("platform");
    assert_eq!(far_rows.len(), 1, "{far_rows:?}");
    assert_eq!(far_rows[0].0, "sre");
    assert!(far_rows[0].1.contains("400ms"));

    // The answer comes home attributed, and authored by the room.
    let home = log.replies("eng");
    let returned = home
        .iter()
        .find(|(_, text)| text.contains("400ms"))
        .expect("the answer came home");
    assert_eq!(
        returned.0, HIVE_REFERRAL_AUTHOR,
        "the row that carries it is the room's, not the far teammate's: {home:?}"
    );
    assert!(
        returned.1.contains("@sre") && returned.1.contains("Platform"),
        "and it names who said it and where: {}",
        returned.1
    );

    assert_eq!(outcome.referrals.asked.len(), 1);
    assert!(
        outcome.referrals.asked[0].returned,
        "{:?}",
        outcome.referrals
    );
    assert!(
        outcome
            .summary()
            .contains("asked 1 question of another desk"),
        "the operator is told the room went outside: {}",
        outcome.summary()
    );
}

/// **Naming a PERSON puts the exchange in their pair thread, not on a desk.**
///
/// A crossing addressed to somebody by name is a conversation between the two
/// of them. Run on the answerer's desk, it published a question that desk was
/// never asked, in front of colleagues who had no part in it, in a transcript
/// whose job is to record what THAT desk did.
///
/// The sibling test above pins the other half: `@#platform` is a question put
/// to the desk, and still runs there so the room can settle it. Both forms are
/// indistinguishable by the time the library hands back a `Referral` — each
/// carries the target's home desk — so the two tests together are what keep
/// the distinction from collapsing back into one.
#[tokio::test]
async fn naming_a_person_holds_the_exchange_in_their_pair_thread() {
    let far = FarDesk::answering("The replica lag budget is 400ms.");
    let (log, _outcome) = run(
        REFERRING,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @sre",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        Some(&far),
    )
    .await;

    let pair = super::referral::pair_conversation("planner", "sre");
    assert_eq!(pair, "dm:planner+sre", "sorted, so one thread not two");

    // Both sides are there, in order, so the thread reads as the conversation
    // it was.
    let said = log.replies(&pair);
    assert_eq!(said.len(), 2, "question then answer: {said:?}");
    assert_eq!(said[0].0, "planner", "the asker speaks first");
    assert!(said[0].1.contains("replica lag budget"), "{said:?}");
    assert_eq!(said[1].0, "sre", "and the person asked answers");
    assert!(said[1].1.contains("400ms"), "{said:?}");

    // And the answerer's desk carries none of it. This is the whole point: the
    // question was put to a person, so their colleagues are not made to read it.
    assert!(
        log.replies("platform").is_empty(),
        "a desk that was never asked holds nothing: {:?}",
        log.replies("platform")
    );

    // The room still gets the conclusion, under the room's own author — the
    // property this must not break.
    let home = log.replies("eng");
    let returned = home
        .iter()
        .find(|(_, text)| text.contains("400ms"))
        .expect("the answer still comes home");
    assert_eq!(returned.0, HIVE_REFERRAL_AUTHOR, "{home:?}");
}

#[tokio::test]
async fn the_answer_comes_home_under_the_room_so_it_can_never_be_counted_as_support() {
    // The property the whole design turns on. A row authored by a roster id
    // folds as a trace and can be counted as a supporter; if the far desk's
    // answer came back under `@sre`, one supporter would count on two desks —
    // which is voting twice, not pooling information.
    let far = FarDesk::answering("!support #stage The platform desk agrees.");
    let (log, _) = run(
        REFERRING,
        &[
            ("planner", "!question #lag Ask them. @#platform"),
            ("scout", "!propose #stage Stage it."),
            ("critic", "!support #stage ^1 Fine."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        Some(&far),
    )
    .await;

    let home = log.replies("eng");
    let carried: Vec<&(String, String)> = home
        .iter()
        .filter(|(_, text)| text.contains("The platform desk agrees."))
        .collect();
    assert_eq!(carried.len(), 1, "{home:?}");
    assert_eq!(
        carried[0].0, HIVE_REFERRAL_AUTHOR,
        "an answer that crossed is authored by the room, never by the far teammate"
    );
    assert!(
        !carried[0].1.trim_start().starts_with('!'),
        "and its leading marker is stripped, so a far desk's `!support` cannot fold as \
         a trace on this one: {}",
        carried[0].1
    );
}

#[tokio::test]
async fn a_line_that_asks_nobody_refers_nothing() {
    let far = FarDesk::answering("unused");
    let (_, outcome) = run(
        REFERRING,
        &[
            (
                "planner",
                "!propose #stage Stage the rollout behind a flag.",
            ),
            ("scout", "!support #stage ^1 Agreed."),
            ("critic", "!commit #stage ^1 Recorded."),
        ],
        Some(&far),
    )
    .await;

    assert!(far.asked().is_empty(), "{:?}", far.asked());
    assert!(outcome.referrals.asked.is_empty());
    assert_eq!(
        outcome.referral_summary(),
        "",
        "a room that asked nothing says nothing about referral"
    );
}

/// **A question put to a seat on this desk is not a question of another desk.**
///
/// `reach` widens strictly (`local` → `channels` → `desks`), so a desk that
/// opted in to referral may legitimately put a question to one of its own
/// seats. The close used to describe every recorded question as being "of
/// another desk" regardless, and a live six-day `companies/vending_machine_co`
/// run reported "The room asked 2 questions of another desk (@fleet_tech on
/// ops, @field_realist on ops)" — on the ops desk, naming two ops seats. An
/// operator reading that has been told the room reached outside when it did
/// not, which is exactly the kind of claim the close exists to make reliably.
#[test]
fn the_close_tells_a_local_question_from_a_crossing_one() {
    use crate::hivemind::referral::{AskedQuestion, ReferralLedger};

    let outcome = |asked: Vec<AskedQuestion>| EpisodeOutcome {
        ending: EpisodeEnding::Exhausted,
        turns: 4,
        first_seq: None,
        last_seq: None,
        report_seq: None,
        violations: Vec::new(),
        failed_turns: 0,
        referrals: ReferralLedger {
            asked,
            over_cap: 0,
            failed: 0,
        },
    };
    let question = |target: &str, desk: &str, crossed: bool| AskedQuestion {
        asker: "route_planner".to_owned(),
        target: target.to_owned(),
        desk: desk.to_owned(),
        returned: false,
        crossed,
    };

    let local = outcome(vec![question("field_realist", "ops", false)]).referral_summary();
    assert!(
        local.contains("put 1 question to a seat on this desk (@field_realist)"),
        "{local}"
    );
    assert!(
        !local.contains("another desk"),
        "a question that never left the desk must not be reported as crossing: {local}"
    );

    let crossing =
        outcome(vec![question("account_manager", "commercial", true)]).referral_summary();
    assert!(
        crossing.contains("asked 1 question of another desk (@account_manager on commercial)"),
        "{crossing}"
    );

    // Both in one episode: each is counted under its own heading, and the
    // crossing one still names the desk it reached.
    let both = outcome(vec![
        question("account_manager", "commercial", true),
        question("field_realist", "ops", false),
    ])
    .referral_summary();
    assert!(both.contains("asked 1 question of another desk"), "{both}");
    assert!(
        both.contains("put 1 question to a seat on this desk"),
        "{both}"
    );
}

/// **Every close is a sentence, whatever the episode's referral facts were.**
///
/// The summary used to hang every clause off one "The room …" prefix, so an
/// episode whose only referral fact was a failure printed "The room 1 went
/// unanswered." — which a live `companies/vending_machine_co` run duly did.
#[test]
fn the_close_reads_as_english_for_every_combination_of_referral_facts() {
    use crate::hivemind::referral::{AskedQuestion, ReferralLedger};

    let outcome = |asked: Vec<AskedQuestion>, failed: u32, over_cap: u32| EpisodeOutcome {
        ending: EpisodeEnding::Exhausted,
        turns: 4,
        first_seq: None,
        last_seq: None,
        report_seq: None,
        violations: Vec::new(),
        failed_turns: 0,
        referrals: ReferralLedger {
            asked,
            over_cap,
            failed,
        },
    };
    let asked = || {
        vec![AskedQuestion {
            asker: "route_planner".to_owned(),
            target: "account_manager".to_owned(),
            desk: "commercial".to_owned(),
            returned: false,
            crossed: true,
        }]
    };

    // A failure on its own is its own sentence, with its own subject.
    let only_failed = outcome(Vec::new(), 1, 0).referral_summary();
    assert!(
        only_failed.contains("1 question went unanswered."),
        "{only_failed}"
    );
    assert!(
        !only_failed.contains("The room 1"),
        "the failure clause was hung off a prefix it does not continue: {only_failed}"
    );

    // Plural agreement on the same clause.
    let two_failed = outcome(Vec::new(), 2, 0).referral_summary();
    assert!(
        two_failed.contains("2 questions went unanswered."),
        "{two_failed}"
    );

    // And it still composes with a question the room did ask.
    let both = outcome(asked(), 1, 0).referral_summary();
    assert!(
        both.contains("The room asked 1 question of another desk"),
        "{both}"
    );
    assert!(both.contains("1 question went unanswered."), "{both}");

    // A cap refusal alone, likewise.
    let capped = outcome(Vec::new(), 0, 2).referral_summary();
    assert!(capped.starts_with(" 2 more were declined"), "{capped}");
}

/// **A desk whose name already ends in "desk" does not get a second one.**
///
/// Every desk in this repo is named "… desk", and the referral note appended
/// the word unconditionally: a live run printed "@route_planner on the
/// Operations desk desk did not answer the question."
#[test]
fn a_referral_note_names_a_desk_once() {
    use crate::hivemind::referral::{returned_note, unanswered_note};

    let named = returned_note("account_manager", "Commercial desk", "no idea");
    assert!(named.contains("on the Commercial desk answered"), "{named}");
    assert!(!named.contains("desk desk"), "{named}");

    let missing = unanswered_note("route_planner", "Operations desk");
    assert!(
        missing.contains("on the Operations desk did not answer"),
        "{missing}"
    );
    assert!(!missing.contains("desk desk"), "{missing}");

    // A name that does not carry the word still gets it.
    let bare = unanswered_note("planner", "eng");
    assert!(bare.contains("on the eng desk did not answer"), "{bare}");
}

/// **A barred move demoted for its grammar violation must not still trigger a
/// referral.**
///
/// `moves::demote` (called from `line_from` after a second grammar violation)
/// strips only the leading `!` off a barred move and leaves the rest of the
/// text — including any `@#desk` mention it names — intact. Before this fix,
/// `consider` resolved mentions from that raw, demoted content with no check
/// that the line still carried an allowed marker, so a member using a move
/// its seat does not have could still spend a far desk's turn and bring an
/// answer home, even though the line itself was refused as illegitimate.
#[tokio::test]
async fn a_demoted_barred_move_never_reaches_the_far_desk() {
    let far = FarDesk::answering("unused");
    // `planner`'s seat may only `!propose`; `!object` is barred for it. Two
    // consecutive barred attempts (the seat is asked once, corrected once,
    // and demoted on the second miss) both name `@#platform` in the body, the
    // same shape a legitimate referral-worthy line would use.
    let hive = "hive = { turn_budget = 6, quorum = 2, blind_round = false, \
                referral = { enabled = true }, moves = { planner = [\"propose\"] } }";
    let (_, outcome) = run(
        hive,
        &[
            (
                "planner",
                "!object >1 ^1 @#platform can you check the migration plan?",
            ),
            (
                "planner",
                "!object >1 ^1 @#platform can you check the migration plan?",
            ),
            ("scout", "!support #stage ^1 Agreed."),
            ("critic", "!commit #stage ^1 Recorded."),
        ],
        Some(&far),
    )
    .await;

    assert!(
        far.asked().is_empty(),
        "a demoted, illegitimate move must never spend a far desk's turn: {:?}",
        far.asked()
    );
    assert!(outcome.referrals.asked.is_empty());
    assert_eq!(
        outcome.violations.len(),
        1,
        "the barred move is still recorded as a violation: {:?}",
        outcome.violations
    );
}

#[tokio::test]
async fn a_desk_that_did_not_opt_in_never_leaves_the_room() {
    let far = FarDesk::answering("unused");
    // The same line, on a desk with no `referral` block: the mention is text
    // the room reads and nothing more.
    let (log, outcome) = run(
        PLAIN,
        &[
            ("planner", "!question #lag Ask them. @#platform"),
            ("scout", "!propose #stage Stage it."),
            ("critic", "!support #stage ^1 Fine."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        Some(&far),
    )
    .await;

    assert!(far.asked().is_empty(), "{:?}", far.asked());
    assert!(
        log.replies("platform").is_empty(),
        "the far desk is untouched"
    );
    assert!(outcome.referrals.asked.is_empty());
}

#[tokio::test]
async fn a_far_turn_that_does_not_finish_leaves_the_room_running() {
    let far = FarDesk::broken();
    let (log, outcome) = run(
        REFERRING,
        &[
            ("planner", "!question #lag Ask them. @#platform"),
            ("scout", "!propose #stage Stage it."),
            ("critic", "!support #stage ^1 Fine."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        Some(&far),
    )
    .await;

    assert_eq!(far.asked().len(), 1, "the question was put");
    assert!(
        log.replies("platform").is_empty(),
        "nothing is journaled on a desk whose turn failed"
    );
    // The asking room is told, on its own desk, that it got nothing back — and
    // it converges anyway. A question that went unanswered is a worse episode,
    // not a broken one.
    let home = log.replies("eng");
    assert!(
        home.iter()
            .any(|(author, text)| author == HIVE_REFERRAL_AUTHOR
                && text.contains("did not answer the question")),
        "{home:?}"
    );
    assert_eq!(outcome.referrals.failed, 1);
    assert!(outcome.referrals.asked.is_empty());
    assert!(matches!(outcome.ending, EpisodeEnding::Converged { .. }));
    assert!(
        outcome.summary().contains("1 question went unanswered"),
        "{}",
        outcome.summary()
    );
}

#[tokio::test]
async fn the_peer_cap_bounds_how_many_questions_one_episode_asks() {
    // `max_hops` bounds how *deep* a chain goes; the library deliberately
    // bounds nothing about how wide it is, because only a host knows what a
    // question costs. Here it costs a full model turn on another desk.
    let far = FarDesk::answering("Answered.");
    let hive = "hive = { turn_budget = 8, quorum = 2, blind_round = false, \
                referral = { enabled = true, peer_cap = 1 } }";
    let (_, outcome) = run(
        hive,
        &[
            ("planner", "!question #a First question. @#platform"),
            ("scout", "!question #b Second question. @#platform"),
            ("critic", "!propose #stage Stage it."),
            ("planner", "!support #stage ^3 Fine."),
            ("scout", "!commit #stage ^4 Recorded."),
        ],
        Some(&far),
    )
    .await;

    assert_eq!(
        far.asked().len(),
        1,
        "the second question never reached the far desk: {:?}",
        far.asked()
    );
    assert_eq!(outcome.referrals.over_cap, 1);
    assert!(
        outcome
            .summary()
            .contains("declined by this desk's `referral.peer_cap`"),
        "{}",
        outcome.summary()
    );
}

#[tokio::test]
async fn the_same_line_is_never_asked_twice() {
    // The idempotency the port exists for, scoped as this host can honestly
    // scope it: per episode, because an episode is not resumable. The driver
    // considers each committed line once, and the queue refuses a repeat of the
    // same `(conversation, trigger sequence)` even if it were handed one.
    let far = FarDesk::answering("Answered.");
    let (_, outcome) = run(
        REFERRING,
        &[
            ("planner", "!question #a Ask them. @#platform"),
            ("scout", "!propose #stage Stage it."),
            ("critic", "!support #stage ^1 Fine."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        Some(&far),
    )
    .await;
    assert_eq!(far.asked().len(), 1, "{:?}", far.asked());
    assert_eq!(outcome.referrals.asked.len(), 1);
}

// ---------------------------------------------------------------------------
// The prompt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_referring_desk_is_shown_its_peers_and_told_to_ask_early() {
    let far = FarDesk::answering("Answered.");
    let log = Arc::new(MemoryLog::default());
    let trigger = open(&log).await;
    let manifest = two_desks(REFERRING);
    let desk = desk_of(&manifest, "eng").expect("a room");
    let runner = Runner::new(&[
        ("planner", "!propose #stage Stage it."),
        ("scout", "!support #stage ^1 Fine."),
        ("critic", "!commit #stage ^1 Recorded."),
    ]);
    let federation = desk_federation(&record(&manifest), &desk).expect("a federation");
    EpisodeDriver::new(
        MemoryLog::company(),
        desk,
        Arc::clone(&log) as Arc<dyn EventLog>,
        &runner,
        "Decide the rollout.",
    )
    .with_federation(federation, &far)
    .run(trigger)
    .await
    .expect("the episode runs");

    let prompt = runner
        .prompts_for("planner")
        .into_iter()
        .next()
        .expect("planner spoke");
    assert!(
        prompt.contains("@#platform (Platform) — Owns the database and the edge"),
        "the seat is shown the desk it may ask and what that desk is for:\n{prompt}"
    );
    assert!(
        prompt.contains("EARLY"),
        "and told when to ask, which is the largest measured effect in the mechanism:\n{prompt}"
    );
    assert!(
        prompt.contains("it is not a vote"),
        "and told what an answer is worth:\n{prompt}"
    );
}

#[tokio::test]
async fn a_desk_without_referral_is_shown_no_peer_block_at_all() {
    let log = Arc::new(MemoryLog::default());
    let trigger = open(&log).await;
    let manifest = two_desks(PLAIN);
    let desk = desk_of(&manifest, "eng").expect("a room");
    let runner = Runner::new(&[
        ("planner", "!propose #stage Stage it."),
        ("scout", "!support #stage ^1 Fine."),
        ("critic", "!commit #stage ^1 Recorded."),
    ]);
    EpisodeDriver::new(
        MemoryLog::company(),
        desk,
        Arc::clone(&log) as Arc<dyn EventLog>,
        &runner,
        "Decide the rollout.",
    )
    .run(trigger)
    .await
    .expect("the episode runs");

    let prompt = runner
        .prompts_for("planner")
        .into_iter()
        .next()
        .expect("planner spoke");
    assert!(
        !prompt.contains("Other desks you may put"),
        "a seat that cannot ask must not be told it can — a member that spends its one \
         line on an impossible move has spent a turn of the room's budget on nothing:\n{prompt}"
    );
}

// ---------------------------------------------------------------------------
// The rendered rows
// ---------------------------------------------------------------------------

/// **A question put to a DESK is answered by that desk, not by its first seat.**
///
/// `forward_to_desk` in the library resolves `@#platform` to that desk's one
/// responder — the first active member other than the author — and hands it
/// over as the whole of the crossing. On `members = ["sre", "dba"]` that is
/// `sre` every time and `dba` is not reachable by a desk crossing at all. The
/// asking room did not ask `sre`; it asked the desk, and the objection the
/// host's own `forward` comment raises — that a desk crossing must not "skip
/// the deliberation the desk exists for" — is only answered by convening it.
#[tokio::test]
async fn a_desk_crossing_convenes_the_far_desk_rather_than_asking_one_seat() {
    let far = Room::answering("The replica lag budget is 400ms, measured over the last quarter.");
    let (log, _) = run_with(
        REFERRING,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        &far,
    )
    .await;

    let convened = far.convened();
    assert_eq!(convened.len(), 1, "one question, one room: {convened:?}");
    assert_eq!(convened[0].0, "platform", "the far desk was convened");
    assert_eq!(
        convened[0].1, "planner",
        "attributed to the seat that asked, which is who the question is from"
    );
    assert!(
        convened[0].2.contains("What is the replica lag budget?"),
        "and the room is handed the line that asked it:\n{}",
        convened[0].2
    );
    assert!(
        far.fell_back().is_empty(),
        "no seat was asked on the side — a room that answered has answered: {:?}",
        far.fell_back()
    );

    // **Credited to the desk, never to the seat the library happened to pick.**
    //
    // A room's conclusion is not any one member's line, and several of its
    // seats may have argued the other way. Naming `@sre` would put words in a
    // teammate's mouth on a desk where nobody reading this can check.
    let home = log.replies("eng");
    let returned = home
        .iter()
        .find(|(_, text)| text.contains("400ms"))
        .expect("the answer came home");
    assert_eq!(
        returned.0, HIVE_REFERRAL_AUTHOR,
        "carried by the room as every crossing answer is: {home:?}"
    );
    assert!(
        returned.1.contains("Platform"),
        "and it names the desk that answered: {}",
        returned.1
    );
    assert!(
        !returned.1.contains("@sre") && !returned.1.contains("@dba"),
        "but not a seat, which did not personally answer: {}",
        returned.1
    );
}

/// **A continuation that repeats the line it already committed is not a row.**
///
/// The asker's turn continues on the answer its crossing earned, which is the
/// point of the second pass. A seat handed the same prompt shape twice can
/// simply re-emit the line it can see at the bottom of the transcript, and
/// journaling that put the identical row on the desk back to back — the second
/// copy carrying the crossing, so the console showed one member saying the same
/// thing twice. Seen on `companies/retail_co`.
///
/// The prompt now says what a continuation is for; this pins the guarantee,
/// which does not depend on a model reading it.
#[tokio::test]
async fn a_continuation_that_repeats_the_committed_line_is_not_journaled() {
    let far = FarDesk::answering("The replica lag budget is 400ms.");
    let (log, _) = run(
        REFERRING,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            // The continuation, saying nothing the first pass did not.
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        Some(&far),
    )
    .await;

    let asked: Vec<_> = log
        .replies("eng")
        .into_iter()
        .filter(|(author, text)| author == "planner" && text.contains("replica lag budget?"))
        .collect();
    assert_eq!(
        asked.len(),
        1,
        "the question is one row, not two: {asked:?}"
    );
}

/// **No continuation on an answer this seat cannot read.**
///
/// `returns = false` is a legitimate policy: ask a peer desk, let the answer
/// stand on their transcript, and do not carry it back. `forward` still records
/// the question on the ledger, so counting successful far-desk turns reported an
/// answer the asking desk had never received — and the continuation ran with a
/// prompt announcing it, free to commit a decision on information its speaker
/// never saw (Codex, #2332).
#[tokio::test]
async fn a_crossing_whose_answer_never_came_home_grants_no_continuation() {
    const NO_RETURNS: &str = "hive = { turn_budget = 6, quorum = 2, blind_round = false, \
                              referral = { enabled = true, returns = false } }";
    let far = FarDesk::answering("The replica lag budget is 400ms.");
    let (log, _) = run(
        NO_RETURNS,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            // Would be the continuation, if one were granted.
            (
                "planner",
                "!evidence #lag ^2 platform came back with 400ms.",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        Some(&far),
    )
    .await;

    assert_eq!(far.asked().len(), 1, "the question was still put");
    let home = log.replies("eng");
    assert!(
        home.iter()
            .all(|(_, text)| !text.contains("The replica lag budget is 400ms")),
        "the far desk's answer stays on the far desk, which is what `returns = false` \
         asks for: {home:?}"
    );
    // **The structural tell, not the text.** A continuation makes the asker
    // speak twice in immediate succession — its question, then its second pass
    // on the answer. Denied one, the floor passes to somebody else. Asserted
    // this way because the room's ordinary rotation brings the asker back later
    // anyway, so counting its rows or matching its words cannot tell a granted
    // continuation from a scheduled turn.
    let asked_at = home
        .iter()
        .position(|(author, text)| {
            author == "planner" && text.contains("What is the replica lag budget?")
        })
        .expect("the question is on the desk");
    assert_ne!(
        home[asked_at + 1].0,
        "planner",
        "the asker must not speak again on an answer it never received: {home:?}"
    );
}

/// **A continuation that says something new IS a row.**
///
/// The guard above must not cost the feature it protects: the whole reason the
/// asker speaks twice is to use what came back, and a seat that does exactly
/// that has earned its second row.
#[tokio::test]
async fn a_continuation_that_reads_the_answer_is_journaled() {
    let far = FarDesk::answering("The replica lag budget is 400ms.");
    let (log, _) = run(
        REFERRING,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            (
                "planner",
                "!evidence #lag ^2 platform came back with 400ms, which the staged plan fits.",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        Some(&far),
    )
    .await;

    assert!(
        log.replies("eng")
            .iter()
            .any(|(author, text)| author == "planner"
                && text.contains("platform came back with 400ms")),
        "the asker's conclusion on its own crossing: {:?}",
        log.replies("eng")
    );
}

/// **A room's answer is not written onto the far desk a second time.**
///
/// The single-seat path has to journal the answer there — the far desk's only
/// row IS that seat's turn, and nothing else records it. A room has already
/// written the question, every turn and the closing report, so repeating the
/// answer appended a verbatim copy of the report under the seat the library had
/// resolved, with `parent: None`. Three faults in one row: text already on the
/// desk, a conclusion attributed to one member who did not write it, and a
/// channel-level row no projection drops, reading as that member posting the
/// room's summary as their own message.
#[tokio::test]
async fn a_rooms_conclusion_is_not_echoed_onto_the_far_desk() {
    const CONCLUSION: &str = "the Platform desk settled on a 400ms budget";
    let far = Room::answering(CONCLUSION);
    let (log, _) = run_with(
        REFERRING,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        &far,
    )
    .await;

    // Nothing at all on the far desk from this side: the `Room` fixture stands
    // in for the real episode, so every row there would be an echo.
    let far_rows = log.replies("platform");
    assert!(
        far_rows.is_empty(),
        "the room wrote its own transcript; this path must add nothing: {far_rows:?}"
    );
    // And the answer still comes home, once.
    let carried: Vec<_> = log
        .replies("eng")
        .into_iter()
        .filter(|(_, text)| text.contains(CONCLUSION))
        .collect();
    assert_eq!(carried.len(), 1, "exactly one copy comes home: {carried:?}");
    assert_eq!(carried[0].0, HIVE_REFERRAL_AUTHOR);
}

/// **`deliberates = false` is the single-seat arm, unchanged.**
///
/// Every measurement taken before the room existed was taken against this
/// path, so it has to stay reachable and stay identical — a knob that changed
/// the cheap arm too would make the comparison it exists for meaningless.
#[tokio::test]
async fn a_desk_that_turns_deliberation_off_still_asks_one_seat() {
    const SEAT_ONLY: &str = "hive = { turn_budget = 6, quorum = 2, blind_round = false, \
                             referral = { enabled = true, deliberates = false } }";
    let far = Room::answering("a room would have said this");
    let (log, _) = run_with(
        SEAT_ONLY,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        &far,
    )
    .await;

    assert!(
        far.convened().is_empty(),
        "the desk said not to convene: {:?}",
        far.convened()
    );
    let seat = far.fell_back();
    assert_eq!(seat.len(), 1, "and one seat answered instead: {seat:?}");
    assert_eq!(seat[0].1, "sre", "the far desk's first eligible member");
    let returned = log
        .replies("eng")
        .into_iter()
        .find(|(_, text)| text.contains("one seat's opinion"))
        .expect("the seat's answer came home");
    assert!(
        returned.1.contains("@sre"),
        "attributed to the seat, because a seat is what answered: {}",
        returned.1
    );
}

/// **A far desk that cannot hold a room falls back to its seat.**
///
/// No `[hive]` block, one member, a quorum out of reach right now: all
/// ordinary shapes a company is allowed to have, and every one of them has a
/// correct answer that is not a failure. Refusing the crossing instead would
/// deny a company something it is entitled to do.
#[tokio::test]
async fn a_far_desk_that_cannot_deliberate_is_still_asked() {
    let far = Room::cannot();
    let (log, _) = run_with(
        REFERRING,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        &far,
    )
    .await;

    assert_eq!(far.convened().len(), 1, "it was offered the room");
    let seat = far.fell_back();
    assert_eq!(seat.len(), 1, "and answered as a seat: {seat:?}");
    assert!(
        log.replies("eng")
            .iter()
            .any(|(author, text)| author == HIVE_REFERRAL_AUTHOR
                && text.contains("one seat's opinion")),
        "the answer still comes home"
    );
}

/// **A room that broke is a failed crossing, not a retry as one seat.**
///
/// Falling through to `refer` here would bill the far desk's turns and then
/// bill a turn again for the same question — and tell the asking room nothing
/// about the first attempt.
#[tokio::test]
async fn a_room_that_breaks_does_not_then_ask_a_seat() {
    let far = Room::broken();
    let (log, outcome) = run_with(
        REFERRING,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        &far,
    )
    .await;

    assert_eq!(far.convened().len(), 1);
    assert!(
        far.fell_back().is_empty(),
        "no second attempt on the same question: {:?}",
        far.fell_back()
    );
    assert_eq!(outcome.referrals.failed, 1, "counted as a failed crossing");
    assert!(
        log.replies("eng")
            .iter()
            .any(|(author, text)| author == HIVE_REFERRAL_AUTHOR
                && text.contains("did not answer the question")),
        "and the room that spent the question is told it got nothing back: {:?}",
        log.replies("eng")
    );
}

/// **A room's prompt must not forbid the markers its protocol requires.**
///
/// `referral_prompt` closes with "Do not open your reply with a `!` marker:
/// this is not a deliberation turn, it is an answer to a colleague" — correct
/// for the single seat it was written for. Handed to a room as its task, it
/// contradicts `EpisodePrompt`'s own protocol ("Reply with ONE line only,
/// beginning with exactly one of these markers") inside the same prompt, and a
/// member that obeys the task deposits prose the fold counts for nothing.
/// Three of the first four live crossings ended `idle` with markerless turns.
#[test]
fn the_prompt_a_room_is_handed_does_not_forbid_its_own_protocol() {
    let seat = referral::referral_prompt("planner", "Engineering", "What is the lag budget?");
    assert!(
        seat.contains("Do not open your reply with a `!` marker"),
        "the single-seat prompt still says so, which is right for a seat:\n{seat}"
    );

    let room = referral::referral_room_prompt("planner", "Engineering", "What is the lag budget?");
    assert!(
        !room.contains('`'),
        "a room is told nothing about markers — its protocol owns the shape of a turn:\n{room}"
    );
    assert!(room.contains("planner on the Engineering desk"));
    assert!(room.contains("What is the lag budget?"));
    // Recovered by the same reader, from either shape.
    assert_eq!(
        referral::asked_message(&room),
        "What is the lag budget?",
        "the room prompt puts the question last, with no footer to strip"
    );
}

/// **A question that quotes the footer is not truncated at it.**
///
/// The question is operator- and agent-authored, so it can contain the
/// footer's opening sentence; searching forwards treats that copy as the
/// generated footer and cuts the question there.
/// **A ROOM question quoting the footer survives too.**
///
/// `referral_room_prompt` deliberately has no footer, so the backwards split
/// that fixed the single-seat case found the question's own copy of that
/// sentence and truncated there instead — the same defect as the forward
/// split, in the other prompt shape (Codex, #2332). The footer is now taken
/// from `referral_prompt` whole and removed only as a suffix, so a shape that
/// does not end with it keeps its question intact.
#[test]
fn a_room_question_containing_the_footers_words_is_not_truncated() {
    let question = "Answer in a few sentences. is what the customer wrote — can we refund it?";
    let room = referral::referral_room_prompt("planner", "Engineering", question);
    assert_eq!(
        referral::asked_message(&room),
        question,
        "a footerless prompt has nothing to strip"
    );
}

#[test]
fn a_question_containing_the_footers_words_survives_unwrapping() {
    let question = "Answer in a few sentences. is what they told us — what is the lag budget?";
    let prompt = referral::referral_prompt("planner", "Engineering", question);
    assert_eq!(
        referral::asked_message(&prompt),
        question,
        "the generated footer is last, so only the last occurrence delimits it"
    );
}

/// **A room that could not answer is reported as the DESK failing, not a seat.**
///
/// `forward` returns from the `deliberate` arm before `refer` ever runs, so on
/// a room failure no seat was asked anything. Naming the seat the library
/// resolved credits a teammate with a refusal it never made, on a desk where
/// nobody reading the row can check — the same defect `room_note` removes on
/// the success path (CodeRabbit, #2332).
#[tokio::test]
async fn a_room_that_could_not_answer_is_not_blamed_on_a_seat() {
    let far = Room::broken();
    let (log, _) = run_with(
        REFERRING,
        &[
            (
                "planner",
                "!question #lag What is the replica lag budget? @#platform",
            ),
            ("scout", "!propose #stage Stage the rollout behind a flag."),
            ("critic", "!support #stage ^1 Staging fits the lag budget."),
            ("planner", "!commit #stage ^3 Recorded."),
        ],
        &far,
    )
    .await;

    let note = log
        .replies("eng")
        .into_iter()
        .find(|(_, text)| text.contains("did not answer the question"))
        .expect("the room that spent the question is told it got nothing back");
    assert_eq!(note.0, HIVE_REFERRAL_AUTHOR);
    assert!(
        note.1.contains("Platform"),
        "the desk that was asked is named: {}",
        note.1
    );
    assert!(
        !note.1.contains("@sre") && !note.1.contains("@dba"),
        "and no seat is blamed for a refusal it never made: {}",
        note.1
    );
}

#[test]
fn a_rooms_failure_names_the_desk_and_no_seat_within_it() {
    assert_eq!(
        referral::room_unanswered_note("Platform"),
        "the Platform desk did not answer the question."
    );
    // The seat form is unchanged for a crossing that actually asked a seat.
    assert_eq!(
        referral::unanswered_note("sre", "Platform"),
        "@sre on the Platform desk did not answer the question."
    );
}

#[test]
fn a_rooms_answer_names_the_desk_and_no_seat_within_it() {
    let note = referral::room_note("Platform", "  !support #stage It holds.  ");
    assert_eq!(
        note,
        "the Platform desk answered the question: support #stage It holds."
    );
}

#[test]
fn a_returned_answer_names_its_author_and_desk_and_carries_no_marker() {
    let note = referral::returned_note("sre", "Platform", "  !support #stage It holds.  ");
    assert_eq!(
        note,
        "@sre on the Platform desk answered the question: support #stage It holds."
    );
}

#[test]
fn the_referral_prompt_frames_a_colleagues_question_not_a_deliberation_turn() {
    let prompt = referral::referral_prompt("Planner", "Engineering", "What is the lag budget?");
    assert!(prompt.contains("Planner on the Engineering desk"));
    assert!(prompt.contains("What is the lag budget?"));
    assert!(
        prompt.contains("Do not open your reply with a `!` marker"),
        "the far teammate is answering a colleague, not depositing a move on its own \
         desk's board:\n{prompt}"
    );
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

/// The referral block is parsed off `[[group_chat]].hive`, not invented here.
#[test]
fn the_manifest_parses_a_referral_block() {
    let manifest = two_desks(
        "hive = { referral = { enabled = true, max_hops = 3, reach = \"channels\", \
         returns = false, peer_cap = 4 } }",
    );
    let desk = desk_of(&manifest, "eng").expect("a room");
    let referral = &desk.config.referral;
    assert!(referral.enabled());
    assert_eq!(referral.peer_cap(), 4);
    let policy = referral.policy();
    assert_eq!(policy.max_hops, 3);
    assert!(!policy.returns);
    assert!(policy.reach.crosses());
    assert!(
        !policy.reach.addresses_desks(),
        "`channels` lets a turn run elsewhere without making `@#desk` mean anything"
    );
}

/// Every check here catches a policy that would be *silently* inert. A desk
/// that asks nothing looks exactly like a desk whose members had nothing to
/// ask, so a typo has to be a validation error rather than a quiet no-op.
#[test]
fn the_manifest_refuses_a_referral_policy_that_could_never_fire() {
    let problems = |hive: &str| record(&two_desks(hive)).manifest.validate();

    let found = problems("hive = { referral = { enabled = true, reach = \"everywhere\" } }");
    assert!(
        found.iter().any(|p| p.contains("hive.referral.reach")),
        "{found:?}"
    );

    for key in ["max_hops", "peer_cap"] {
        let found = problems(&format!(
            "hive = {{ referral = {{ enabled = true, {key} = 0 }} }}"
        ));
        assert!(
            found
                .iter()
                .any(|p| p.contains(&format!("hive.referral.{key} = 0"))),
            "{key}: {found:?}"
        );
    }

    // A round trip is two hops. One hop plus `returns` describes a question
    // whose answer is thrown away, which is worse than refusing it.
    let found = problems("hive = { referral = { enabled = true, max_hops = 1 } }");
    assert!(
        found.iter().any(|p| p.contains("a round trip is two hops")),
        "{found:?}"
    );
    // Declared one-way, it is a policy somebody meant.
    let found = problems("hive = { referral = { enabled = true, max_hops = 1, returns = false } }");
    assert!(!found.iter().any(|p| p.contains("round trip")), "{found:?}");

    // And the ordinary opted-in block is accepted.
    assert!(problems(REFERRING).is_empty(), "{:?}", problems(REFERRING));
}

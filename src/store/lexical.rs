//! One lexical ranker behind every [`ContextStore::search`].
//!
//! [`ContextStore::search`]: crate::ports::ContextStore::search
//!
//! ## What was wrong
//!
//! `ContextStore::search` is the floor under every retrieval path: the memory
//! loop before a turn, openhuman's `memory_loader` on each turn, and the
//! `memory_recall` tool an agent can call. `mongodb.rs`, `fs.rs` and
//! `sqlite.rs` each carried their own copy of it, and all three copies were the
//! same two mistakes:
//!
//! ```text
//! if let Some(pos) = body.find(query) {
//!     hits.push(ChunkHit { … score: 1.0 });
//! }
//! ```
//!
//! 1. **`body.find(query)` is a substring test, not a search.** On the memory
//!    loop the `query` is the whole incoming message, so a hit requires that
//!    entire message to appear verbatim inside a stored chunk. In practice that
//!    never happens — one word of difference and there is no hit at all. It is
//!    all-or-nothing, never "looks related". And because all three returned
//!    `score: 1.0`, a caller's `min_relevance_score` could not filter anything
//!    either.
//! 2. **`if hits.len() >= limit { break; }` came before any sorting** — of
//!    which there was none. Mongo reads by `ord`, sqlite by `rowid`, fs by
//!    index order, so with more than `limit` matches the *oldest* won over the
//!    *best*.
//!
//! A fourth copy — `store/tinycortex.rs::score_chunks` — already did the right
//! thing (distinct-token overlap, sorted, zero-overlap dropped). That is what
//! made this easy to miss: the good version was three files away in the same
//! directory. This module is that version, hardened, and now used by all of
//! them.
//!
//! ## What it does instead
//!
//! Token overlap, with one addition: **terms are not weighted equally**. A
//! query that is a whole message is half made of "the", "a", "for" and "and".
//! Under flat overlap a chunk sharing four stopwords outranks one sharing two
//! rare words, which is exactly backwards. Each term is therefore weighted by
//! its rarity among the candidates of *this* company (the BM25 shape of IDF):
//!
//! ```text
//! idf(t)  = ln(1 + (N − df + 0.5) / (df + 0.5))   with df = max(df(t), 1)
//! len(d)  = min(1, 1 / (1 − b + b · |d|/avg))     with b = LENGTH_NORM_B
//! score(d) = len(d) · Σ idf(matched terms) / Σ idf(all terms)
//! ```
//!
//! A term present in every chunk ends up with almost no weight (at N = 200 and
//! df = 200, idf ≈ 0.0025); a term in a single chunk carries the most. The
//! division keeps the score inside `[0, 1]` as [`ChunkHit`] promises, and makes
//! it comparable across queries of different lengths.
//!
//! ### Two corrections a measurement forced
//!
//! Both came out of running this against a real 402-chunk context store, and
//! both fix the same failure: a score of 1.00 for a chunk that has nothing to
//! do with the question.
//!
//! **1. `df.max(1)` — a term that appears nowhere still counts.** The first
//! version gave such a term weight zero, reasoning that it says nothing about
//! the choice *between* chunks. True, and still wrong: for a question about a
//! subject the memory does not hold, those are precisely all the content words,
//! and then only stopword weight is left in the denominator. Every chunk
//! containing "the" and "of" scored 1.00. Measured: an off-topic question
//! returned five hits at 1.00, all five about an unrelated loan. Counting an
//! unfindable term as if it were rare (df = 1) costs the query part of its
//! weight, which is exactly the message: the memory does not have this subject.
//!
//! **2. The length penalty.** See [`LENGTH_NORM_B`]: a chunk that mentions
//! everything mentions nothing in particular.
//!
//! What the score means afterwards: *of the evidence findable in this store,
//! how much does this chunk carry?* — a ranking measure, not an absolute
//! similarity. Anyone using it as a threshold (openhuman's `memory_loader` does,
//! with `min_relevance_score = 0.4`) should know that.
//!
//! Terms are matched as *substrings*, as in the scorer this grew out of:
//! "revenue" also hits "revenues". That gives short words ("in", "the") false
//! hits, but those are exactly the words that appear everywhere and therefore
//! weigh almost nothing — the weighting repairs what the substring match loosens.
//!
//! Weighting is per query over the candidates of that company, not over a global
//! index: there is no second structure that can fall behind, and two tenants
//! share no statistics.
//!
//! ## Why an accumulator and not a `Vec`
//!
//! IDF needs the document frequency over *all* candidates, so the naive shape is
//! two passes or "load everything into memory". That is not acceptable for the
//! Mongo backend: it would put a company's entire `context_chunks` collection in
//! RAM. With [`Ranker`] each store hands over its candidates one at a time in
//! whatever order it already reads them, only candidates *with* overlap keep a
//! snippet, and the weighting follows at the end from the df counted along the
//! way. One pass, and memory grows with the number of hits rather than with the
//! size of the collection.

use crate::ports::types::{ChunkAddr, ChunkHit};

/// How many bytes of context a snippet carries either side of the first matched
/// term.
///
/// This was 24, which is not a memory but a fragment: on a body shaped
/// `Task: …\nOutcome: …` it yields half of one sentence, and the memory loop has
/// nothing else — it injects `hit.snippet`, it never reads the body. 160 comes
/// from the budget that already exists: `MAX_HISTORY_CHARS` (2000) over
/// `RETRIEVE_TOP_K` (5) is 400 characters per hit, and 2 × 160 bytes plus the
/// term stays under that, so all five hits still fit in the preamble instead of
/// the fifth being cut away.
pub const SNIPPET_WINDOW_BYTES: usize = 160;

/// Below this score a candidate does not count as a hit.
///
/// Needed because overlap search, unlike the substring test it replaces, almost
/// always finds *something*: one shared word is enough. Without a floor the
/// memory loop fills its five slots with chunks that share a single stopword
/// with the question, and that is noise in the preamble of every turn.
///
/// Chosen on a measurement over a real 402-chunk context store, with twelve
/// questions the memory does hold and five it does not:
///
/// ```text
/// questions with grounding in memory : top-5 scores 0.23 – 1.00
/// questions without grounding        : top-5 scores 0.19 – 0.30
/// ```
///
/// Those two overlap, and that is not measurement error but the limit of
/// lexical search: a question about working from home when a child is ill
/// shares enough everyday words with an old mail instruction to land at 0.30.
/// Separating them properly needs meaning, and therefore embeddings — a
/// separate decision with its own per-turn cost.
///
/// 0.20 is therefore a choice, not a law: it removes the worst cold question
/// entirely (three hits at 0.19 → none) and at most costs the weakest of five
/// on a warm one.
pub const MIN_RELEVANCE: f64 = 0.20;

/// How hard a long chunk is penalised — the `b` of BM25.
///
/// Necessary, not tidy: measured over a real 402-chunk store the median chunk is
/// 932 characters, but the fourteen that contain *every* term of an arbitrary
/// query have a median length of 8016. Those are the rows carrying an
/// "open work handed to you" tail that enumerates every task in flight at the
/// time. Without this correction they sat at score 1.00 on top of questions they
/// had nothing to do with — a chunk that mentions everything mentions nothing in
/// particular.
///
/// 0.75 is the BM25 default and needs no change here: a candidate of average
/// length keeps factor 1.0, one of eight times the average keeps 0.16 of it, and
/// shorter than average earns no bonus (the factor is clamped at 1.0 — a short
/// note is not more relevant, it is only shorter).
const LENGTH_NORM_B: f64 = 0.75;

/// Collects candidates and ranks them by weighted token overlap with the query.
///
/// Usage: [`Ranker::new`] with the query, [`Ranker::offer`] per candidate chunk
/// in whatever order the store already reads them, and [`Ranker::best`] at the
/// end.
pub struct Ranker {
    /// The distinct, lowercased query terms, in query order.
    terms: Vec<String>,
    /// Document frequency per term: how many candidates contain it.
    df: Vec<usize>,
    /// How many candidates went past (the N of the IDF).
    candidates: usize,
    /// The summed body length of *all* candidates, for the average length in the
    /// length norm. Counted during the same pass, so it costs no second trip to
    /// the store.
    total_len: usize,
    /// Only the candidates *with* overlap. The rest is not kept.
    hits: Vec<Candidate>,
}

/// A candidate with overlap, before weighting: which terms it matched is
/// settled, what that is worth is not — that depends on the candidates still to
/// come.
struct Candidate {
    addr: String,
    snippet: String,
    matched: Vec<usize>,
    /// The length of the whole body, not of the snippet: the penalty is about
    /// how much a chunk covers, not about how much of it we showed.
    len: usize,
}

impl Ranker {
    /// Starts a ranking for `query`.
    ///
    /// A query without terms (empty, or only whitespace) never yields hits — the
    /// same as before, and the only safe choice: returning "everything" would
    /// push the entire history into a turn's preamble.
    pub fn new(query: &str) -> Self {
        let mut terms: Vec<String> = Vec::new();
        for raw in query.split_whitespace() {
            let term = raw.to_lowercase();
            if term.is_empty() || terms.contains(&term) {
                continue;
            }
            terms.push(term);
        }
        let df = vec![0usize; terms.len()];
        Self {
            terms,
            df,
            candidates: 0,
            total_len: 0,
            hits: Vec::new(),
        }
    }

    /// Whether this ranking can find anything at all.
    ///
    /// Lets a store skip the trip instead of reading the whole collection only
    /// to return nothing.
    pub fn matches_nothing(&self) -> bool {
        self.terms.is_empty()
    }

    /// Takes one candidate into account.
    pub fn offer(&mut self, addr: &str, body: &str) {
        if self.terms.is_empty() {
            return;
        }
        self.candidates += 1;
        self.total_len += body.len();
        let lower = body.to_lowercase();

        let mut matched = Vec::new();
        // The earliest position at which any matched term occurs: that is where
        // the snippet is anchored, so the reader sees the match and not the
        // first 320 bytes of the chunk.
        let mut anchor: Option<(usize, usize)> = None;
        for (i, term) in self.terms.iter().enumerate() {
            let Some(pos) = lower.find(term.as_str()) else {
                continue;
            };
            matched.push(i);
            self.df[i] += 1;
            if anchor.is_none_or(|(earlier, _)| pos < earlier) {
                anchor = Some((pos, term.len()));
            }
        }
        if matched.is_empty() {
            return;
        }

        let snippet = match anchor {
            Some((pos, len)) => snippet_around(body, pos, len),
            None => body.to_string(),
        };
        self.hits.push(Candidate {
            addr: addr.to_string(),
            snippet,
            matched,
            len: body.len(),
        });
    }

    /// Ranks, then returns the best `limit`.
    ///
    /// **Sorting happens here, and truncation only after** — that is half of the
    /// bug this module replaces. Equal scores keep the order the store handed
    /// them over in (`sort_by` is stable), so a store reading by `ord` or
    /// `rowid` keeps its own explainable order.
    pub fn best(self, limit: usize) -> Vec<ChunkHit> {
        if self.hits.is_empty() {
            return Vec::new();
        }
        let weight: Vec<f64> = self
            .df
            .iter()
            .map(|&df| idf(self.candidates, df.max(1)))
            .collect();
        let total: f64 = weight.iter().sum();
        let average_len = if self.candidates > 0 {
            self.total_len as f64 / self.candidates as f64
        } else {
            0.0
        };

        let mut hits: Vec<ChunkHit> = self
            .hits
            .into_iter()
            .filter_map(|c| {
                let score = if total > 0.0 {
                    c.matched.iter().map(|&i| weight[i]).sum::<f64>() / total
                } else {
                    // Unreachable while a candidate only becomes a hit by
                    // matching a term (that term then has df ≥ 1, hence weight
                    // > 0). Kept as a backstop for the day someone wires
                    // `offer` differently: flat overlap is then the fairest
                    // thing left, and in any case not a division by zero.
                    c.matched.len() as f64 / self.terms.len() as f64
                };
                let score = score * length_factor(c.len, average_len);
                (score >= MIN_RELEVANCE).then(|| ChunkHit {
                    addr: ChunkAddr::new(c.addr),
                    snippet: c.snippet,
                    score: score.clamp(0.0, 1.0),
                })
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        hits
    }
}

/// Ranks a sequence of `(addr, body)` pairs in one call.
///
/// For stores that already hold their candidates in memory; anything that
/// streams uses [`Ranker`] directly and keeps the bodies out of memory.
pub fn rank<'a>(
    candidates: impl IntoIterator<Item = (&'a str, &'a str)>,
    query: &str,
    limit: usize,
) -> Vec<ChunkHit> {
    let mut ranker = Ranker::new(query);
    if ranker.matches_nothing() {
        return Vec::new();
    }
    for (addr, body) in candidates {
        ranker.offer(addr, body);
    }
    ranker.best(limit)
}

/// The weight of a term occurring in `df` of `n` candidates.
///
/// The BM25 shape, with the `+1` inside the logarithm so the result is never
/// negative (which classic `ln(N/df)` becomes as soon as a term is in more than
/// half the candidates — and for a query that is a whole message, that is the
/// normal case rather than the exception).
fn idf(n: usize, df: usize) -> f64 {
    let n = n as f64;
    let df = df as f64;
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

/// How much of its score a candidate of `len` keeps.
///
/// The length component of BM25: `1 / (1 − b + b · dl/avgdl)`, clamped at 1.0. A
/// candidate of average length keeps exactly 1.0.
fn length_factor(len: usize, average: f64) -> f64 {
    if average <= 0.0 {
        return 1.0;
    }
    let ratio = len as f64 / average;
    let denominator = 1.0 - LENGTH_NORM_B + LENGTH_NORM_B * ratio;
    if denominator <= 0.0 {
        return 1.0;
    }
    (1.0 / denominator).min(1.0)
}

/// A window around `pos` of `body`, always on character boundaries.
///
/// `pos` comes from a `find` on the lowercased copy of `body`. For Latin script
/// that is the same byte position, but not for every character (Turkish `İ`
/// lowercases to three bytes where it was two), so the position is clamped to
/// the length of `body` first. The previous implementations did not: there `pos`
/// could fall outside `body` and the slice then cut a character in half — a
/// panic in a search function, over a capital letter.
fn snippet_around(body: &str, pos: usize, term_len: usize) -> String {
    let pos = pos.min(body.len());
    let raw_start = pos.saturating_sub(SNIPPET_WINDOW_BYTES);
    let raw_end = pos
        .saturating_add(term_len)
        .saturating_add(SNIPPET_WINDOW_BYTES)
        .min(body.len());
    let start = (raw_start..=pos)
        .find(|&i| body.is_char_boundary(i))
        .unwrap_or(0);
    let end = (raw_end..=body.len())
        .find(|&i| body.is_char_boundary(i))
        .unwrap_or(body.len());
    if start >= end {
        return String::new();
    }
    body[start..end].to_string()
}

#[cfg(test)]
mod test {
    use super::*;

    fn scores(query: &str, candidates: &[(&str, &str)]) -> Vec<(String, f64)> {
        rank(candidates.iter().copied(), query, usize::MAX)
            .into_iter()
            .map(|h| (h.addr.as_ref().to_string(), h.score))
            .collect()
    }

    #[test]
    fn an_empty_query_finds_nothing() {
        assert!(scores("", &[("a", "anything at all")]).is_empty());
        assert!(scores("   ", &[("a", "anything at all")]).is_empty());
    }

    #[test]
    fn one_word_of_difference_is_still_a_hit() {
        // This is the core of it: the substring test that stood here returned
        // NOTHING, because the query does not occur verbatim in the body.
        let out = scores(
            "draw up a quarterly overview of revenue",
            &[
                ("a", "Task: draw up an overview of revenue\nOutcome: done"),
                ("b", "Task: file the supplier invoices\nOutcome: done"),
            ],
        );
        assert_eq!(out.len(), 1, "partial overlap must hit");
        assert_eq!(out[0].0, "a");
        assert!(out[0].1 > 0.0);
    }

    #[test]
    fn a_term_that_appears_nowhere_still_counts() {
        // Measured and reverted: at first terms with df = 0 were given weight
        // zero, "because they say nothing about the choice between chunks".
        // That is exactly backwards. For a question about a subject NOT in
        // memory, only stopword weight then remained in the denominator, and
        // every arbitrary chunk containing "the" and "of" scored 1.00.
        //
        // An unfindable term therefore counts as if it were rare (df = 1): it
        // costs the query part of its weight, which is the right message —
        // memory does not hold this subject.
        let with = scores(
            "quarterly figures shipyard",
            &[("a", "the quarterly figures for February")],
        );
        let without = scores(
            "quarterly figures",
            &[("a", "the quarterly figures for February")],
        );
        assert!(with[0].1 < without[0].1, "the unfindable term must cost");
    }

    #[test]
    fn a_question_about_an_unknown_subject_scores_much_lower() {
        // The regression above, as a test on the outcome. Deliberately a ratio
        // and not an absolute threshold: IDF is a corpus statistic and with
        // three chunks it says little. What must hold at *every* corpus size is
        // that a question about an unknown subject lands below one about a known
        // subject.
        //
        // Measured on a real 402-chunk store: an off-topic question went from
        // 1.00 (five hits about an unrelated loan) to 0.19, below the floor.
        let memory: &[(&str, &str)] = &[
            (
                "a",
                "Task: produce the quarterly revenue figures for the north region\nOutcome: ready",
            ),
            (
                "b",
                "Task: send the quotation to the customer in Ashford\nOutcome: sent",
            ),
            (
                "c",
                "Task: file the supplier invoices for March\nOutcome: done",
            ),
        ];
        let known = scores("send the quotation to the customer in Ashford", memory);
        let unknown = scores(
            "put the christmas tree in the canteen and hang some lights on it",
            memory,
        );
        assert!(
            unknown.first().map(|h| h.1).unwrap_or(0.0) < known[0].1 / 2.0,
            "unknown {unknown:?} must land far below known {known:?}"
        );
    }

    #[test]
    fn a_long_chunk_that_mentions_everything_does_not_win() {
        let short = "Task: draw up the quotation for Ashford\nOutcome: sent";
        let long = format!(
            "Task: take a screenshot\nOutcome: cannot. [Open work: {} quotation Ashford              quarterly revenue north supplier invoices]",
            "noise ".repeat(400)
        );
        let out = scores(
            "draw up the quotation for Ashford",
            &[("long", &long), ("short", short)],
        );
        assert_eq!(
            out[0].0, "short",
            "a chunk that mentions everything mentions nothing in particular"
        );
    }

    #[test]
    fn rare_words_outweigh_stopwords() {
        // "the" and "of" are in all three, "quarterly" in one. The candidate
        // sharing only stopwords must not win.
        let out = scores(
            "the quarterly figures of March",
            &[
                ("stop", "the minutes of the meeting of Tuesday"),
                ("hit", "the quarterly figures of February are ready"),
                ("noise", "the agenda of the coming week"),
            ],
        );
        assert_eq!(out[0].0, "hit", "the rare term must decide the ranking");
        // The stopword candidates fall through the floor; if they survive, they
        // are at least below "hit".
        for (addr, score) in out.iter().skip(1) {
            assert!(score < &out[0].1, "{addr} must not outrank the real hit");
        }
    }

    #[test]
    fn the_best_beats_the_oldest() {
        // The second half of the bug: the previous code truncated to `limit` in
        // insertion order, so "old1" would have won here.
        let candidates: Vec<(&str, &str)> = vec![
            ("old1", "revenue"),
            ("old2", "revenue"),
            ("old3", "revenue"),
            ("new", "revenue margin quarter report"),
        ];
        let out = rank(candidates, "revenue margin quarter report", 1);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].addr.as_ref(),
            "new",
            "sorting belongs before cutting"
        );
    }

    #[test]
    fn equal_scores_keep_the_order_of_the_store() {
        let out = scores("revenue", &[("first", "revenue"), ("second", "revenue")]);
        assert_eq!(out[0].0, "first");
        assert_eq!(out[1].0, "second");
    }

    #[test]
    fn the_score_stays_inside_the_port_contract() {
        for (_, score) in scores(
            "revenue margin",
            &[("a", "revenue margin"), ("b", "revenue")],
        ) {
            assert!((0.0..=1.0).contains(&score), "score outside [0,1]: {score}");
        }
    }

    #[test]
    fn the_snippet_wraps_the_hit_and_not_the_start_of_the_body() {
        let body = format!("{}NEEDLE{}", "x".repeat(400), "y".repeat(400));
        let out = rank([("a", body.as_str())], "needle", 1);
        assert_eq!(out.len(), 1);
        let s = &out[0].snippet;
        assert!(s.contains("NEEDLE"), "the snippet must contain the hit");
        assert!(
            s.starts_with('x') && s.ends_with('y'),
            "with context on both sides"
        );
        assert!(s.len() <= 2 * SNIPPET_WINDOW_BYTES + "needle".len());
    }

    #[test]
    fn multibyte_text_never_splits_a_character() {
        // 'é' is two bytes; a window computed in bytes must still land on a
        // character boundary, or this is a panic instead of a search result.
        let body = format!("{}needle{}", "é".repeat(200), "é".repeat(200));
        let out = rank([("a", body.as_str())], "needle", 1);
        assert_eq!(out.len(), 1);
        assert!(out[0].snippet.contains("needle"));
    }

    #[test]
    fn case_does_not_matter() {
        let out = scores("REVENUE", &[("a", "the Revenue of March")]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn no_overlap_is_no_hit() {
        assert!(scores("quarterly", &[("a", "the agenda for tomorrow")]).is_empty());
    }
}

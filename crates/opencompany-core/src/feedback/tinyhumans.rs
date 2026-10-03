//! Forwarding feedback to the TinyHumans hub behind a mockable client.
//!
//! A provisioned instance — one configured with a TinyHumans credential — sends
//! its feedback to the backend enrichment hub instead of filing a GitHub issue
//! itself, so the report is recorded on behalf of the credential's owner and
//! the hub decides whether an issue is ultimately filed.
//!
//! The [`TinyHumansClient`] trait and its offline [`MockTinyHumansClient`]
//! compile in the default build so the whole routing decision is exercised
//! without linking a network crate. Only the real HTTP client
//! [`HttpTinyHumansClient`] is gated behind the `tinyhumans` feature.
//!
//! Two invariants this module must not break:
//!
//! * **Only scrubbed text leaves.** The body handed to [`TinyHumansClient::ingest`]
//!   is the same one the scrub-then-preview gate produced, byte for byte, so
//!   forwarding is not a second path around [`crate::feedback::scrub`].
//! * **The credential never appears in a body.** It travels only as the
//!   `Authorization` header, held by the HTTP client and never by a request.

use std::sync::Mutex as StdMutex;

use async_trait::async_trait;

use crate::Result;
use crate::feedback::board::{
    BoardComment, BoardDetail, BoardItem, BoardPage, BoardQuery, BoardSort, VoteValue,
};
use crate::feedback::types::FeedbackCategory;

/// The product discriminator the hub routes on. Feedback from this runtime is
/// always attributed to opencompany, whichever company reported it.
///
/// Re-exported from [`crate::product::PRODUCT_IDENTITY`] rather than holding
/// its own `"opencompany"` literal: that module is this crate's single
/// source of truth for the product name, and a second copy of the string
/// here would be exactly the kind of duplicate this task exists to remove
/// (issue #376). If the two ever need to differ — e.g. the hub's `product`
/// field wants a different spelling than the `x-sdk-name` header — that is a
/// deliberate divergence to introduce explicitly, not something to default
/// into by leaving a stale literal in place.
pub const PRODUCT: &str = crate::product::PRODUCT_IDENTITY;

/// One feedback report to forward.
///
/// `title` and `body` are the byte-exact strings the preview showed; `origin`
/// and `external_ref` let an operator trace a hub item back to the local one
/// without carrying anything private across.
#[derive(Clone, Debug, PartialEq)]
pub struct IngestRequest {
    /// The reported category, mapped to the hub's coarser type on the wire.
    pub category: FeedbackCategory,
    /// The issue title.
    pub title: String,
    /// The scrubbed, signed body.
    pub body: String,
    /// The reporting company's `@handle`.
    pub origin: String,
    /// The local [`FeedbackItem`](crate::feedback::FeedbackItem) id.
    pub external_ref: String,
}

impl IngestRequest {
    /// The hub's `type` for this report.
    ///
    /// The hub accepts only `feature | bug`, a coarser split than the local
    /// taxonomy. Nothing is lost: the precise category is the first line of
    /// every body (`**Category:** …`), and it also travels as `external_ref`'s
    /// companion in the local store.
    pub fn wire_type(&self) -> &'static str {
        match self.category {
            // Something the product did wrong.
            FeedbackCategory::Bug | FeedbackCategory::WrongOutput => "bug",
            // Something the product does not do (well enough) yet.
            FeedbackCategory::MissingCapability
            | FeedbackCategory::TemplateGap
            | FeedbackCategory::ApprovalFriction
            | FeedbackCategory::Docs => "feature",
        }
    }
}

/// What the hub did with a forwarded report.
#[derive(Clone, Debug, PartialEq)]
pub enum IngestOutcome {
    /// The hub accepted the report into the enrichment pipeline.
    Accepted {
        /// The hub's id for the report, when it returned one.
        remote_id: Option<String>,
    },
    /// The hub received the report but its moderation rejected it.
    Rejected {
        /// The human-safe moderation reason.
        reason: String,
    },
    /// The owner's daily feedback limit was reached; nothing was recorded.
    RateLimited {
        /// The human-safe limit message.
        reason: String,
    },
}

/// The TinyHumans backend, scoped to feedback: ingestion plus the shared board.
///
/// The board half is a straight proxy — this runtime holds no board state, it
/// only lends the console its credential (see [`crate::feedback::board`]).
#[async_trait]
pub trait TinyHumansClient: Send + Sync {
    /// Forwards one report to the hub, recorded as the credential's owner.
    async fn ingest(&self, request: &IngestRequest) -> Result<IngestOutcome>;

    /// One page of the shared board, ordered and filtered per `query`.
    async fn list_board(&self, query: BoardQuery) -> Result<BoardPage>;

    /// One board item with its comments.
    async fn board_item(&self, id: &str) -> Result<BoardDetail>;

    /// Casts (or retracts) this credential's vote, returning the updated item.
    async fn vote_board_item(&self, id: &str, value: VoteValue) -> Result<BoardItem>;

    /// Adds a comment, returning the stored comment.
    async fn comment_board_item(&self, id: &str, body: &str) -> Result<BoardComment>;
}

/// An in-memory [`TinyHumansClient`] for offline tests.
///
/// Every forwarded request is recorded so a test can assert the *scrubbed* body
/// crossed the boundary. Seed a non-accepting outcome with
/// [`with_outcome`](Self::with_outcome) or a transport failure with
/// [`with_failure`](Self::with_failure).
#[derive(Debug)]
pub struct MockTinyHumansClient {
    forwarded: StdMutex<Vec<IngestRequest>>,
    outcome: IngestOutcome,
    failure: Option<String>,
    /// The seeded board. Empty unless a test calls [`with_board`](Self::with_board),
    /// so a test that only cares about ingestion sees an empty board rather than
    /// invented rows.
    board: StdMutex<Vec<BoardItem>>,
    /// Comments per board-item id.
    comments: StdMutex<std::collections::HashMap<String, Vec<BoardComment>>>,
}

impl Default for MockTinyHumansClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTinyHumansClient {
    /// A mock that accepts everything.
    pub fn new() -> Self {
        Self {
            forwarded: StdMutex::new(Vec::new()),
            outcome: IngestOutcome::Accepted {
                remote_id: Some("hub-1".to_string()),
            },
            failure: None,
            board: StdMutex::new(Vec::new()),
            comments: StdMutex::new(std::collections::HashMap::new()),
        }
    }

    /// Seeds the board this mock serves.
    pub fn with_board(self, items: Vec<BoardItem>) -> Self {
        *self.board.lock().expect("mock poisoned") = items;
        self
    }

    /// Seeds the comments on one board item.
    pub fn with_comments(self, id: &str, comments: Vec<BoardComment>) -> Self {
        self.comments
            .lock()
            .expect("mock poisoned")
            .insert(id.to_string(), comments);
        self
    }

    /// Returns `outcome` instead of accepting.
    pub fn with_outcome(mut self, outcome: IngestOutcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// Fails every forward with `message`, simulating an unreachable hub.
    pub fn with_failure(mut self, message: &str) -> Self {
        self.failure = Some(message.to_string());
        self
    }

    /// A snapshot of every request forwarded through this mock.
    pub fn forwarded(&self) -> Vec<IngestRequest> {
        self.forwarded.lock().expect("mock poisoned").clone()
    }

    /// The configured failure as an error, if any.
    fn failure_error(&self) -> Option<crate::error::OpenCompanyError> {
        self.failure
            .as_ref()
            .map(|message| crate::error::OpenCompanyError::TinyHumans {
                code: "unreachable".to_string(),
                message: message.clone(),
            })
    }

    /// The stored item with `id`, or a hub-shaped 404.
    fn find(&self, id: &str) -> Result<BoardItem> {
        self.board
            .lock()
            .expect("mock poisoned")
            .iter()
            .find(|item| item.id == id)
            .cloned()
            .ok_or_else(|| crate::error::OpenCompanyError::TinyHumans {
                code: "http_404".to_string(),
                message: format!("no board item {id}"),
            })
    }
}

#[async_trait]
impl TinyHumansClient for MockTinyHumansClient {
    async fn ingest(&self, request: &IngestRequest) -> Result<IngestOutcome> {
        // Record before failing: a test asserting "we tried to send X" still
        // sees the attempt on the failure path.
        self.forwarded
            .lock()
            .expect("mock poisoned")
            .push(request.clone());
        if let Some(error) = self.failure_error() {
            return Err(error);
        }
        Ok(self.outcome.clone())
    }

    async fn list_board(&self, query: BoardQuery) -> Result<BoardPage> {
        if let Some(error) = self.failure_error() {
            return Err(error);
        }
        let query = query.clamped();
        let mut items: Vec<BoardItem> = self
            .board
            .lock()
            .expect("mock poisoned")
            .iter()
            .filter(|item| query.kind.is_none_or(|kind| item.kind == kind))
            .filter(|item| query.status.is_none_or(|status| item.status == status))
            .cloned()
            .collect();
        match query.sort {
            // The mock has no time decay to model, so `hot` and `top` agree
            // here. What a route test asserts is that the ordering *travelled*,
            // not that this crate reimplements the hub's ranking.
            BoardSort::Hot | BoardSort::Top => {
                items.sort_by_key(|item| std::cmp::Reverse(item.score))
            }
            BoardSort::New => items.sort_by(|a, b| b.created_at.cmp(&a.created_at)),
        }
        let total = items.len() as u32;
        let skip = ((query.page - 1) * query.limit) as usize;
        let page: Vec<BoardItem> = items
            .into_iter()
            .skip(skip)
            .take(query.limit as usize)
            .collect();
        Ok(BoardPage {
            items: page,
            total,
            page: query.page,
            limit: query.limit,
        })
    }

    async fn board_item(&self, id: &str) -> Result<BoardDetail> {
        if let Some(error) = self.failure_error() {
            return Err(error);
        }
        Ok(BoardDetail {
            item: self.find(id)?,
            comments: self
                .comments
                .lock()
                .expect("mock poisoned")
                .get(id)
                .cloned()
                .unwrap_or_default(),
        })
    }

    async fn vote_board_item(&self, id: &str, value: VoteValue) -> Result<BoardItem> {
        if let Some(error) = self.failure_error() {
            return Err(error);
        }
        // Confirm it exists before mutating, so a 404 reads the same as the hub's.
        self.find(id)?;
        let mut board = self.board.lock().expect("mock poisoned");
        let item = board
            .iter_mut()
            .find(|item| item.id == id)
            .expect("checked above");
        // Retract the previous vote, then apply the new one — the same
        // arithmetic the hub does, so a console asserting "my second upvote did
        // not double-count" sees the real behaviour offline.
        match item.my_vote {
            VoteValue::Up => item.upvotes = item.upvotes.saturating_sub(1),
            VoteValue::Down => item.downvotes = item.downvotes.saturating_sub(1),
            VoteValue::None => {}
        }
        match value {
            VoteValue::Up => item.upvotes += 1,
            VoteValue::Down => item.downvotes += 1,
            VoteValue::None => {}
        }
        item.my_vote = value;
        item.score = i64::from(item.upvotes) - i64::from(item.downvotes);
        Ok(item.clone())
    }

    async fn comment_board_item(&self, id: &str, body: &str) -> Result<BoardComment> {
        if let Some(error) = self.failure_error() {
            return Err(error);
        }
        self.find(id)?;
        let stored = {
            let mut comments = self.comments.lock().expect("mock poisoned");
            let thread = comments.entry(id.to_string()).or_default();
            let comment = BoardComment {
                id: format!("comment-{}", thread.len() + 1),
                author: Some("you".to_string()),
                body: body.to_string(),
                created_at: "1970-01-01T00:00:00.000Z".to_string(),
            };
            thread.push(comment.clone());
            comment
        };
        let mut board = self.board.lock().expect("mock poisoned");
        if let Some(item) = board.iter_mut().find(|item| item.id == id) {
            item.comment_count += 1;
        }
        Ok(stored)
    }
}

/// The real HTTP TinyHumans client, compiled only under the `tinyhumans` feature.
#[cfg(feature = "tinyhumans")]
pub use http::HttpTinyHumansClient;

#[cfg(feature = "tinyhumans")]
mod http {
    use super::{IngestOutcome, IngestRequest, PRODUCT, TinyHumansClient};
    use crate::Result;
    use crate::error::OpenCompanyError;
    use crate::feedback::board::{
        BoardComment, BoardDetail, BoardItem, BoardKind, BoardPage, BoardQuery, BoardStatus,
        VoteValue,
    };
    use crate::ports::types::SecretValue;
    use async_trait::async_trait;

    /// A [`TinyHumansClient`] backed by `POST {api_url}/feedback/ingest`.
    ///
    /// The credential authenticates the call; the backend resolves it to the
    /// owning account, which is what makes a forwarded report "recorded on
    /// behalf of the key owner".
    pub struct HttpTinyHumansClient {
        sdk: tinyhumans_sdk::TinyHumansClient,
    }

    impl HttpTinyHumansClient {
        /// Builds a client posting to `api_url` as `credential`'s owner.
        pub fn new(api_url: impl Into<String>, credential: SecretValue) -> Self {
            // This client bypasses the embedded openhuman_core entirely, so
            // unlike the harness's `IntegrationClient`-backed calls it must tag
            // itself with our product identity directly — see `crate::product`.
            let (name, value) = crate::product::product_identity_header();
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(name, reqwest::header::HeaderValue::from_static(value));
            Self {
                // Trailing slashes would produce `//feedback/ingest`.
                sdk: tinyhumans_sdk::TinyHumansClient::new(api_url.into().trim_end_matches('/'))
                    // The credential rides the header and only the header.
                    .with_token(Some(credential.expose().to_string()))
                    .with_default_headers(headers),
            }
        }

        /// Maps an SDK failure onto the crate error with the hub's own message.
        fn err(error: tinyhumans_sdk::Error) -> OpenCompanyError {
            match error {
                tinyhumans_sdk::Error::Status { status, body } => OpenCompanyError::TinyHumans {
                    code: format!("http_{status}"),
                    message: wire_error(&body).unwrap_or_else(|| format!("HTTP {status}")),
                },
                tinyhumans_sdk::Error::Http(e) => OpenCompanyError::TinyHumans {
                    code: "unreachable".to_string(),
                    message: e.to_string(),
                },
                other => OpenCompanyError::TinyHumans {
                    code: "decode".to_string(),
                    message: other.to_string(),
                },
            }
        }
    }

    /// Reads a hub board item out of its camelCase JSON.
    ///
    /// Tolerant on purpose: a field the hub adds, renames or omits must not turn
    /// a whole page of the board into an error page in the console. Only `id` is
    /// structurally required, because without it no row can be voted on.
    fn parse_item(value: &serde_json::Value) -> Result<BoardItem> {
        let text = |key: &str| value.get(key).and_then(|v| v.as_str());
        let count = |key: &str| value.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let id = text("id")
            .ok_or_else(|| decode_err("board item without an id"))?
            .to_string();
        let upvotes = count("upvoteCount");
        let downvotes = count("downvoteCount");
        Ok(BoardItem {
            id,
            kind: text("type")
                .and_then(BoardKind::parse)
                .unwrap_or(BoardKind::Feature),
            title: text("title").unwrap_or_default().to_string(),
            body: text("body").unwrap_or_default().to_string(),
            status: text("status")
                .and_then(BoardStatus::parse)
                .unwrap_or(BoardStatus::Open),
            author: text("createdByName").map(str::to_string),
            upvotes,
            downvotes,
            score: value
                .get("score")
                .and_then(|v| v.as_i64())
                .unwrap_or(i64::from(upvotes) - i64::from(downvotes)),
            comment_count: count("commentCount"),
            my_vote: value
                .get("myVote")
                .and_then(|v| v.as_i64())
                .and_then(|v| i8::try_from(v).ok())
                .and_then(|v| VoteValue::try_from(v).ok())
                .unwrap_or(VoteValue::None),
            issue_url: value
                .get("github")
                .and_then(|g| g.get("issueUrl"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            created_at: text("createdAt").unwrap_or_default().to_string(),
        })
    }

    /// Reads a hub comment out of its camelCase JSON.
    fn parse_comment(value: &serde_json::Value) -> BoardComment {
        let text = |key: &str| value.get(key).and_then(|v| v.as_str());
        BoardComment {
            id: text("id").unwrap_or_default().to_string(),
            author: text("userName").map(str::to_string),
            body: text("body").unwrap_or_default().to_string(),
            created_at: text("createdAt").unwrap_or_default().to_string(),
        }
    }

    /// The comments array of a detail payload, skipping anything unreadable.
    fn parse_comments(value: &serde_json::Value) -> Vec<BoardComment> {
        value
            .get("comments")
            .and_then(|v| v.as_array())
            .map(|items| items.iter().map(parse_comment).collect())
            .unwrap_or_default()
    }

    /// The same error shape [`HttpTinyHumansClient::err`] builds, for the free
    /// parse functions above.
    fn decode_err(message: impl std::fmt::Display) -> OpenCompanyError {
        OpenCompanyError::TinyHumans {
            code: "decode".to_string(),
            message: message.to_string(),
        }
    }

    #[async_trait]
    impl TinyHumansClient for HttpTinyHumansClient {
        async fn ingest(&self, request: &IngestRequest) -> Result<IngestOutcome> {
            use tinyhumans_sdk::api::types::{FeedbackType, IngestFeedbackRequest};
            let body = IngestFeedbackRequest {
                kind: match request.wire_type() {
                    "bug" => FeedbackType::Bug,
                    _ => FeedbackType::Feature,
                },
                title: request.title.clone(),
                body: request.body.clone(),
                product: PRODUCT.to_string(),
                origin: Some(request.origin.clone()),
                external_ref: Some(request.external_ref.clone()),
            };
            let data = match self.sdk.feedback().ingest_feedback(&body).await {
                Ok(data) => data.0,
                // The daily-limit refusal is a normal outcome for a busy
                // operator, not a transport failure: report it rather than
                // erroring.
                Err(tinyhumans_sdk::Error::Status { status: 429, body }) => {
                    return Ok(IngestOutcome::RateLimited {
                        reason: wire_error(&body)
                            .unwrap_or_else(|| "daily feedback limit reached".to_string()),
                    });
                }
                Err(error) => return Err(Self::err(error)),
            };
            // { accepted, reason, feedback } — a 200 with `accepted: false` is
            // a moderation rejection, not an error.
            let accepted = data
                .get("accepted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !accepted {
                return Ok(IngestOutcome::Rejected {
                    reason: data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("rejected by moderation")
                        .to_string(),
                });
            }
            Ok(IngestOutcome::Accepted {
                remote_id: data
                    .get("feedback")
                    .and_then(|f| f.get("id"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            })
        }

        async fn list_board(&self, query: BoardQuery) -> Result<BoardPage> {
            let query = query.clamped();
            let params: Vec<tinyhumans_sdk::QueryParam> = vec![
                ("sort", Some(query.sort.as_str().to_string())),
                ("page", Some(query.page.to_string())),
                ("limit", Some(query.limit.to_string())),
                ("type", query.kind.map(|kind| kind.as_str().to_string())),
                (
                    "status",
                    query.status.map(|status| status.as_str().to_string()),
                ),
            ];
            let data = self
                .sdk
                .feedback()
                .list_feedback(&params)
                .await
                .map_err(Self::err)?
                .0;
            let items = data
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| decode_err("board page without items"))?
                .iter()
                .map(parse_item)
                .collect::<Result<Vec<_>>>()?;
            let number = |key: &str, fallback: u32| {
                data.get(key)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(u64::from(fallback)) as u32
            };
            Ok(BoardPage {
                total: number("total", items.len() as u32),
                page: number("page", query.page),
                limit: number("limit", query.limit),
                items,
            })
        }

        async fn board_item(&self, id: &str) -> Result<BoardDetail> {
            let data = self
                .sdk
                .feedback()
                .get_feedback(id)
                .await
                .map_err(Self::err)?
                .0;
            let item = data
                .get("feedback")
                .ok_or_else(|| decode_err("detail without a feedback item"))?;
            Ok(BoardDetail {
                item: parse_item(item)?,
                comments: parse_comments(&data),
            })
        }

        async fn vote_board_item(&self, id: &str, value: VoteValue) -> Result<BoardItem> {
            let vote = tinyhumans_sdk::api::types::FeedbackVoteRequest {
                value: value.as_i8(),
            };
            let data = self
                .sdk
                .feedback()
                .vote_feedback(id, &vote)
                .await
                .map_err(Self::err)?;
            parse_item(&data.0)
        }

        async fn comment_board_item(&self, id: &str, body: &str) -> Result<BoardComment> {
            let comment = tinyhumans_sdk::api::types::FeedbackCommentRequest {
                body: body.to_string(),
            };
            let data = self
                .sdk
                .feedback()
                .comment_feedback(id, &comment)
                .await
                .map_err(Self::err)?;
            Ok(parse_comment(&data.0))
        }
    }

    /// The `error` string from a failure envelope, when present.
    fn wire_error(value: &serde_json::Value) -> Option<String> {
        value
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
}

#[cfg(test)]
#[path = "tinyhumans_tests.rs"]
mod tests;

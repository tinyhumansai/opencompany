# Live Operations

## Outcome

The Operator sees company work as it happens and can respond to approvals,
questions, failures, and completed work without polling or losing continuity.

## Why this matters

The event log already supports subscriptions, while the HTTP and console
surfaces remain mostly request/response. Long-running work needs a live control
plane: the Operator should know what is active, what is blocked, and what needs
attention across every company and browser session.

## Proposed capability

- Stream chat output incrementally with cancellation and reconnect support.
- Provide a sequenced company event feed using SSE as the baseline transport.
- Resume from the last acknowledged sequence after network loss.
- Normalize task, workflow, approval, inbox, teammate, commerce, lifecycle,
  usage, and failure events into one product-facing feed.
- Separate durable feed items from transient progress signals.
- Maintain unread state and per-category notification preferences.
- Deliver optional browser, email, or webhook notifications for actionable
  events without leaking sensitive payloads.
- Aggregate platform-level events while preserving tenant authorization.
- Offer filters for attention required, active work, failures, and history.
- Define retention, pagination, and backfill behavior.

## Acceptance boundary

- Every durable event has a monotonic company-local sequence.
- Reconnection with `since` does not lose or duplicate durable feed items.
- Authorization is reevaluated when a stream connects and when credentials
  expire.
- Slow clients cannot block runtime work or grow memory without bounds.
- Notification delivery is idempotent and records success or failure.
- Secret material and raw private context never enter notification previews.
- The request/response API remains usable as a compatibility path.

## Likely implementation seams

- `EventLog::subscribe` implementations in `src/store/`
- `/api/v1/companies/{id}/events` and single-company equivalents
- streaming chat alongside the current operator chat route
- event projection and redaction in `src/server/`
- unread/notification preference storage ports
- a console feed client with cursor persistence and reconnect backoff

## Open questions

- Whether transient token/progress frames share SSE or use a separate channel.
- Which event payloads are retained verbatim versus projected on read.
- Whether platform aggregation uses one multiplexed stream or one per company.
- Minimum delivery guarantees for email and webhook notifications.

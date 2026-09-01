//! Dynamic ledgers: the company's own record, as data rather than as code.
//!
//! A company keeps more than a task board. It keeps goals, decisions, risks,
//! customer promises, experiments run, invoices chased — and every one of those
//! is the same shape: **a set of rows with an id, a status, some prose, and a
//! reason each closed one closed**. Before this, exactly one of those shapes
//! was first-class ([`TaskStore`](crate::ports::TaskStore), with six hard-coded
//! columns) and every other axis a company needed was written into a note, a
//! chat message or a workspace file nobody designed.
//!
//! So the shape is declared rather than compiled. A [`LedgerSpec`] names its
//! fields, its statuses, which of them close a row, and how the rendered file
//! is laid out; the [`engine`] folds an append-only log into entries and
//! renders both a built-in and a company-declared ledger identically. The
//! runtime ships three ([`registry`]) and a company writes the rest.
//!
//! # The four rules that keep it honest
//!
//! **Append-only.** There is one write operation and it merges. Opening,
//! amending, blocking, closing and re-prioritising a row are all *record an
//! event against this id*. A merge has no inverse to get wrong, and the log
//! keeps the history either way — which is what the rewritten-whole board lost
//! silently on every write.
//!
//! **Agents may create and record; only people may delete.**
//! [`AuthorKind::may_delete`](types::AuthorKind::may_delete) is that rule, in
//! one place, and it is not a permission setting. An agent's relationship with
//! a ledger is entirely additive, so everything it does is recoverable by
//! reading the log; deletion is not, and a runtime where a turn can erase the
//! record of what it did is one whose record means nothing. Closing a row is
//! the agent-shaped way of being finished with it, and it keeps the reason.
//!
//! **Everything renders into `derived/`, and nothing hand-writes it.** The
//! folder name is the invariant. The workspace write guard refuses an edit to a
//! path under it and names the tool that actually writes the row — because an
//! edit that lands and is then erased by the next derivation is worse than one
//! that is refused, since the author believes it took.
//!
//! **Bounds are code, not intent.** Every section is clamped on the way in
//! ([`budget`]), so a declaration cannot grow its own file past what a reader —
//! or a turn carrying it — is asked to hold.

pub mod analytics;
pub mod board;
pub mod budget;
pub mod derived;
pub mod engine;
pub mod native;
pub mod registry;
pub mod spec;
pub mod types;

pub use board::{BoardColumn, COLUMNS as BOARD_COLUMN_TABLE};
pub use engine::{Entries, Entry, fold, index, ordered, render};
pub use registry::{MAX_DECLARED, Registry, builtins};
pub use spec::{
    Check, DERIVED_DIR, Field, FieldRole, LedgerSource, LedgerSpec, MAX_FIELD_CHARS, ORDERS, Order,
    REASON_FIELD, Section, StatusSpec, normalize_slug, parse, parse_order,
};
pub use types::{AuthorKind, LedgerAuthor, LedgerEvent};

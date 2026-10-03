//! Pure finances projection: the ledger + `[budget]` → the console's
//! [`Finances`] shape.
//!
//! No I/O and no ledger writes — the ledger stays the single financial source of
//! truth; metering only reads it. The `graphql/finances.rs` resolver and its
//! REST twin load `CompanyRecord.ledger` and the manifest's `[budget]`, then
//! call [`finances_from`].
//!
//! Sign convention (set by the cost hook): outflows are negative `amount_usd`,
//! inflows positive.

use std::collections::HashMap;

use crate::company::Budget;
use crate::ports::types::LedgerEntry;

use super::calendar::{epoch_day, iso_day, month_start_millis};
use super::types::{CategorySpend, Direction, Finances, Transaction};

/// Projects the ledger into the [`Finances`] read surface.
///
/// - `ledger`: the company's append-only ledger (any order; sorted here).
/// - `budget`: the manifest's `[budget]` (`monthly_usd` is the cap).
/// - `now_millis`: "now", used to find the current-month boundary (UTC).
pub fn finances_from(ledger: &[LedgerEntry], budget: &Budget, now_millis: u64) -> Finances {
    let month_start = month_start_millis(now_millis);

    let mut spent_usd = 0.0;
    let mut revenue_usd = 0.0;
    let mut by_category_map: HashMap<String, f64> = HashMap::new();

    for entry in ledger {
        if entry.at_millis < month_start {
            continue;
        }
        if entry.amount_usd < 0.0 {
            let magnitude = -entry.amount_usd;
            spent_usd += magnitude;
            *by_category_map
                .entry(category_label(&entry.kind))
                .or_default() += magnitude;
        } else if entry.amount_usd > 0.0 {
            revenue_usd += entry.amount_usd;
        }
    }

    let mut by_category: Vec<CategorySpend> = by_category_map
        .into_iter()
        .map(|(category, amount)| CategorySpend { category, amount })
        .collect();
    by_category.sort_by(|a, b| {
        b.amount
            .partial_cmp(&a.amount)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.category.cmp(&b.category))
    });

    // Bookkeeping net across all time: inflows (+) minus outflows (−).
    let bookkeeping_net: f64 = ledger.iter().map(|e| e.amount_usd).sum();
    let balance_usd = bookkeeping_net;

    // Monetary entries only, newest first. The id keeps the entry's append
    // position (not the sorted order) so paging stays deterministic. Ties on
    // `at_millis` keep append order via the stable sort.
    let mut indexed: Vec<(usize, &LedgerEntry)> = ledger
        .iter()
        .enumerate()
        .filter(|(_, e)| e.amount_usd != 0.0)
        .collect();
    indexed.sort_by_key(|(_, e)| std::cmp::Reverse(e.at_millis));
    let transactions: Vec<Transaction> = indexed
        .into_iter()
        .map(|(i, e)| Transaction {
            id: format!("tx-{i}"),
            date: iso_day(epoch_day(e.at_millis)),
            description: e.memo.clone(),
            category: category_label(&e.kind),
            amount_usd: e.amount_usd.abs(),
            direction: if e.amount_usd < 0.0 {
                Direction::Out
            } else {
                Direction::In
            },
        })
        .collect();

    Finances {
        balance_usd,
        budget_usd: budget.monthly_usd,
        spent_usd,
        revenue_usd,
        net_usd: revenue_usd - spent_usd,
        by_category,
        transactions,
    }
}

/// Maps a dotted [`LedgerEntry::kind`] to its prosumer category label.
///
/// The prefix (segment before the first `.`) selects the label; unknown
/// prefixes are Title-cased so a new kind still renders sensibly.
pub fn category_label(kind: &str) -> String {
    let prefix = kind.split('.').next().unwrap_or(kind);
    match prefix {
        "inference" => "Inference".to_string(),
        "tools" => "Tools".to_string(),
        // `x402` is the retired tiny.place payment kind. Ledgers are
        // append-only, so rows written before its removal still carry it and
        // must keep rendering as the payments they were.
        "payment" | "x402" => "Payments".to_string(),
        "registry" => "Registry".to_string(),
        "filing" => "Filings".to_string(),
        other => title_case(other),
    }
}

/// Upper-cases the first character of a lowercase slug.
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
#[path = "finances_tests.rs"]
mod tests;

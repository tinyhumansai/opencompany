//! Pure finances projection: the ledger + `[budget]` + (optional) economy
//! wallet balance → the console's [`Finances`] shape.
//!
//! No I/O and no ledger writes — the ledger stays the single financial source of
//! truth (see `docs/spec/company-as-agent/commerce.md`); metering only reads it.
//! WS2's `graphql/finances.rs` resolver loads `CompanyRecord.ledger`, the
//! manifest's `[budget]`, and — under the `tinyplace` feature — the economy
//! wallet balance, then calls [`finances_from`].
//!
//! Sign convention (set by the economy adapter and cost hook): outflows are
//! negative `amount_usd`, inflows positive.

use std::collections::HashMap;

use crate::company::Budget;
use crate::ports::types::LedgerEntry;

use super::calendar::{epoch_day, iso_day, month_start_millis};
use super::types::{CategorySpend, Direction, Finances, Transaction};

/// Projects the ledger into the [`Finances`] read surface.
///
/// - `ledger`: the company's append-only ledger (any order; sorted here).
/// - `budget`: the manifest's `[budget]` (`monthly_usd` is the cap).
/// - `economy_balance`: the tiny.place wallet balance when the `tinyplace`
///   feature journals one; `None` falls back to the bookkeeping net.
/// - `now_millis`: "now", used to find the current-month boundary (UTC).
pub fn finances_from(
    ledger: &[LedgerEntry],
    budget: &Budget,
    economy_balance: Option<f64>,
    now_millis: u64,
) -> Finances {
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
    let balance_usd = economy_balance.unwrap_or(bookkeeping_net);

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
        budget_usd: budget.monthly_usd.unwrap_or(0.0),
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
mod tests {
    use super::*;
    use crate::metering::calendar::{MILLIS_PER_DAY, days_from_civil};

    fn at(y: i64, m: u32, d: u32) -> u64 {
        (days_from_civil(y, m, d) as u64) * MILLIS_PER_DAY + 12 * 3_600_000
    }

    fn entry(at_millis: u64, kind: &str, amount: f64, memo: &str) -> LedgerEntry {
        LedgerEntry {
            at_millis,
            kind: kind.to_string(),
            amount_usd: amount,
            memo: memo.to_string(),
        }
    }

    fn budget(cap: Option<f64>) -> Budget {
        Budget { monthly_usd: cap }
    }

    #[test]
    fn empty_ledger_is_all_zero() {
        let f = finances_from(&[], &budget(None), None, at(2026, 7, 16));
        assert_eq!(f.spent_usd, 0.0);
        assert_eq!(f.revenue_usd, 0.0);
        assert_eq!(f.net_usd, 0.0);
        assert_eq!(f.balance_usd, 0.0);
        assert_eq!(f.budget_usd, 0.0);
        assert!(f.by_category.is_empty());
        assert!(f.transactions.is_empty());
    }

    #[test]
    fn current_month_spend_and_revenue() {
        let now = at(2026, 7, 16);
        let ledger = vec![
            entry(at(2026, 7, 10), "inference.spend", -12.0, "ceo"),
            entry(at(2026, 7, 12), "x402.out", -8.0, "paid quote"),
            entry(at(2026, 7, 14), "x402.in", 30.0, "a2a sale"),
            // Last month: excluded from spent/revenue, still counts to balance.
            entry(at(2026, 6, 30), "inference.spend", -100.0, "old"),
        ];
        let f = finances_from(&ledger, &budget(Some(2000.0)), None, now);
        assert!((f.spent_usd - 20.0).abs() < 1e-9);
        assert!((f.revenue_usd - 30.0).abs() < 1e-9);
        assert!((f.net_usd - 10.0).abs() < 1e-9);
        assert_eq!(f.budget_usd, 2000.0);
        // Bookkeeping net across all time: 30 - 12 - 8 - 100 = -90.
        assert!((f.balance_usd - (-90.0)).abs() < 1e-9);
    }

    #[test]
    fn economy_balance_overrides_bookkeeping() {
        let now = at(2026, 7, 16);
        let ledger = vec![entry(at(2026, 7, 10), "inference.spend", -12.0, "ceo")];
        let f = finances_from(&ledger, &budget(None), Some(8420.55), now);
        assert!((f.balance_usd - 8420.55).abs() < 1e-9);
    }

    #[test]
    fn by_category_groups_current_month_spend_only() {
        let now = at(2026, 7, 16);
        let ledger = vec![
            entry(at(2026, 7, 10), "inference.spend", -12.0, "a"),
            entry(at(2026, 7, 11), "inference.spend", -8.0, "b"),
            entry(at(2026, 7, 12), "tools.github", -5.0, "c"),
            entry(at(2026, 7, 13), "x402.out", -3.0, "d"),
            entry(
                at(2026, 7, 14),
                "x402.in",
                50.0,
                "revenue not a spend category",
            ),
        ];
        let f = finances_from(&ledger, &budget(None), None, now);
        assert_eq!(f.by_category.len(), 3);
        // Highest first: Inference 20, Tools 5, Payments 3.
        assert_eq!(f.by_category[0].category, "Inference");
        assert!((f.by_category[0].amount - 20.0).abs() < 1e-9);
        assert_eq!(f.by_category[1].category, "Tools");
        assert_eq!(f.by_category[2].category, "Payments");
        // by_category sums to spent_usd.
        let sum: f64 = f.by_category.iter().map(|c| c.amount).sum();
        assert!((sum - f.spent_usd).abs() < 1e-9);
    }

    #[test]
    fn transactions_are_newest_first_and_directional() {
        let now = at(2026, 7, 16);
        let ledger = vec![
            entry(at(2026, 7, 10), "inference.spend", -12.0, "older"),
            entry(at(2026, 7, 14), "x402.in", 30.0, "newer revenue"),
            // Zero-amount entries (e.g. filings) are excluded from the money list.
            entry(at(2026, 7, 15), "filing.submit", 0.0, "a filing"),
        ];
        let f = finances_from(&ledger, &budget(None), None, now);
        assert_eq!(f.transactions.len(), 2);
        assert_eq!(f.transactions[0].description, "newer revenue");
        assert_eq!(f.transactions[0].direction, Direction::In);
        assert_eq!(f.transactions[0].amount_usd, 30.0);
        assert_eq!(f.transactions[0].category, "Payments");
        assert_eq!(f.transactions[1].description, "older");
        assert_eq!(f.transactions[1].direction, Direction::Out);
        assert_eq!(f.transactions[1].amount_usd, 12.0);
        // Ids are stable to the append position, not the sorted order.
        assert_eq!(f.transactions[0].id, "tx-1");
        assert_eq!(f.transactions[1].id, "tx-0");
    }

    #[test]
    fn category_label_maps_prefixes() {
        assert_eq!(category_label("inference.spend"), "Inference");
        assert_eq!(category_label("tools.github"), "Tools");
        assert_eq!(category_label("payment.send"), "Payments");
        assert_eq!(category_label("x402.out"), "Payments");
        assert_eq!(category_label("registry.fee"), "Registry");
        assert_eq!(category_label("filing.submit"), "Filings");
        // Unknown prefix is Title-cased.
        assert_eq!(category_label("subscription.figma"), "Subscription");
        assert_eq!(category_label("bare"), "Bare");
    }
}

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
    let f = finances_from(&[], &budget(None), at(2026, 7, 16));
    assert_eq!(f.spent_usd, 0.0);
    assert_eq!(f.revenue_usd, 0.0);
    assert_eq!(f.net_usd, 0.0);
    assert_eq!(f.balance_usd, 0.0);
    assert_eq!(f.budget_usd, None);
    assert!(f.by_category.is_empty());
    assert!(f.transactions.is_empty());
}

#[test]
fn zero_cap_is_distinct_from_no_cap() {
    let now = at(2026, 7, 16);
    // A manifest that sets `monthly_usd = 0` is a hard cap, not an absent
    // budget: it survives as `Some(0.0)` so the console can say "capped at
    // zero" rather than "no budget is set".
    let capped = finances_from(&[], &budget(Some(0.0)), now);
    assert_eq!(capped.budget_usd, Some(0.0));
    let uncapped = finances_from(&[], &budget(None), now);
    assert_eq!(uncapped.budget_usd, None);
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
    let f = finances_from(&ledger, &budget(Some(2000.0)), now);
    assert!((f.spent_usd - 20.0).abs() < 1e-9);
    assert!((f.revenue_usd - 30.0).abs() < 1e-9);
    assert!((f.net_usd - 10.0).abs() < 1e-9);
    assert_eq!(f.budget_usd, Some(2000.0));
    // Bookkeeping net across all time: 30 - 12 - 8 - 100 = -90.
    assert!((f.balance_usd - (-90.0)).abs() < 1e-9);
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
    let f = finances_from(&ledger, &budget(None), now);
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
    let f = finances_from(&ledger, &budget(None), now);
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

//! Tests for the referral read side: the attribution heads a question and its
//! answer carry, and the reserved authors.

use super::*;

#[test]
fn answer_rows_attribute_and_unattribute_symmetrically() {
    let note = returned_note("writer", "Content desk", "  Use the short form.  ");
    assert_eq!(
        note,
        "@writer on #Content desk answered: Use the short form."
    );
    assert_eq!(
        unattributed("writer", "Content desk", &note),
        "Use the short form."
    );
    assert_eq!(unattributed("writer", "Content desk", "plain"), "plain");
    assert_eq!(
        asked_message("@engineer on #Engineering desk asks: draft the copy"),
        "draft the copy"
    );
    assert_eq!(asked_message("no head here"), "no head here");
    assert_eq!(pair_conversation("zed", "amy"), "dm:amy+zed");
    assert!(is_hive_author(HIVE_REFERRAL_AUTHOR));
    assert!(is_legacy_report_author("hive-report"));
    assert!(!is_hive_author("writer"));
}

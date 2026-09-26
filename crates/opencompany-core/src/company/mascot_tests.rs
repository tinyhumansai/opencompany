//! Unit tests for `mascot.rs`.

use super::*;

#[test]
fn valid_mode_parses() {
    assert_eq!(parse_mode("static").unwrap(), "static");
    assert_eq!(parse_mode(" animated ").unwrap(), "animated");
}

#[test]
fn invalid_mode_names_the_accepted_set() {
    let err = parse_mode("paused").unwrap_err();
    let message = err.to_string();
    assert!(message.contains("static"));
    assert!(message.contains("animated"));
}

#[test]
fn every_costume_parses() {
    for costume in MASCOT_COSTUMES {
        assert_eq!(parse_costume(costume).unwrap(), costume);
    }
}

#[test]
fn unknown_costume_is_refused() {
    assert!(parse_costume("face_mask").is_err());
    assert!(parse_costume("").is_err());
}

#[test]
fn costume_numbers_pair_one_to_one_with_costumes() {
    assert_eq!(MASCOT_COSTUMES.len(), MASCOT_COSTUME_NUMBERS.len());
    // Every number is distinct — two costumes must never collide on one
    // `mascotAnimationNumber`, or choosing one would silently render the
    // other.
    let mut numbers = MASCOT_COSTUME_NUMBERS.to_vec();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers.len(), MASCOT_COSTUME_NUMBERS.len());
}

#[test]
fn default_costume_is_in_the_closed_list() {
    assert!(MASCOT_COSTUMES.contains(&DEFAULT_MASCOT_COSTUME));
}

#[test]
fn every_skin_color_parses_and_has_a_hex() {
    assert_eq!(MASCOT_SKIN_COLORS.len(), MASCOT_SKIN_COLOR_HEXES.len());
    for color in MASCOT_SKIN_COLORS {
        assert_eq!(parse_skin_color(color).unwrap(), color);
    }
    for hex in MASCOT_SKIN_COLOR_HEXES {
        assert_eq!(hex.len(), 6);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}

#[test]
fn every_hand_color_parses_and_has_a_hex() {
    assert_eq!(MASCOT_HAND_COLORS.len(), MASCOT_HAND_COLOR_HEXES.len());
    for color in MASCOT_HAND_COLORS {
        assert_eq!(parse_hand_color(color).unwrap(), color);
    }
    for hex in MASCOT_HAND_COLOR_HEXES {
        assert_eq!(hex.len(), 6);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}

#[test]
fn unknown_colors_are_refused() {
    assert!(parse_skin_color("chartreuse").is_err());
    assert!(parse_hand_color("chartreuse").is_err());
}

#[test]
fn default_swatches_are_named_entries() {
    assert!(MASCOT_SKIN_COLORS.contains(&"default"));
    assert!(MASCOT_HAND_COLORS.contains(&"default"));
}

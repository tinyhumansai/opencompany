//! Avatar-reference parsing and sniffing tests: which forms an avatar
//! reference may take and which MIME types are accepted (split out of
//! `avatar_tests.rs`).

use super::*;

#[test]
fn accepts_every_shipped_flavour() {
    for flavour in TINY_FLAVOURS {
        let stored = normalize(&format!("tiny:{flavour}")).expect("a shipped flavour");
        assert_eq!(parse(&stored).unwrap(), AvatarRef::Tiny(flavour));
    }
}

#[test]
fn refuses_a_flavour_with_no_file() {
    // The whole point of validating: "puce" would render as a broken image
    // on every surface that draws a face, not just the one that set it.
    let err = parse("tiny:puce").unwrap_err().to_string();
    assert!(err.contains("puce"), "{err}");
    assert!(
        err.contains("amber"),
        "the refusal must list what to pick: {err}"
    );
}

#[test]
fn accepts_a_node_reference() {
    assert_eq!(
        parse("blob:01J8Z5Q9YQ0000000000000000").unwrap(),
        AvatarRef::Blob("01J8Z5Q9YQ0000000000000000")
    );
}

#[test]
fn accepts_every_shipped_mascot_kind() {
    for kind in MASCOT_KINDS {
        let stored = normalize(&format!("mascot:{kind}")).expect("a shipped mascot kind");
        assert_eq!(parse(&stored).unwrap(), AvatarRef::Mascot(kind));
    }
}

#[test]
fn refuses_a_mascot_kind_with_no_file() {
    // Same rule as `refuses_a_flavour_with_no_file`: an unshipped kind would
    // render as a broken canvas on every surface that draws a face.
    let err = parse("mascot:bogus").unwrap_err().to_string();
    assert!(err.contains("bogus"), "{err}");
    assert!(
        err.contains("animated"),
        "the refusal must list what to pick: {err}"
    );
}

/// The security rule this module exists for: a URL is not an avatar. Each of
/// these is rendered into an `src=` on every surface that draws a face, so a
/// stored one is an instruction the console obeys for whoever wrote it.
#[test]
fn refuses_anything_that_is_not_one_of_the_three_forms() {
    for hostile in [
        "https://tracker.example/beacon.gif",
        "javascript:alert(1)",
        "data:image/gif;base64,R0lGOD",
        "/avatars/blob-amber.webp",
        "blob:../../etc/passwd",
        "blob:one two",
        "blob:",
        "",
        "amber",
    ] {
        let err = parse(hostile).unwrap_err().to_string();
        assert!(
            err.contains("A URL can't be stored as an avatar.") || err.contains("isn't one of"),
            "{hostile} was accepted or refused unhelpfully: {err}"
        );
    }
}

#[test]
fn refuses_an_unbounded_string() {
    assert!(parse(&format!("tiny:{}", "a".repeat(MAX_LEN))).is_err());
    assert!(parse(&format!("mascot:{}", "a".repeat(MAX_LEN))).is_err());
}

#[test]
fn trims_on_the_way_in() {
    assert_eq!(normalize("  tiny:teal \n").unwrap(), "tiny:teal");
    assert_eq!(
        normalize("  mascot:animated \n").unwrap(),
        "mascot:animated"
    );
}

#[test]
fn sniffs_the_four_accepted_formats() {
    assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
    assert_eq!(sniff_image(b"\xff\xd8\xff\xe0rest"), Some("image/jpeg"));
    assert_eq!(sniff_image(b"GIF89a...."), Some("image/gif"));
    assert_eq!(sniff_image(b"GIF87a...."), Some("image/gif"));
    assert_eq!(
        sniff_image(b"RIFF\x20\x00\x00\x00WEBPVP8 "),
        Some("image/webp")
    );
}

/// The point of sniffing rather than trusting the declared type: each of
/// these arrives labelled `image/png` by anyone who wants it to be.
#[test]
fn sniffing_refuses_what_only_claims_to_be_an_image() {
    for bytes in [
        &b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script/></svg>"[..],
        &b"<!doctype html><script>fetch('/')</script>"[..],
        &b"%PDF-1.7"[..],
        // A RIFF container that is not WebP — the near-miss the second half
        // of the WebP check exists for.
        &b"RIFF\x20\x00\x00\x00WAVEfmt "[..],
        &b""[..],
        &b"RIFF"[..],
    ] {
        assert_eq!(
            sniff_image(bytes),
            None,
            "{:?}",
            &bytes[..bytes.len().min(16)]
        );
    }
}

/// GIF is accepted deliberately (a moving face is more recognisable, not
/// less); SVG is refused deliberately (a document that can carry script).
#[test]
fn image_types() {
    for ok in [
        "image/png",
        "image/jpeg",
        "image/webp",
        "image/gif",
        "IMAGE/GIF",
    ] {
        assert!(is_supported_image(ok), "{ok}");
    }
    for no in ["image/svg+xml", "text/html", "application/pdf", ""] {
        assert!(!is_supported_image(no), "{no}");
    }
}

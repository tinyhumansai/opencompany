//! Who is sitting at this machine, as the operating system already knows.
//!
//! The console asks for this **once**, to prefill a profile nobody has filled in
//! yet: a person signing in for the first time should not have to type a name
//! their computer has known since it was set up, or hunt for a picture that is
//! already on it.
//!
//! ## It is a suggestion, and stays one
//!
//! Nothing here is stored, sent anywhere, or applied on its own. The console
//! offers what it finds, and a person accepts it, edits it, or ignores it — at
//! which point what gets saved is a decision rather than a guess. That is the
//! whole reason this is a read and not an import: a name lifted off a machine
//! and written into a company directory unasked is somebody's laptop's idea of
//! who they are, published to their colleagues.
//!
//! ## What each platform actually knows
//!
//! There is no portable "current user's full name and picture", so each platform
//! is asked in its own terms, and every field is optional because on any given
//! machine it may genuinely not be set:
//!
//! * **Linux** — the full name is the GECOS field of `/etc/passwd` (its first
//!   comma-separated part, which is the name; the rest is office/phone). The
//!   picture is `~/.face` where the desktop environments put it, else
//!   AccountsService's copy.
//! * **macOS** — `dscl` reads the local directory: `RealName` for the name,
//!   `JPEGPhoto` for the account picture.
//! * **Windows** — the account picture lives under
//!   `%PUBLIC%\AccountPictures`, in the current account's SID folder; the full
//!   name is not read (the APIs that hold it are not reachable without a
//!   dependency this crate does not want).
//!
//! Everything is best-effort: any failure is `None`, never an error. A profile
//! prefill that fails is a profile a person fills in themselves, which is
//! exactly what happens today.

#[cfg(any(target_os = "linux", target_os = "windows", test))]
use std::path::Path;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::path::PathBuf;

/// The largest account picture worth carrying into the webview.
///
/// The host's own avatar ceiling is 4 MB, and this is read into memory and then
/// base64'd into an IPC payload — so a machine with a 30 MB portrait as its
/// account picture should be reported as having none rather than have the
/// console try, and fail, to upload it.
const MAX_PICTURE_BYTES: u64 = 4 * 1024 * 1024;

/// What this machine knows about the person using it. Every field optional.
#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    /// The account's login name — `enamakel`. Always available in practice.
    pub username: Option<String>,
    /// The account's full name — "Steven Enamakel" — where the OS holds one.
    pub full_name: Option<String>,
    /// The account picture as a `data:` URL, where the OS holds one.
    ///
    /// A data URL rather than a path because the webview cannot read the
    /// filesystem, and rather than raw bytes because the console turns it
    /// straight into a `File` to upload. It is **not** an avatar reference and
    /// is never stored as one: the console uploads it through
    /// `POST …/avatars` like any other image, so what ends up on the record
    /// names bytes this host holds. See `docs/spec/runtime/avatars.md`.
    pub picture_data_url: Option<String>,
}

/// Reads what this machine knows. Never fails; an unknown field is `None`.
pub fn device_identity() -> DeviceIdentity {
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty());
    DeviceIdentity {
        full_name: username.as_deref().and_then(full_name),
        picture_data_url: picture(username.as_deref()),
        username,
    }
}

/// The account's full name, per platform. `None` where the OS has none set —
/// which is common on Linux and is not a failure.
#[cfg(target_os = "macos")]
fn full_name(username: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/dscl")
        .args([".", "-read", &format!("/Users/{username}"), "RealName"])
        .output()
        .ok()?;
    // `dscl` answers either `RealName: Steven Enamakel` or `RealName:` followed
    // by an indented line — the second when the value contains a space, which
    // is to say for almost every real name.
    let text = String::from_utf8_lossy(&out.stdout);
    let value = text
        .strip_prefix("RealName:")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or_else(|| text.lines().nth(1).map(str::trim))?;
    non_empty(value)
}

#[cfg(target_os = "linux")]
fn full_name(username: &str) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    let line = passwd
        .lines()
        .find(|line| line.split(':').next() == Some(username))?;
    // GECOS is field 5, and is itself comma-separated: name, office, work
    // phone, home phone. Only the first part is a name.
    let gecos = line.split(':').nth(4)?;
    non_empty(gecos.split(',').next().unwrap_or_default())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn full_name(_username: &str) -> Option<String> {
    None
}

/// The account picture as a data URL, per platform.
///
/// Two shapes, because the platforms differ in kind and not only in path: Linux
/// and Windows keep a *file*, which is read and sniffed; macOS keeps the bytes
/// **inside the directory record**, so there is nothing to open and they are
/// extracted from `dscl` instead.
#[cfg(target_os = "linux")]
fn picture(username: Option<&str>) -> Option<String> {
    // `~/.face` is what GNOME, KDE and friends write; AccountsService keeps its
    // own copy, which is the one that survives a home directory the desktop
    // never wrote to.
    let home = std::env::var("HOME").ok()?;
    let mut candidates = vec![
        PathBuf::from(&home).join(".face"),
        PathBuf::from(&home).join(".face.icon"),
    ];
    if let Some(username) = username {
        candidates.push(PathBuf::from("/var/lib/AccountsService/icons").join(username));
    }
    // The first *usable* candidate wins, not the first that exists: `~/.face`
    // can be a stale or corrupt first-choice file — unreadable, over the
    // ceiling, or not one of the accepted images — and the fallbacks after it
    // are only ever tried if encoding the earlier ones actually succeeds.
    candidates
        .into_iter()
        .filter_map(|path| encode_picture(&path))
        .next()
}

/// macOS keeps the picture in the local directory record rather than in a file,
/// so it is extracted rather than opened: `dscl` prints `JPEGPhoto` as
/// whitespace-separated hex, which is decoded back to bytes here.
#[cfg(target_os = "macos")]
fn picture(username: Option<&str>) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/dscl")
        .args([".", "-read", &format!("/Users/{}", username?), "JPEGPhoto"])
        .output()
        .ok()?;
    decode_jpegphoto(&String::from_utf8_lossy(&out.stdout))
}

/// Decodes a `dscl` `JPEGPhoto` record into a `data:` URL.
///
/// The payload is the hex after the `JPEGPhoto:` label, and the label itself
/// contains a hex digit — `E` in `JPEGPhoto` — so filtering the whole response
/// would prepend that digit to an otherwise even-length payload: the parity
/// check below would then refuse the picture every time, or worse corrupt the
/// first byte of the decoded image when the count happens to come out even.
/// Only the value after the label is treated as hex.
///
/// The payload may wrap: `dscl` answers `JPEGPhoto:` alone on the first line
/// and indented hex on the lines that follow when the value is long, the same
/// shape the `RealName` reader above handles.
#[cfg(any(target_os = "macos", test))]
fn decode_jpegphoto(output: &str) -> Option<String> {
    let hex: String = output
        .strip_prefix("JPEGPhoto:")
        .unwrap_or(output)
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.len() < 2 || !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect();
    if bytes.len() as u64 > MAX_PICTURE_BYTES {
        return None;
    }
    let mime = sniff(&bytes)?;
    Some(format!("data:{mime};base64,{}", base64_encode(&bytes)))
}

#[cfg(target_os = "windows")]
fn picture(username: Option<&str>) -> Option<String> {
    let _ = username;
    let public = std::env::var("PUBLIC").ok()?;
    // The directory holds one folder per account SID with several sizes in
    // it. The current account's SID is resolved rather than every SID being
    // scanned, because the largest file across the whole directory on a shared
    // machine is another local user's picture — offered here as this person's.
    let dir = PathBuf::from(public)
        .join("AccountPictures")
        .join(current_sid()?);
    // Sizes sit either directly in the SID folder or one level deeper in a
    // GUID-named subfolder (the layout Windows itself writes for the standard
    // account images), so two levels of walk reach both.
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in walk(&dir, 2) {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.len() > MAX_PICTURE_BYTES {
            continue;
        }
        if best.as_ref().is_none_or(|(size, _)| meta.len() > *size) {
            best = Some((meta.len(), entry.path()));
        }
    }
    best.and_then(|(_, path)| encode_picture(&path))
}

/// The current account's SID, for scoping the picture lookup to this person.
///
/// `whoami /user` answers it. The output is localized and its column layout
/// varies, so the SID is found by shape rather than position — the one token
/// shaped like `S-1-5-21-…`. A username can never match that shape, so the
/// hunt cannot grab the wrong column. `None` means "no SID on this machine",
/// and the caller offers no picture suggestion at all.
#[cfg(target_os = "windows")]
fn current_sid() -> Option<String> {
    let out = std::process::Command::new("whoami")
        .arg("/user")
        .output()
        .ok()?;
    parse_whoami_user(&String::from_utf8_lossy(&out.stdout))
}

/// Extracts the SID token from `whoami /user` output.
///
/// The shape check is deliberately stricter than a bare `S-` prefix: a real
/// SID always has a second dash (`S-1-5-…`), so `S-123` or `S-admin` cannot
/// match. Split into its own function so the parse is testable on any host.
#[cfg(any(target_os = "windows", test))]
fn parse_whoami_user(text: &str) -> Option<String> {
    text.lines()
        .flat_map(str::split_whitespace)
        .find(|token| {
            token.starts_with("S-")
                && token.len() > 2
                && token[2..].chars().all(|c| c.is_ascii_digit() || c == '-')
                && token[2..].contains('-')
        })
        .map(str::to_string)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn picture(_username: Option<&str>) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
fn walk(dir: &Path, depth: usize) -> Vec<std::fs::DirEntry> {
    if depth == 0 {
        return Vec::new();
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        if entry.path().is_dir() {
            out.extend(walk(&entry.path(), depth - 1));
        } else {
            out.push(entry);
        }
    }
    out
}

/// Reads a picture file into a `data:` URL, or `None` if it is missing,
/// unreadable, over the ceiling, or not an image this host would accept anyway.
#[cfg(any(target_os = "linux", target_os = "windows", test))]
fn encode_picture(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_PICTURE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    // Sniffed, not taken from the extension: `~/.face` conventionally has no
    // extension at all, and the type has to be right or the console builds a
    // `File` the host will refuse. The four signatures are the same set
    // `src/company/avatar.rs` accepts.
    let mime = sniff(&bytes)?;
    Some(format!("data:{mime};base64,{}", base64_encode(&bytes)))
}

/// The four image types the host accepts, by signature.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Standard base64, hand-rolled to keep a dependency out of the desktop shell
/// for one call site.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // The bytes that expose an alphabet typo — the last two entries and the
        // high bits — which ASCII test vectors never reach.
        assert_eq!(base64_encode(&[0xff, 0xef, 0xbe]), "/+++");
    }

    /// The signature check is what stops `~/.face` — which conventionally has no
    /// extension — being reported under a type the host will refuse.
    #[test]
    fn only_the_four_accepted_formats_are_recognised() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(b"\xff\xd8\xffrest"), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\x20\x00\x00\x00WEBP"), Some("image/webp"));
        assert_eq!(sniff(b"<svg><script/></svg>"), None);
        assert_eq!(sniff(b"RIFF\x20\x00\x00\x00WAVE"), None);
        assert_eq!(sniff(b""), None);
    }

    /// A picture that is missing, oversized or not an image is `None` rather
    /// than an error: a prefill that cannot happen is a form somebody fills in.
    #[test]
    fn an_unreadable_picture_is_simply_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(encode_picture(&dir.path().join("nothing")), None);
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, b"not an image").unwrap();
        assert_eq!(encode_picture(&text), None);
    }

    /// The happy path, end to end: bytes on disk become a data URL the console
    /// can turn into a `File`.
    #[test]
    fn a_png_becomes_a_data_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".face");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nbody").unwrap();
        let url = encode_picture(&path).expect("a data url");
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
    }

    /// Reading identity must never panic or block, whatever this machine is.
    #[test]
    fn reading_this_machine_answers_something() {
        let _ = device_identity();
    }

    /// The `JPEGPhoto:` label contains a hex digit — `E` — so the payload must
    /// be the hex *after* the label. Filtering the whole response prepends that
    /// digit to an even-length payload, making the count odd and the picture
    /// absent on every macOS machine. This is the regression for that bug.
    #[test]
    fn jpegphoto_decodes_the_hex_after_the_label() {
        let payload = b"\xff\xd8\xff\xe0"; // a JPEG signature, four bytes
        let hex: String = payload.iter().map(|b| format!("{b:02X}")).collect();
        let out = format!("JPEGPhoto: {hex}");
        let url = decode_jpegphoto(&out).expect("a data url");
        assert_eq!(
            url,
            format!("data:image/jpeg;base64,{}", base64_encode(payload)),
            "{out}"
        );
    }

    /// Long values make `dscl` put the label on its own line and wrap the hex
    /// beneath it — the same shape the `RealName` reader handles.
    #[test]
    fn jpegphoto_handles_a_wrapped_payload() {
        let out = "JPEGPhoto:\n  FFD8\n  FFE0\n";
        let url = decode_jpegphoto(out).expect("a data url");
        assert_eq!(url, "data:image/jpeg;base64,/9j/4A==");
    }

    /// A payload whose bytes are not one of the accepted images is absent, like
    /// every other picture source on this file.
    #[test]
    fn jpegphoto_that_is_not_an_image_is_absent() {
        assert_eq!(decode_jpegphoto("JPEGPhoto: 4141"), None);
        assert_eq!(decode_jpegphoto("JPEGPhoto:"), None);
    }

    /// `whoami /user` is localized and its column layout varies; the SID is
    /// found by shape — a `S-1-5-21-…` digit token — never by column or
    /// position. This is the parse behind scoping the Windows picture lookup
    /// to the current account rather than every SID on the machine.
    #[test]
    fn whoami_user_sid_is_found_by_shape() {
        assert_eq!(
            parse_whoami_user(
                "USER INFORMATION\n\
                 ----------------\n\
                 \n\
                 User Name        SID\n\
                 ================ =================================\n\
                 workstation\\alice S-1-5-21-1004336348-1177238915-682003330-1001\n",
            ),
            Some("S-1-5-21-1004336348-1177238915-682003330-1001".to_string())
        );
        // Nothing SID-shaped means no suggestion — the correct answer when the
        // account cannot be identified.
        assert_eq!(parse_whoami_user("workstation\\alice"), None);
        assert_eq!(parse_whoami_user(""), None);
        // `S-` alone, or `S-123` with no second dash, is not a SID.
        assert_eq!(parse_whoami_user("S-"), None);
        assert_eq!(parse_whoami_user("S-123"), None);
    }
}

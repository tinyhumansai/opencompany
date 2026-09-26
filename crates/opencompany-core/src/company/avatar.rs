//! Avatar references: which face a teammate or a person wears.
//!
//! Every teammate and every person already *has* a face — the console hashes
//! their stable id into one of the mascots shipped in `frontend/public/avatars/`
//! and draws that, which is why a company with nobody's avatar set still reads
//! as a roster of individuals rather than a column of grey squares. This module
//! is the other half: what is stored when somebody **chooses** a face instead.
//!
//! ## The grammar
//!
//! An avatar reference is one short string in exactly one of three forms:
//!
//! | Form | Means |
//! |---|---|
//! | `tiny:<flavour>` | one of the [shipped mascots](TINY_FLAVOURS) — a flavour of tiny |
//! | `blob:<nodeId>` | a custom image the operator uploaded, held as a binary workspace node |
//! | `mascot:<kind>` | one of the [shipped animated mascots](MASCOT_KINDS) — a curated Rive character, never uploaded |
//!
//! Absent (`None`) is a fourth state and the default: *nobody has chosen*, so the
//! console keeps hashing. It is deliberately distinct from every stored form,
//! because "reset to the default face" has to be expressible and none of the
//! stored forms nor an empty string can express it.
//!
//! ## Why the grammar is closed
//!
//! The obvious shape is to store a URL and be done. That is exactly what this
//! refuses, and the reason is that the string ends up in an `src=` attribute on
//! every console surface that draws a face — chat gutters, facepiles, the org
//! chart, the members pane. A stored URL is therefore an instruction the console
//! obeys on behalf of whoever wrote it: `javascript:` is script injection,
//! `http://tracker.example/x.gif` is a beacon that fires for every viewer and
//! reports who looked at the roster and when, and either survives in the record
//! long after the person who set it lost their account.
//!
//! Every stored form names something *this host already holds*, so rendering one
//! reaches nothing the viewer's session did not already reach. `mascot:` is
//! curated for the same reason `blob:` is validated against its referent rather
//! than trusted by shape: a `.riv` file is a programmable, document-like format
//! with its own runtime, not a raster image `sniff_image` can validate by
//! signature and dimensions — the same class of thing SVG is refused for below.
//! Accepting one as an arbitrary upload would reopen exactly that risk, so a
//! mascot kind is a closed enum naming a file shipped with the console, the same
//! posture as `tiny:`, never something a member's bytes could become.
//!
//! ## Animation
//!
//! GIFs are first-class: an avatar is a small square that a person picked to be
//! recognisable, and a moving one is more recognisable, not less. Nothing here
//! transcodes, so an animated GIF or WebP is stored and served as the bytes that
//! were uploaded and animates wherever the console draws it. See
//! [`is_supported_image`] for the accepted types, and the upload route for the
//! size ceiling.

use futures::StreamExt;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin};

/// The leading bytes that announce each accepted format.
///
/// Hoisted so `sniff_image` and [`image_dimensions`] read the same signatures
/// and cannot drift apart — one checks *that* the bytes name a format, the
/// other *what size* the format the bytes name claims.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG_SIGNATURE: &[u8] = b"\xff\xd8\xff";
const GIF_SIGNATURE_87: &[u8] = b"GIF87a";
const GIF_SIGNATURE_89: &[u8] = b"GIF89a";

/// The mascots shipped with the console, one file per colourway.
///
/// **Must stay in step with `frontend/public/avatars/blob-<flavour>.webp` and
/// with `TINY_FLAVOURS` in `frontend/src/lib/avatar.ts`.** A flavour accepted
/// here that has no file renders as a broken image on every surface at once,
/// which is why the host validates the name rather than storing whatever it is
/// handed.
pub const TINY_FLAVOURS: [&str; 11] = [
    "amber", "blue", "clay", "cloud", "ember", "graphite", "green", "indigo", "rose", "teal",
    "violet",
];

/// The animated mascots shipped with the console, one Rive file per kind.
///
/// **Must stay in step with `frontend/public/avatars/mascot-<kind>.riv` and
/// with `MASCOT_KINDS` in `frontend/src/lib/avatar.ts`.** A list of one on
/// purpose even though v1 ships a single kind (`"animated"`): the same closed,
/// validated-membership shape `TINY_FLAVOURS` uses, so a second colorway or a
/// second character later is an addition to this list, not a grammar change.
pub const MASCOT_KINDS: [&str; 1] = ["animated"];

/// The longest an avatar reference may be.
///
/// Both forms are a short prefix plus an identifier the host itself minted, so
/// this is far above anything legitimate; it exists so an unbounded string
/// cannot be pushed into a record through a field nobody thought to bound.
const MAX_LEN: usize = 128;

/// The workspace folder every avatar's bytes live under.
///
/// The upload route mints faces here, and [`resolve`] copies a validated
/// `blob:` referent that lives anywhere else into here, so the node a stored
/// reference names is always one this host created for the purpose — never a
/// published/generated binary that a later republish could rewrite underneath
/// the face. One folder also lets an operator see — and delete — what the
/// company holds without hunting through the tree.
pub const AVATARS_FOLDER: &str = "avatars";

/// A parsed avatar reference — where the face actually comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvatarRef<'a> {
    /// One of the [shipped mascots](TINY_FLAVOURS), by flavour name.
    Tiny(&'a str),
    /// A custom image, by the id of the binary workspace node holding its bytes.
    Blob(&'a str),
    /// One of the [shipped animated mascots](MASCOT_KINDS), by kind name.
    Mascot(&'a str),
}

/// The image types an uploaded avatar may be.
///
/// GIF is on the list on purpose — see the module docs. SVG is **not**: an SVG
/// is a document that can carry script and fetch remote resources, so accepting
/// one would reintroduce, inside a file, precisely what refusing arbitrary URLs
/// keeps out.
pub fn is_supported_image(mime: &str) -> bool {
    matches!(
        mime.trim().to_ascii_lowercase().as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    )
}

/// The largest an uploaded avatar may be.
///
/// Four mebibytes is generous for a square somebody will see at 32px and mean
/// for a phone photo, which is the trade being made: it has to fit an animated
/// GIF with enough frames to be worth animating, and it must not let the roster
/// become a place to park a video. The console shrinks a still image before it
/// uploads; an animated one cannot be shrunk without transcoding it, so this
/// ceiling is what an animation is actually held to.
pub const MAX_AVATAR_BYTES: usize = 4 * 1024 * 1024;

/// The largest edge an avatar image may have, in pixels.
///
/// Four thousand is far above anything a face is drawn at — the console shows
/// these at tens of pixels — and still small enough that no decoder's per-image
/// allocation is worth a second thought. Together with [`MAX_AVATAR_PIXELS`]
/// this is what turns a decompression bomb (a header claiming 65535×65535 in a
/// body of a few hundred compressed bytes) into a `400` on the upload instead
/// of a gigabyte allocation for every operator who views the roster.
pub const MAX_AVATAR_DIMENSION: u32 = 4096;

/// The largest total area an avatar image may decode to, in pixels.
///
/// A square with both edges at [`MAX_AVATAR_DIMENSION`] is the largest shape
/// the two caps accept; this is what refuses an extreme aspect ratio whose
/// edges each happen to fit. `4096²` is 16 megapixels — an RGBA buffer of
/// 64 MiB, the worst case any surface ever decodes for one face.
pub const MAX_AVATAR_PIXELS: u64 = 4096 * 4096;

/// The largest decoded area an animated avatar may repaint in one full cycle.
///
/// [`MAX_AVATAR_PIXELS`] bounds a single still; an animation multiplies the
/// cost of viewing by its frame count, so the thing that matters is the *sum*
/// of every frame's decoded pixels. Eight full-screen `4096²` frames would be
/// a pathological avatar, so the ceiling sits at eight of them — while a
/// genuinely moving face (a few hundred frames at 128×128) stays two orders of
/// magnitude below it. This is what turns "a valid 4096×4096 animation with
/// hundreds of full-canvas frames under the 4 MiB byte ceiling" into a `400`,
/// so every operator who views the roster is never asked to repeatedly decode
/// billions of pixels.
pub const MAX_AVATAR_ANIMATED_PIXELS: u64 = 8 * MAX_AVATAR_PIXELS;

/// The media type these bytes actually are, read from their signature.
///
/// **Not** the type the upload declared. A declared type is a claim by whoever
/// is uploading, and an avatar is stored once and then served back to every
/// member of the company for as long as the teammate exists — so the type it is
/// served under has to be a fact about the bytes rather than a claim about them.
/// The four accepted formats all begin with an unambiguous signature, so this
/// costs a dozen bytes to answer honestly.
///
/// `None` means "not one of the four", which the upload route refuses. In
/// particular an SVG, an HTML document and a PDF all land here as `None`
/// whatever they were labelled as.
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(PNG_SIGNATURE) {
        return Some("image/png");
    }
    if bytes.starts_with(JPEG_SIGNATURE) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(GIF_SIGNATURE_87) || bytes.starts_with(GIF_SIGNATURE_89) {
        return Some("image/gif");
    }
    // RIFF....WEBP — the four size bytes in between are part of the container,
    // so both ends of the signature have to be checked for this to mean WebP
    // rather than "some RIFF file", of which .wav is the commonest.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// The decoded dimensions of a supported image, read from its own header.
///
/// [`sniff_image`] answers "do the leading bytes *name* one of the four
/// formats"; this answers "and what size does the format those bytes name
/// claim". The gap between the two is the decompression bomb: a header can
/// promise 65535×65535 in a payload small enough to pass the avatar ceiling,
/// and a decoder that trusts the promise hands the allocation to whoever
/// views it. Reading the announced size before the bytes are stored turns that
/// from a per-viewer allocation into a `400` on the upload.
///
/// A header parse, not a decode — decoding to learn the size would be the
/// allocation the check exists to prevent.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(PNG_SIGNATURE) {
        // Signature (8) + IHDR length (4) + "IHDR" (4), then big-endian width
        // and height at a fixed offset.
        let w = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
        let h = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
        return Some((w, h));
    }
    if bytes.starts_with(GIF_SIGNATURE_87) || bytes.starts_with(GIF_SIGNATURE_89) {
        // Logical screen width and height, little-endian, right after the
        // signature.
        let w = u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?) as u32;
        let h = u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?) as u32;
        return Some((w, h));
    }
    if bytes.starts_with(JPEG_SIGNATURE) {
        return jpeg_dimensions(bytes);
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return webp_dimensions(bytes);
    }
    None
}

/// The total decoded area a GIF announces across all of its frames.
///
/// The logical screen size alone understates what a decoder does: an animation
/// repaints some rectangle per frame, so the cost of one full cycle is the sum
/// of every Image Descriptor's area. This walks the block stream — skipping
/// extension blocks and color tables, never decompressing the LZW raster data —
/// which is a header parse in the same spirit as [`image_dimensions`]: it
/// answers "how much painting does a viewer do" without doing any of it.
///
/// Returns `Ok(None)` when the bytes are not a GIF, or a GIF that never reaches
/// an Image Descriptor (nothing to count). Returns `Err` when a GIF that has
/// reached at least one Image Descriptor ends without a clean trailer: a viewer
/// decodes the frames that are present, so a truncated animation repaints its
/// counted area just the same and must be held to
/// [`MAX_AVATAR_ANIMATED_PIXELS`] — treating it as a still would let the frame
/// flood through the cap.
fn gif_animation_cost(bytes: &[u8]) -> Result<Option<u64>> {
    if !(bytes.starts_with(GIF_SIGNATURE_87) || bytes.starts_with(GIF_SIGNATURE_89)) {
        return Ok(None);
    }
    // Signature (6) + Logical Screen Descriptor (7). The packed flags at
    // offset 10 carry the global color table size in the low three bits —
    // but only when the table is present, which bit 7 says.
    let Some(&flags) = bytes.get(10) else {
        // Shorter than the descriptor: no frame can have been reached, so
        // there is nothing animated to count.
        return Ok(None);
    };
    let mut i = 13;
    if flags & 0x80 != 0 {
        let table_entries = 1 << ((flags & 0x07) + 1);
        i += 3 * table_entries;
    }
    let mut cost: u64 = 0;
    let mut saw_descriptor = false;
    loop {
        // The stream ends before the trailer. A GIF that already reached a
        // frame is a truncated animation — a viewer repaints the frames that
        // are there — and must be refused rather than read as a still. A GIF
        // with no frame at all is a header-only file: a still, nothing to count.
        let Some(&kind) = bytes.get(i) else {
            return if saw_descriptor {
                Err(truncated_animation())
            } else {
                Ok(None)
            };
        };
        i += 1;
        match kind {
            // Trailer — the stream is over, and we walked every frame in it.
            0x3B => break,
            // Extension: one label byte, then the same sub-block structure as
            // the raster data. Its payload never contributes pixels.
            0x21 => {
                i += 1;
                let Some(next) = skip_sub_blocks(bytes, i) else {
                    return if saw_descriptor {
                        Err(truncated_animation())
                    } else {
                        Ok(None)
                    };
                };
                i = next;
            }
            // Image Descriptor: left/top (4) + width/height (4) + packed flags (1).
            0x2C => {
                saw_descriptor = true;
                // A frame's own bytes cut off mid-descriptor are a truncated
                // animation too — the descriptor this frame *is* was reached.
                let Some(frame_w) = bytes.get(i + 4..i + 6).and_then(|b| b.try_into().ok()) else {
                    return Err(truncated_animation());
                };
                let frame_w = u16::from_le_bytes(frame_w) as u32;
                let Some(frame_h) = bytes.get(i + 6..i + 8).and_then(|b| b.try_into().ok()) else {
                    return Err(truncated_animation());
                };
                let frame_h = u16::from_le_bytes(frame_h) as u32;
                cost = cost.saturating_add((frame_w as u64) * (frame_h as u64));
                let Some(&packed) = bytes.get(i + 8) else {
                    return Err(truncated_animation());
                };
                i += 9;
                // A local color table follows the descriptor when its flag is
                // set; the table size is again the low three bits.
                if packed & 0x80 != 0 {
                    let local_entries = 1 << ((packed & 0x07) + 1);
                    i += 3 * local_entries;
                }
                // LZW minimum code size byte, then the raster sub-blocks.
                i += 1;
                let Some(next) = skip_sub_blocks(bytes, i) else {
                    return Err(truncated_animation());
                };
                i = next;
            }
            // Any other block kind is malformed; stop rather than misread it.
            // Once a frame is on the table the browser has already decoded it,
            // so this is a truncated animation too, not a still.
            _ => {
                return if saw_descriptor {
                    Err(truncated_animation())
                } else {
                    Ok(None)
                };
            }
        }
    }
    Ok(saw_descriptor.then_some(cost))
}

/// Advances `i` past a run of sub-blocks — each a one-byte length followed by
/// that many bytes — ending at the zero-length terminator.
fn skip_sub_blocks(bytes: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let n = *bytes.get(i)? as usize;
        i += 1;
        if n == 0 {
            return Some(i);
        }
        i = i.checked_add(n)?;
        if i > bytes.len() {
            return None;
        }
    }
}

/// The common validation error for an animation stream that ends before its
/// trailer or remaining chunk data.
fn truncated_animation() -> OpenCompanyError {
    OpenCompanyError::InvalidRequest(
        "that image is a truncated animation, so it can't be an avatar.".to_string(),
    )
}

/// The height and width a JPEG announces, from its SOF segment.
///
/// The SOF marker carries the frame size, and it may sit after any number of
/// APPn/COM/DQT/DHT/DRI segments — there is no fixed offset — so the marker
/// list has to be walked. Each segment is `FF` + marker + 2-byte length; the
/// length covers itself and the payload but not the marker.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // Start after the SOI (`FF D8`), which `sniff_image` has already required.
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        // `FF FF` is a fill byte; the second FF begins the real marker.
        if bytes[i + 1] == 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // Standalone markers carry no length field: SOI, EOI, RSTn, TEM.
        if marker == 0xD8 || marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if len < 2 {
            return None;
        }
        // A SOF segment is precision(1) + height(2) + width(2), then the
        // per-component bytes. The SOF markers are C0–CF except the ones that
        // are not SOF: DHT (C4), JPG (C8), DAC (CC), DNL (DC), DRI (DD).
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC | 0xDC | 0xDD) {
            // `len` covers the length field and the payload but not the marker,
            // so the segment ends at `i + 2 + len`. A real SOF always carries
            // at least one component, which makes the payload `precision(1) +
            // height(2) + width(2) + 3*components` — `len >= 10`. Anything
            // shorter cannot hold the size bytes the fixed indexes below read;
            // the segment-end check alone would let a declared length of two
            // through and then index past the buffer.
            if len < 10 || i + 2 + len > bytes.len() {
                return None;
            }
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// The width and height a WebP announces, from its first image chunk.
///
/// After the 12-byte RIFF/WEBP container, chunks are FourCC (4) + size (4) +
/// payload, padded to an even byte count. The canvas size lives in the first
/// image chunk — VP8X when the file is extended (alpha, animation), else VP8
/// (lossy) or VP8L (lossless).
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let fourcc = bytes.get(i..i + 4)?;
        let size = u32::from_le_bytes(bytes.get(i + 4..i + 8)?.try_into().ok()?) as usize;
        let data = bytes.get(i + 8..i + 8 + size)?;
        match fourcc {
            b"VP8 " => {
                // Lossy: frame tag (3) + start code (3), then width and height
                // each as their own little-endian 16-bit word — 14 bits of
                // dimension with the 2 scale bits on top (RFC 6386 §9.1). The
                // two are *not* packed into one shared field, so height lives
                // wholly in `data[8..10]` rather than continuing through
                // `data[7]`.
                if data.len() < 10 {
                    return None;
                }
                let w = (data[6] as u32) | (((data[7] & 0x3F) as u32) << 8);
                let h = (data[8] as u32) | (((data[9] & 0x3F) as u32) << 8);
                return Some((w, h));
            }
            b"VP8L" => {
                // Lossless: `0x2F` signature, then packed as (little-endian):
                //
                //   bits  0–13: (width−1), 14 bits
                //   bits 14–27: (height−1), 14 bits
                //   bit     28: alpha_is_used (1 bit, non-normative hint)
                //   bits 29–31: version (3 bits, must be 0)
                //
                // Only the 14 height bits belong to height; alpha and version
                // must be masked out (RFC 9649 §4, WebP Lossless Bitstream).
                if data.len() < 5 || data[0] != 0x2F {
                    return None;
                }
                let w = 1 + ((data[1] as u32) | (((data[2] & 0x3F) as u32) << 8));
                let h = 1
                    + ((((data[2] & 0xC0) as u32) >> 6)
                        | ((data[3] as u32) << 2)
                        | (((data[4] & 0x0F) as u32) << 10));
                return Some((w, h));
            }
            b"VP8X" => {
                // Extended: flags (1) + reserved (3) + 24-bit (width−1) and
                // (height−1).
                if data.len() < 10 {
                    return None;
                }
                let w = 1 + ((data[4] as u32) | ((data[5] as u32) << 8) | ((data[6] as u32) << 16));
                let h = 1 + ((data[7] as u32) | ((data[8] as u32) << 8) | ((data[9] as u32) << 16));
                return Some((w, h));
            }
            // ALPH, ANIM, ANMF, ICCP, EXIF, XMP and anything unknown are
            // skipped — the first image chunk has already been read by the
            // time a file reaches them.
            _ => {}
        }
        i += 8 + size + (size & 1);
    }
    None
}

/// The total decoded area an animated WebP repaints in one full cycle.
///
/// The canvas size alone understates what a decoder does for the same reason
/// the GIF walker above exists: an animation repaints some rectangle per frame,
/// so the cost of one cycle is the sum of every frame's rectangle. An animated
/// WebP carries one ANMF chunk per frame, and each announces its own
/// `(width−1, height−1)` in 24-bit little-endian fields at a fixed offset in
/// the chunk payload — so this walks RIFF chunks, never touching the encoded
/// frame bytes, in the same spirit as [`image_dimensions`].
///
/// Returns `Ok(None)` when the bytes are not a WebP, or a WebP with no ANMF
/// chunks (nothing to count). Returns `Err` when an animated WebP's chunk
/// stream is cut off: a viewer decodes the ANMF frames that are present, so a
/// truncated animation must be held to [`MAX_AVATAR_ANIMATED_PIXELS`] just like
/// a complete one.
fn webp_animation_cost(bytes: &[u8]) -> Result<Option<u64>> {
    if !(bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP") {
        return Ok(None);
    }
    let mut cost: u64 = 0;
    let mut saw_frame = false;
    let mut i = 12;
    while i + 8 <= bytes.len() {
        // A chunk header cut off mid-file. Once a frame is on the table the
        // browser has already decoded it, so truncation here is a truncated
        // animation; before any frame there is nothing animated to count.
        let Some(fourcc) = bytes.get(i..i + 4) else {
            return if saw_frame {
                Err(truncated_animation())
            } else {
                Ok(None)
            };
        };
        let Some(size) = bytes.get(i + 4..i + 8).and_then(|b| b.try_into().ok()) else {
            return if saw_frame {
                Err(truncated_animation())
            } else {
                Ok(None)
            };
        };
        let size = u32::from_le_bytes(size) as usize;
        let Some(data) = bytes.get(i + 8..i + 8 + size) else {
            // The chunk's declared payload is not all present. A truncated
            // ANMF is itself proof of animation — the fourcc names a frame —
            // so it is refused even as the first frame.
            return if saw_frame || fourcc == b"ANMF" {
                Err(truncated_animation())
            } else {
                Ok(None)
            };
        };
        if fourcc == b"ANMF" {
            // A frame whose payload opens with X(3) + Y(3), then the 24-bit
            // `width−1` and `height−1` at 6..9 and 9..12. A shorter ANMF cannot
            // name its rectangle; refuse rather than misread it.
            if data.len() < 12 {
                return Err(truncated_animation());
            }
            saw_frame = true;
            let frame_w =
                1 + ((data[6] as u32) | ((data[7] as u32) << 8) | ((data[8] as u32) << 16));
            let frame_h =
                1 + ((data[9] as u32) | ((data[10] as u32) << 8) | ((data[11] as u32) << 16));
            cost = cost.saturating_add((frame_w as u64) * (frame_h as u64));
        }
        i += 8 + size + (size & 1);
    }
    if saw_frame && i != bytes.len() {
        return Err(truncated_animation());
    }
    Ok(saw_frame.then_some(cost))
}

/// The total decoded area an APNG repaints in one full cycle.
///
/// An APNG is an ordinary PNG carrying two extra chunks: an `acTL` announces
/// the frame count, and an `fcTL` precedes each frame carrying its rectangle in
/// big-endian width and height. The default image is frame 0 of the cycle and
/// covers the canvas, so it is paid for like the first GIF descriptor; every
/// `fcTL` adds its own rectangle. The walk reads chunk headers only — the same
/// promise as [`image_dimensions`], never a decode.
///
/// Returns `Ok(None)` when the bytes are not a PNG, or a PNG with no `acTL` (a
/// still image — nothing animated to count). Returns `Err` when an APNG's chunk
/// stream is cut off after its animation chunks begin: a viewer decodes the
/// frames that are present, so a truncated animation must be held to
/// [`MAX_AVATAR_ANIMATED_PIXELS`] just like a complete one.
fn apng_animation_cost(bytes: &[u8]) -> Result<Option<u64>> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Ok(None);
    }
    // The canvas the default image covers — the same bytes `image_dimensions`
    // reads, re-read here so the cost is computed in one place.
    let Some(w) = bytes.get(16..20).and_then(|b| b.try_into().ok()) else {
        // Shorter than an IHDR: no animation chunk can have been reached.
        return Ok(None);
    };
    let w = u32::from_be_bytes(w) as u64;
    let Some(h) = bytes.get(20..24).and_then(|b| b.try_into().ok()) else {
        return Ok(None);
    };
    let h = u32::from_be_bytes(h) as u64;
    let mut cost: u64 = 0;
    let mut animated = false;
    let mut i = 8;
    while i + 8 <= bytes.len() {
        let Some(len) = bytes.get(i..i + 4).and_then(|b| b.try_into().ok()) else {
            return if animated {
                Err(truncated_animation())
            } else {
                Ok(None)
            };
        };
        let len = u32::from_be_bytes(len) as usize;
        let Some(chunk_type) = bytes.get(i + 4..i + 8) else {
            return if animated {
                Err(truncated_animation())
            } else {
                Ok(None)
            };
        };
        let Some(data) = bytes.get(i + 8..i + 8 + len) else {
            // The chunk's declared payload is not all present. A truncated
            // acTL or fcTL is itself proof of animation — the chunk type names
            // it — so it is refused even before `animated` has been set.
            return if animated || chunk_type == b"acTL" || chunk_type == b"fcTL" {
                Err(truncated_animation())
            } else {
                Ok(None)
            };
        };
        if chunk_type == b"acTL" {
            animated = true;
            cost = cost.saturating_add(w * h);
        } else if chunk_type == b"fcTL" {
            // Sequence(4), then big-endian width and height at 4..12.
            let Some(frame_w) = data.get(4..8).and_then(|b| b.try_into().ok()) else {
                return Err(truncated_animation());
            };
            let frame_w = u32::from_be_bytes(frame_w) as u64;
            let Some(frame_h) = data.get(8..12).and_then(|b| b.try_into().ok()) else {
                return Err(truncated_animation());
            };
            let frame_h = u32::from_be_bytes(frame_h) as u64;
            cost = cost.saturating_add(frame_w * frame_h);
        }
        // A PNG chunk is length(4) + type(4) + data + CRC(4); the CRC is not
        // counted in `len`, so the next chunk starts 12 bytes past this header.
        i += 12 + len;
    }
    // A trailing partial chunk header or payload is malformed. Once animation
    // has started, silently ignoring it would turn a truncated animation into
    // a still result and bypass the frame-cost check.
    if animated && i != bytes.len() {
        return Err(truncated_animation());
    }
    Ok(animated.then_some(cost))
}

/// Refuses an image whose decoded size is a decompression bomb.
///
/// [`sniff_image`] proves the leading bytes *name* one of the four formats;
/// this proves the image they name is not pathological. An avatar is rendered
/// at a handful of pixels, so an edge over [`MAX_AVATAR_DIMENSION`] or an area
/// over [`MAX_AVATAR_PIXELS`] has nothing legitimate behind it — it is a
/// header that would make every decoder that touches it allocate a buffer
/// nobody needs. A payload too short to announce a size is refused as not an
/// image: a truncated avatar would not decode anywhere either.
///
/// GIF, animated WebP and APNG are deliberately allowed to move, so their
/// single-frame size does not bound what a viewer decodes — every frame is
/// repainted each cycle. Each format's walker counts how much painting that
/// is, and a cycle over [`MAX_AVATAR_ANIMATED_PIXELS`] is a bomb too.
pub fn check_image_dimensions(bytes: &[u8]) -> Result<()> {
    let Some((w, h)) = image_dimensions(bytes) else {
        return Err(not_an_image());
    };
    if w > MAX_AVATAR_DIMENSION
        || h > MAX_AVATAR_DIMENSION
        || (w as u64) * (h as u64) > MAX_AVATAR_PIXELS
    {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "that image is {w}×{h} pixels — an avatar has to fit within \
             {MAX_AVATAR_DIMENSION}×{MAX_AVATAR_DIMENSION}."
        )));
    }
    // The three formats browsers can animate each carry a per-frame table the
    // walkers above read; a still has nothing to count and is bounded by the
    // single-frame check already done. Each walker answers `Ok(None)` for a
    // format it is not, or for a file of its format that never reaches a frame,
    // and `Err` for a truncated animation — a file whose frames a viewer would
    // decode but whose walk could not be completed, which must not be read as a
    // still and sneaked past the per-cycle cap.
    let animated_cost =
        if bytes.starts_with(GIF_SIGNATURE_87) || bytes.starts_with(GIF_SIGNATURE_89) {
            gif_animation_cost(bytes)?
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            webp_animation_cost(bytes)?
        } else if bytes.starts_with(PNG_SIGNATURE) {
            apng_animation_cost(bytes)?
        } else {
            None
        };
    if let Some(cost) = animated_cost
        && cost > MAX_AVATAR_ANIMATED_PIXELS
    {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "that image animates {cost} pixels per cycle — an avatar's \
             animation may total at most {MAX_AVATAR_ANIMATED_PIXELS}."
        )));
    }
    Ok(())
}

/// Parses a stored or submitted avatar reference.
///
/// Returns [`OpenCompanyError::InvalidRequest`] naming all three accepted
/// forms, because the commonest way to get this wrong is to send a URL and
/// the error has to say what to send instead.
pub fn parse(value: &str) -> Result<AvatarRef<'_>> {
    let value = value.trim();
    if value.len() > MAX_LEN {
        return Err(refusal());
    }
    if let Some(flavour) = value.strip_prefix("tiny:") {
        return if TINY_FLAVOURS.contains(&flavour) {
            Ok(AvatarRef::Tiny(flavour))
        } else {
            Err(OpenCompanyError::InvalidRequest(format!(
                "\"{flavour}\" isn't one of the tiny avatars. Pick one of: {}.",
                TINY_FLAVOURS.join(", ")
            )))
        };
    }
    if let Some(node) = value.strip_prefix("blob:") {
        return if is_node_id(node) {
            Ok(AvatarRef::Blob(node))
        } else {
            Err(refusal())
        };
    }
    if let Some(kind) = value.strip_prefix("mascot:") {
        return if MASCOT_KINDS.contains(&kind) {
            Ok(AvatarRef::Mascot(kind))
        } else {
            Err(OpenCompanyError::InvalidRequest(format!(
                "\"{kind}\" isn't one of the animated mascots. Pick one of: {}.",
                MASCOT_KINDS.join(", ")
            )))
        };
    }
    Err(refusal())
}

/// Validates a submitted reference and returns the form to store.
///
/// Trimming here rather than at each call site is what keeps a copy-pasted
/// value with a trailing space from being stored as a reference that parses
/// nowhere.
pub fn normalize(value: &str) -> Result<String> {
    parse(value)?;
    Ok(value.trim().to_string())
}

/// Whether `node` could be a workspace node id.
///
/// Node ids are ULIDs, but this deliberately checks the *character set* rather
/// than the format: the id is interpolated into a route path by the console, so
/// what matters is that it cannot carry a separator or an escape. Whether it
/// names a node that exists is the read's answer, not this function's — a
/// deleted avatar node is a 404 on one image, which the console draws as the
/// hashed default.
fn is_node_id(node: &str) -> bool {
    !node.is_empty()
        && node.len() <= 64
        && node
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn refusal() -> OpenCompanyError {
    OpenCompanyError::InvalidRequest(format!(
        "an avatar must be \"tiny:<flavour>\" (one of: {}), \"mascot:<kind>\" (one of: {}), or \
         \"blob:<nodeId>\" for an uploaded image. A URL can't be stored as an avatar.",
        TINY_FLAVOURS.join(", "),
        MASCOT_KINDS.join(", ")
    ))
}

/// Validates a submitted reference **and what it points at**, returning the form
/// to store.
///
/// [`normalize`] answers "is this a well-formed reference"; this answers "is
/// there an image here". The difference matters because a `blob:` reference is
/// just a node id, and any member can type one: pointed at a 60 MB PDF it makes
/// every surface that draws a face try to decode a PDF as an image, on every
/// page load, for everyone. Checking the referent turns that from a thing the
/// roster does into a `400` on the request that asked for it.
///
/// A `tiny:` reference needs no lookup — the file is shipped with the console —
/// so the store is only touched for the form that names something mutable.
pub async fn resolve(
    workspace: &dyn crate::ports::WorkspaceStore,
    company: &crate::ports::types::CompanyId,
    value: &str,
) -> Result<String> {
    let stored = normalize(value)?;
    let AvatarRef::Blob(node_id) = parse(&stored)? else {
        return Ok(stored);
    };
    let Some((node, stream)) = workspace.read_bytes(company, node_id).await? else {
        return Err(OpenCompanyError::InvalidRequest(
            "that image isn't here any more. Upload it again.".to_string(),
        ));
    };
    // The store's own byte count, when it has one, refused before any of the
    // payload is buffered. The stream below re-checks with the bytes
    // themselves, so a store that leaves `size` unset is still bounded.
    if let Some(size) = node.size
        && size > MAX_AVATAR_BYTES as u64
    {
        return Err(not_an_image());
    }
    // The bytes themselves are the only claim worth trusting. A `blob:`
    // reference can name any binary this host holds, and the type a generic
    // workspace upload declared is a claim by whoever uploaded it — a member
    // can reach the 4 MiB avatar ceiling with `image/png` on arbitrary bytes.
    // Sniffing here closes the gap between the avatar route (which sniffs
    // before storing) and a reference typed by hand.
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = stream;
    while let Some(chunk) = stream.next().await.transpose()? {
        if bytes.len().saturating_add(chunk.len()) > MAX_AVATAR_BYTES {
            return Err(not_an_image());
        }
        bytes.extend_from_slice(&chunk);
    }
    match sniff_image(&bytes) {
        Some(sniffed) => {
            // The bytes are a supported format; now make sure they are not a
            // decompression bomb. A hand-typed `blob:` can name any binary this
            // host holds, and one that claims 65535×65535 in a few compressed
            // bytes would make every surface that draws a face allocate it.
            check_image_dimensions(&bytes)?;
            // The bytes and the stored type have to agree, because the type a
            // node was stored under is a claim by whoever uploaded it: the
            // generic workspace route keeps the declared `Content-Type`, and
            // only the avatar route sniffs before storing. A `blob:` whose
            // stored `image/png` is really a GIF would render as a PNG from
            // this path and as a GIF from the Files tab — the same bytes, two
            // faces. A node with no declared type has nothing to agree with,
            // so it is refused too; every path that stores an avatar records
            // the sniffed type.
            let essence = node
                .mime
                .as_deref()
                .and_then(|m| m.split(';').next())
                .map(str::trim)
                .map(str::to_ascii_lowercase);
            if essence.as_deref() != Some(sniffed) {
                return Err(not_an_image());
            }
            // These bytes are a valid avatar — but *where* they live decides
            // whether they stay valid. A `blob:` reference can name any binary
            // this host holds, and publishing an artifact rewrites its node's
            // bytes under the same id, so a face that pointed at one would
            // silently become whatever the next publish wrote — a 60 MB PDF or
            // a 65535×65535 header — without ever passing the checks above
            // again. Copy the validated bytes into the avatars folder so every
            // stored reference names bytes this host validated and nothing
            // rewrites. A node already there is itself such a copy (or the
            // upload route's own), so it is returned untouched.
            if !avatar_node_is_immutable(workspace, company, &node).await? {
                return store_validated_avatar(workspace, company, &bytes, sniffed).await;
            }
            Ok(stored)
        }
        None => Err(not_an_image()),
    }
}

/// Whether the node already lives under the [`AVATARS_FOLDER`] **and was minted
/// by this host's own avatar writers** — the upload route or a prior [`resolve`]
/// copy, both of which store validated bytes that nothing rewrites.
///
/// The folder name alone is not the test. `PATCH …/workspace/{node}` can move
/// *any* binary beneath a folder named `avatars`, and the artifact mirror
/// rewrites a published node's bytes under the same id whenever the artifact is
/// republished — so a face pointed at a moved artifact node would silently
/// become whatever the next publish wrote, without passing these checks again.
/// The origin is the tell: every writer to `avatars/` that validates bytes
/// records [`WorkspaceOrigin::Operator`], and origin is immutable through a
/// move or a rewrite, while the mirror mints [`WorkspaceOrigin::Agent`] nodes.
/// A node under `avatars/` that a member or a mirror moved there still carries
/// its writer's origin, so it is not treated as a face this host validated. A
/// node with no parent, or under any other folder, has no such guarantee
/// either — the generic workspace upload writes elsewhere.
async fn avatar_node_is_immutable(
    workspace: &dyn crate::ports::WorkspaceStore,
    company: &crate::ports::types::CompanyId,
    node: &WorkspaceNode,
) -> Result<bool> {
    if node.created_by != WorkspaceOrigin::Operator {
        return Ok(false);
    }
    let Some(parent_id) = node.parent_id.as_deref() else {
        return Ok(false);
    };
    let Some((parent, _)) = workspace.read(company, parent_id).await? else {
        return Ok(false);
    };
    Ok(parent.name == AVATARS_FOLDER)
}

/// Mints a fresh binary node holding already-validated avatar bytes, under the
/// [`AVATARS_FOLDER`], and answers with the `blob:` reference to store.
///
/// The immutable copy behind [`resolve`]'s referent rule: bytes are validated
/// once, here, against the same checks the upload route applies, and the node
/// they land in is never rewritten.
async fn store_validated_avatar(
    workspace: &dyn crate::ports::WorkspaceStore,
    company: &crate::ports::types::CompanyId,
    bytes: &[u8],
    sniffed: &str,
) -> Result<String> {
    // Adopt-or-create like the upload route, so two writers at the same moment
    // cannot race a second `avatars/` folder into the tree.
    let folder = workspace
        .adopt_or_create_folder(company, None, AVATARS_FOLDER, WorkspaceOrigin::Operator)
        .await?;
    let id = crate::ports::generate_id();
    let node = WorkspaceNode {
        name: format!("avatar-{id}"),
        id: id.clone(),
        kind: NodeKind::File,
        parent_id: Some(folder.id().to_string()),
        updated_at_millis: crate::ports::now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: Some(sniffed.to_string()),
        size: None,
        sha256: None,
        adopted: false,
    };
    let stored = workspace.create_binary(company, &node, bytes).await?;
    Ok(format!("blob:{}", stored.id))
}

/// The refusal a `blob:` reference gets when its bytes are not a supported
/// image or are over the avatar ceiling. One sentence for both: from the
/// caller's side these are one failure — "that isn't an avatar" — and two
/// different sentences for it would read as two different problems.
fn not_an_image() -> OpenCompanyError {
    OpenCompanyError::InvalidRequest(
        "that file isn't a PNG, JPEG, GIF or WebP image, so it can't be an avatar.".to_string(),
    )
}

#[cfg(test)]
#[path = "avatar_tests_formats.rs"]
mod tests_formats;
#[cfg(test)]
#[path = "avatar_tests_reference.rs"]
mod tests_reference;
#[cfg(test)]
#[path = "avatar_tests_resolve.rs"]
mod tests_resolve;

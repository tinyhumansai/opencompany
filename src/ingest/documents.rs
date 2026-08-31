//! PDF and OOXML text extraction (the `documents` feature).
//!
//! Every function here has a twin compiled when the feature is off, answering
//! [`Extracted::Unsupported`] with the feature's name. That shape — rather
//! than `#[cfg]` at the call site — is what keeps `ingest::extract`'s dispatch
//! one table in every build, so a format is never silently routed somewhere
//! else because a feature moved.

use super::Extracted;
#[cfg(feature = "documents")]
use super::text::normalize;

/// The largest declared uncompressed size an OOXML archive or spreadsheet may
/// have before it is refused, in bytes.
///
/// The callers cap the *compressed* blob they hand us (4 MiB for a chat
/// attachment, 25 MiB for a memory drop), but a small highly-compressed part
/// can expand arbitrarily — a zip bomb — and every parser here holds the whole
/// document plus its extraction in memory at once. The entry sizes declared in
/// the archive's central directory are summed before anything is read, and
/// each entry read is additionally capped, so a crafted archive cannot force a
/// multi-hundred-megabyte allocation out of a few-hundred-kilobyte upload.
#[cfg(feature = "documents")]
pub(crate) const MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

/// The most cells a spreadsheet's dense range may hold before it is refused.
///
/// The decompression guard above bounds the *bytes* of an archive, but
/// calamine's `worksheet_range` then materializes the dense bounding box of the
/// cells that actually appear in a sheet: a tiny archive with one cell at `A1`
/// and one at `XFD1048576` passes that guard yet forces a ~17-billion-cell
/// allocation inside the blocking extraction thread. The used range of a real
/// spreadsheet is a small fraction of the grid, so this only bites hostile
/// input; refuse rather than allocate (codex review finding).
#[cfg(feature = "documents")]
pub(crate) const MAX_SPREADSHEET_DENSE_CELLS: usize = 1_000_000;

/// Sums the uncompressed sizes every entry of `archive` declares, without
/// reading any entry data.
///
/// The central directory is the only thing touched, so this stays cheap no
/// matter how much the archive expands. `None` means an entry could not be
/// inspected, which the caller treats as a refusal.
#[cfg(feature = "documents")]
fn declared_uncompressed<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> Option<u64> {
    (0..archive.len()).try_fold(0u64, |total, i| {
        archive
            .by_index(i)
            .ok()
            .map(|file| total.saturating_add(file.size()))
    })
}

/// Extracts a PDF's text layer.
///
/// A scanned PDF has none, and that is [`Extracted::Empty`], not a failure:
/// the file was read, it simply carries pictures of words. The console says so
/// rather than reporting an error the operator cannot act on without OCR.
#[cfg(feature = "documents")]
pub fn pdf(bytes: &[u8]) -> Extracted {
    // `pdf-extract` panics on some malformed documents rather than erroring,
    // and a panic here would take down the request task. Caught so one bad
    // file in a dropped folder is one reported row, not a failed upload of
    // everything beside it.
    let extracted = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    match extracted {
        Ok(Ok(text)) if text.trim().is_empty() => Extracted::Empty,
        Ok(Ok(text)) => Extracted::Text(normalize(&text)),
        Ok(Err(error)) => Extracted::Unsupported(format!("the PDF could not be read: {error}")),
        Err(_) => Extracted::Unsupported(
            "the PDF is malformed enough that the parser gave up on it".to_string(),
        ),
    }
}

/// Extracts the body text of a Word document.
#[cfg(feature = "documents")]
pub fn docx(bytes: &[u8]) -> Extracted {
    // `w:p` is a paragraph and `w:t` is a run of text inside it: joining runs
    // without a paragraph break would run every heading into the sentence
    // after it, and chunking splits on paragraphs.
    ooxml(bytes, &["word/document.xml"], "w:p", "w:t")
}

/// Extracts the text of every slide in a deck, in slide order.
#[cfg(feature = "documents")]
pub fn pptx(bytes: &[u8]) -> Extracted {
    ooxml_glob(bytes, "ppt/slides/slide", "a:p", "a:t")
}

/// Extracts a spreadsheet as one `sheet | cell | cell` line per row.
///
/// Flattened rather than reconstructed as a table because that is what recall
/// can use: a chunk that reads `Q3 | EMEA | 412000` answers a question about
/// EMEA revenue, and the same data as aligned columns does not survive
/// chunking at all.
#[cfg(feature = "documents")]
pub fn xlsx(bytes: &[u8]) -> Extracted {
    use calamine::{Data, Reader};

    // Zip-bomb guard before calamine materializes the whole workbook: the
    // declared uncompressed sizes are summed from the central directory, and
    // an archive that expands beyond the cap is refused without parsing any
    // entry data. `None` (an uninspectable archive) is refused too.
    let mut archive = match zip::ZipArchive::new(std::io::Cursor::new(bytes)) {
        Ok(archive) => archive,
        Err(error) => {
            return Extracted::Unsupported(format!("the spreadsheet could not be read: {error}"));
        }
    };
    if declared_uncompressed(&mut archive).is_none_or(|total| total > MAX_DECOMPRESSED_BYTES) {
        return Extracted::Unsupported(
            "the spreadsheet expands beyond the size this build can read safely".to_string(),
        );
    }

    let cursor = std::io::Cursor::new(bytes.to_vec());
    // Opened as a concrete Xlsx rather than auto-detected: the dispatch only
    // ever sends `.xlsx`/`.xlsm` here, and the other formats calamine's auto
    // open would fall back to (`.xls`, `.xlsb`, `.ods`) build their dense
    // ranges during open — before any extent guard could run — so accepting a
    // mislabeled file would leave the same allocation attack open in them.
    // A file that says `.xlsx` and is not one is refused cleanly instead.
    let mut workbook = match calamine::open_workbook_from_rs::<calamine::Xlsx<_>, _>(cursor) {
        Ok(workbook) => workbook,
        Err(error) => {
            return Extracted::Unsupported(format!("the spreadsheet could not be read: {error}"));
        }
    };
    let mut out = String::new();
    for name in workbook.sheet_names().to_vec() {
        // The dense-range guard: `worksheet_range` materializes the bounding
        // box of a sheet's *actual* cells, so scan them sparsely first (cheap
        // — no grid is allocated) and refuse a sheet whose box would exceed
        // the cap before the materialization happens. A sheet with no cells
        // has nothing to materialize. The reader is dropped here so the
        // workbook's borrow is free for `worksheet_range` below.
        let dense = {
            let Ok(mut reader) = workbook.worksheet_cells_reader(&name) else {
                continue;
            };
            let mut row_min = u32::MAX;
            let mut row_max = 0;
            let mut col_min = u32::MAX;
            let mut col_max = 0;
            while let Ok(Some(cell)) = reader.next_cell() {
                let (row, col) = cell.get_position();
                row_min = row_min.min(row);
                row_max = row_max.max(row);
                col_min = col_min.min(col);
                col_max = col_max.max(col);
            }
            if row_min == u32::MAX {
                continue;
            }
            (row_max - row_min + 1).saturating_mul(col_max - col_min + 1) as usize
        };
        if dense > MAX_SPREADSHEET_DENSE_CELLS {
            return Extracted::Unsupported(
                "the spreadsheet's used range exceeds the size this build can read safely"
                    .to_string(),
            );
        }
        let Ok(range) = workbook.worksheet_range(&name) else {
            continue;
        };
        for row in range.rows() {
            let cells: Vec<String> = row
                .iter()
                .map(|cell| match cell {
                    Data::Empty => String::new(),
                    other => other.to_string(),
                })
                .collect();
            if cells.iter().all(|c| c.trim().is_empty()) {
                continue;
            }
            out.push_str(&name);
            for cell in cells {
                out.push_str(" | ");
                out.push_str(cell.trim());
            }
            out.push('\n');
        }
    }
    if out.trim().is_empty() {
        return Extracted::Empty;
    }
    Extracted::Text(normalize(&out))
}

/// Reads the named entries of an OOXML archive and pulls `text_tag` runs,
/// breaking a paragraph wherever `paragraph_tag` closes.
#[cfg(feature = "documents")]
fn ooxml(bytes: &[u8], entries: &[&str], paragraph_tag: &str, text_tag: &str) -> Extracted {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(archive) => archive,
        Err(error) => {
            return Extracted::Unsupported(format!("the document is not a readable file: {error}"));
        }
    };
    // Zip-bomb guard before any entry data is read: refuse an archive whose
    // declared expansion exceeds the cap instead of materializing it.
    if declared_uncompressed(&mut archive).is_none_or(|total| total > MAX_DECOMPRESSED_BYTES) {
        return Extracted::Unsupported(
            "the document expands beyond the size this build can read safely".to_string(),
        );
    }
    let mut out = String::new();
    for entry in entries {
        let Ok(file) = archive.by_name(entry) else {
            continue;
        };
        let mut xml = String::new();
        // Capped read as well: a lying archive that declares small sizes but
        // streams more data cannot force an unbounded allocation either.
        let mut capped = std::io::Read::take(file, MAX_DECOMPRESSED_BYTES);
        if std::io::Read::read_to_string(&mut capped, &mut xml).is_err() {
            continue;
        }
        out.push_str(&xml_text(&xml, paragraph_tag, text_tag));
    }
    if out.trim().is_empty() {
        return Extracted::Empty;
    }
    Extracted::Text(normalize(&out))
}

/// The [`ooxml`] shape for a part whose entries are numbered (`slide1.xml`,
/// `slide2.xml`, …), read in numeric order.
#[cfg(feature = "documents")]
fn ooxml_glob(bytes: &[u8], prefix: &str, paragraph_tag: &str, text_tag: &str) -> Extracted {
    let cursor = std::io::Cursor::new(bytes);
    let Ok(archive) = zip::ZipArchive::new(cursor) else {
        return Extracted::Unsupported("the document is not a readable file".to_string());
    };
    // Numeric, not lexicographic: `slide10.xml` sorts before `slide2.xml` as a
    // string, and a deck whose slides recall out of order is worse than one
    // that does not recall at all.
    let mut names: Vec<String> = archive
        .file_names()
        .filter(|n| n.starts_with(prefix) && n.ends_with(".xml"))
        .map(str::to_string)
        .collect();
    names.sort_by_key(|n| {
        n.trim_start_matches(prefix)
            .trim_end_matches(".xml")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    ooxml(bytes, &refs, paragraph_tag, text_tag)
}

/// Concatenates every `text_tag` run, inserting a blank line at each
/// `paragraph_tag` close.
#[cfg(feature = "documents")]
fn xml_text(xml: &str, paragraph_tag: &str, text_tag: &str) -> String {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = String::new();
    let mut in_text = false;
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(tag)) if tag.name().as_ref() == text_tag.as_bytes() => in_text = true,
            Ok(Event::End(tag)) if tag.name().as_ref() == text_tag.as_bytes() => in_text = false,
            Ok(Event::End(tag)) if tag.name().as_ref() == paragraph_tag.as_bytes() => {
                out.push('\n');
                out.push('\n');
            }
            Ok(Event::Text(text)) if in_text => {
                out.push_str(&text.decode().unwrap_or_default());
            }
            Ok(Event::Eof) => break,
            // A malformed part yields what was read up to it rather than
            // nothing: a truncated document still holds the text before the
            // break, and that is what the operator dropped it for.
            Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
    out
}

/// Names the feature rather than the format: an operator whose PDF was
/// refused needs to know their build lacks the parser, not that PDFs are
/// unsupported in general.
#[cfg(not(feature = "documents"))]
fn without_feature(format: &str) -> Extracted {
    Extracted::Unsupported(format!(
        "reading {format} needs the `documents` feature, which this build was compiled without"
    ))
}

#[cfg(not(feature = "documents"))]
pub fn pdf(_bytes: &[u8]) -> Extracted {
    without_feature("PDFs")
}

#[cfg(not(feature = "documents"))]
pub fn docx(_bytes: &[u8]) -> Extracted {
    without_feature("Word documents")
}

#[cfg(not(feature = "documents"))]
pub fn pptx(_bytes: &[u8]) -> Extracted {
    without_feature("PowerPoint decks")
}

#[cfg(not(feature = "documents"))]
pub fn xlsx(_bytes: &[u8]) -> Extracted {
    without_feature("spreadsheets")
}

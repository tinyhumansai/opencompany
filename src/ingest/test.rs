//! Tests for extraction and chunking.

use super::*;

/// Markdown dropped from a folder tree arrives as `application/octet-stream`
/// on every browser that has no mapping for the extension. Trusting the
/// declared type would refuse the single most common thing dropped here.
#[test]
fn a_text_file_declared_as_octet_stream_is_still_extracted() {
    let extracted = extract(
        "notes.md",
        Some("application/octet-stream"),
        b"# Title\n\nBody text.",
    );
    assert_eq!(
        extracted.text().map(str::to_string),
        Some("# Title\n\nBody text.".to_string())
    );
}

/// Binary bytes are refused rather than stored as replacement characters: a
/// memory that says nothing is worse than an upload that reported why.
#[test]
fn binary_bytes_are_refused_rather_than_mangled() {
    let extracted = extract(
        "logo.png",
        Some("image/png"),
        &[0x89, b'P', b'N', b'G', 0x00],
    );
    assert!(
        matches!(extracted, Extracted::Unsupported(_)),
        "got {extracted:?}"
    );
}

/// An empty file is `Empty`, not `Text("")` — "we read it and it said nothing"
/// must be distinguishable from a stored memory.
#[test]
fn an_empty_file_extracts_as_empty() {
    assert_eq!(
        extract("blank.txt", Some("text/plain"), b"   \n\n"),
        Extracted::Empty
    );
}

/// A legacy binary format names what to do about it, rather than falling into
/// the generic "not UTF-8" refusal.
#[test]
fn a_legacy_office_format_says_how_to_convert_it() {
    let Extracted::Unsupported(reason) = extract("contract.doc", None, b"\xd0\xcf\x11\xe0") else {
        panic!("a .doc must be refused");
    };
    assert!(reason.contains("export it"), "{reason}");
}

/// Script and style bodies never reach memory: a page's inline analytics would
/// otherwise be the first thing recall finds in it.
#[test]
fn html_extraction_drops_scripts_and_tags() {
    let Extracted::Text(text) = text::from_html(
        "<html><head><style>p{color:red}</style></head><body><script>track('x')</script>\
         <h1>Pricing</h1><p>Enterprise is &pound;40&nbsp;per seat.</p></body></html>",
    ) else {
        panic!("expected text");
    };
    assert!(text.contains("Pricing"), "{text}");
    assert!(text.contains("Enterprise is"), "{text}");
    assert!(!text.contains("track"), "the script body leaked: {text}");
    assert!(!text.contains("color:red"), "the style body leaked: {text}");
}

/// An unclosed `<script>` swallows the rest of the document — what a browser
/// does with it too — rather than leaking code into memory.
#[test]
fn an_unclosed_script_does_not_leak_its_body() {
    let extracted = text::from_html("<body><p>Keep</p><script>secret_token='abc'");
    let Extracted::Text(text) = extracted else {
        panic!("expected text");
    };
    assert!(text.contains("Keep"), "{text}");
    assert!(!text.contains("secret_token"), "{text}");
}

/// Chunk labels are addressing keys, so a folder drop's relative path must not
/// nest one document's chunks under another's prefix.
#[test]
fn a_path_like_source_slugs_into_one_label_segment() {
    let label = label_for("Contracts/2026/acme msa.pdf");
    assert_eq!(label, "document/contracts-2026-acme-msa.pdf");
    assert_eq!(
        label.matches('/').count(),
        1,
        "one segment after the prefix"
    );
}

/// Every chunk names its document: a chunk surfaces from recall alone, and a
/// paragraph of a contract is only useful if you know which contract.
#[test]
fn every_chunk_names_its_source() {
    let text = "Alpha paragraph.\n\nBeta paragraph.";
    let chunks = chunk_document("msa.pdf", text);
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].body.starts_with("msa.pdf\n\n"));
    assert!(chunks[0].label.starts_with("document/msa.pdf/"));
}

/// A long document splits, and the split is bounded — the property that keeps
/// one dropped file from flooding a turn's context.
#[test]
fn a_long_document_splits_into_bounded_chunks() {
    let paragraph = "Sentence about revenue. ".repeat(60);
    let text = std::iter::repeat_n(paragraph.trim(), 6)
        .collect::<Vec<_>>()
        .join("\n\n");
    let chunks = chunk_document("report.txt", &text);
    assert!(chunks.len() > 1, "expected a split, got {}", chunks.len());
    for chunk in &chunks {
        assert!(
            chunk.body.chars().count() <= 3_100,
            "chunk of {} chars: {}",
            chunk.body.chars().count(),
            chunk.label
        );
    }
    // Labels stay ordered, so the console and recall both see the document in
    // reading order.
    let labels: Vec<&str> = chunks.iter().map(|c| c.label.as_str()).collect();
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    assert_eq!(labels, sorted);
}

/// A single paragraph longer than the hard cap is cut rather than emitted
/// whole — the case a spreadsheet export or minified prose produces.
#[test]
fn one_enormous_paragraph_is_hard_split() {
    let text = "word ".repeat(2_000);
    let chunks = chunk_document("rows.csv", &text);
    assert!(chunks.len() > 1);
    assert!(chunks.iter().all(|c| !c.body.trim().is_empty()));
    // Nothing is lost in the cut: every word survives somewhere.
    let joined: String = chunks
        .iter()
        .map(|c| c.body.replace("rows.csv\n\n", ""))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(joined.matches("word").count(), 2_000);
}

/// Extraction of a real OOXML archive, built here rather than checked in as a
/// fixture so the test says what it is asserting about.
#[cfg(feature = "documents")]
#[test]
fn a_docx_yields_its_paragraphs_in_order() {
    let document = r#"<?xml version="1.0"?>
<w:document xmlns:w="x"><w:body>
<w:p><w:r><w:t>First heading</w:t></w:r></w:p>
<w:p><w:r><w:t>Second </w:t></w:r><w:r><w:t>paragraph.</w:t></w:r></w:p>
</w:body></w:document>"#;
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("word/document.xml", options).unwrap();
        std::io::Write::write_all(&mut writer, document.as_bytes()).unwrap();
        writer.finish().unwrap();
    }
    let Extracted::Text(text) = extract("spec.docx", None, &buffer.into_inner()) else {
        panic!("expected text");
    };
    assert!(text.starts_with("First heading"), "{text}");
    assert!(
        text.contains("Second paragraph."),
        "runs inside one paragraph join without a break: {text}"
    );
    assert!(
        text.contains("First heading\n\nSecond"),
        "paragraphs keep their break: {text:?}"
    );
}

/// A build without the parsers refuses by naming the feature, so an operator
/// learns why their PDF produced nothing.
#[cfg(not(feature = "documents"))]
#[test]
fn a_pdf_without_the_feature_names_the_feature() {
    let Extracted::Unsupported(reason) = extract("contract.pdf", Some("application/pdf"), b"%PDF-")
    else {
        panic!("expected a refusal");
    };
    assert!(reason.contains("documents"), "{reason}");
}

/// An archive whose entries declare more uncompressed bytes than the cap is
/// refused before any entry data is read — the zip-bomb guard. The fixture is
/// one entry of zero bytes just over the cap: zeroes compress to almost
/// nothing, so the archive is a few hundred bytes on disk while its declared
/// expansion exceeds the limit, exactly the shape of a crafted bomb.
#[cfg(feature = "documents")]
#[test]
fn an_overexpanding_document_is_refused_not_allocated() {
    use crate::ingest::documents::MAX_DECOMPRESSED_BYTES;

    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("word/document.xml", options).unwrap();
        std::io::Write::write_all(&mut writer, &vec![0u8; MAX_DECOMPRESSED_BYTES as usize + 1])
            .unwrap();
        writer.finish().unwrap();
    }
    let Extracted::Unsupported(reason) = extract("bomb.docx", None, &buffer.into_inner()) else {
        panic!("an overexpanding document must be refused");
    };
    assert!(reason.contains("expands"), "{reason}");
}

/// Builds a minimal XLSX archive from a worksheet's `sheetData` fragment.
///
/// The package parts are the smallest set calamine's Xlsx reader accepts: the
/// content-type map, the root and workbook relationships, and the workbook
/// itself. No shared strings or styles, which the reader tolerates.
#[cfg(feature = "documents")]
fn xlsx_with_sheet(sheet_data: &str) -> Vec<u8> {
    let content_types = r#"<?xml version="1.0"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;
    let workbook_rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
    let worksheet = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>{sheet_data}</sheetData>
</worksheet>"#
    );
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (path, contents) in [
            ("[Content_Types].xml", content_types),
            ("_rels/.rels", root_rels),
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", workbook_rels),
            ("xl/worksheets/sheet1.xml", &worksheet),
        ] {
            writer.start_file(path, options).unwrap();
            std::io::Write::write_all(&mut writer, contents.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }
    buffer.into_inner()
}

/// A tiny spreadsheet still extracts its rows — the dense-range guard must not
/// refuse ordinary files, and the concrete (non-auto) Xlsx open must not
/// either.
#[cfg(feature = "documents")]
#[test]
fn a_small_spreadsheet_yields_its_rows_in_order() {
    let bytes = xlsx_with_sheet(
        r#"<row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>alpha</v></c></row>
           <row r="2"><c r="A2"><v>2</v></c><c r="B2"><v>beta</v></c></row>"#,
    );
    let Extracted::Text(text) = extract("ledger.xlsx", None, &bytes) else {
        panic!("a small spreadsheet must extract");
    };
    assert!(text.contains("alpha"), "{text}");
    assert!(text.contains("beta"), "{text}");
}

/// The dense-range guard: a spreadsheet whose cells span the whole grid — one
/// at `A1` and one at `XFD1048576` — passes the decompression cap (it is a few
/// hundred bytes) yet would force calamine's `worksheet_range` to materialize
/// a ~17-billion-cell dense grid. It must be refused before that allocation,
/// not after (codex review finding).
#[cfg(feature = "documents")]
#[test]
fn a_spreadsheet_with_far_cells_is_refused_not_allocated() {
    let bytes = xlsx_with_sheet(
        r#"<row r="1"><c r="A1"><v>1</v></c></row>
           <row r="1048576"><c r="XFD1048576"><v>2</v></c></row>"#,
    );
    let Extracted::Unsupported(reason) = extract("spread.xlsx", None, &bytes) else {
        panic!("a spreadsheet with far-apart cells must be refused");
    };
    assert!(reason.contains("used range"), "{reason}");
}

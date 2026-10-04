use std::io::Write;
use std::path::{Path, PathBuf};

use docparse::docx::DocxSource;
use docparse::{BlockKind, PageOrigin, Source};

/// Writes a minimal but structurally valid .docx containing `body_xml`.
/// Building fixtures in code keeps binary blobs out of the repository and
/// makes each test state its own input.
fn write_docx(dir: &Path, name: &str, body_xml: &str) -> PathBuf {
    let path = dir.join(name);
    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("word/document.xml", opts).unwrap();
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>{body_xml}</w:body></w:document>"#
    )
    .unwrap();
    zip.finish().unwrap();
    path
}

fn para(text: &str) -> String {
    format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
}

#[test]
fn extracts_paragraphs_headings_and_list_items_with_their_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        "{}{}{}",
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Chapter One</w:t></w:r></w:p>"#,
        para("A plain paragraph."),
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>First bullet</w:t></w:r></w:p>"#,
    );
    let path = write_docx(dir.path(), "a.docx", &body);

    let mut source = DocxSource::open(&path).unwrap();
    let page = source.next_page().unwrap().unwrap();

    assert!(source.next_page().is_none(), "expected exactly one page");
    assert_eq!(page.number, 1);
    assert_eq!(page.origin, PageOrigin::Synthesized);
    assert_eq!(page.width, 0.0, "DOCX has no page geometry");

    let kinds: Vec<&BlockKind> = page.blocks.iter().map(|b| &b.kind).collect();
    assert_eq!(
        kinds,
        vec![
            &BlockKind::Heading { level: 1 },
            &BlockKind::Paragraph,
            &BlockKind::ListItem
        ]
    );
    assert_eq!(page.blocks[0].text, "Chapter One");
    assert_eq!(page.blocks[1].text, "A plain paragraph.");
    assert!(
        page.blocks.iter().all(|b| b.bbox.is_none()),
        "DOCX blocks must not fabricate coordinates"
    );
    assert_eq!(
        page.blocks.iter().map(|b| b.id).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[test]
fn table_cells_record_their_row_and_column() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        "<w:tbl><w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr><w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>",
        para("r0c0"),
        para("r0c1"),
        para("r1c0"),
        para("r1c1"),
    );
    let path = write_docx(dir.path(), "t.docx", &body);

    let mut source = DocxSource::open(&path).unwrap();
    let page = source.next_page().unwrap().unwrap();

    let cells: Vec<(&str, &BlockKind)> = page
        .blocks
        .iter()
        .map(|b| (b.text.as_str(), &b.kind))
        .collect();
    assert_eq!(
        cells,
        vec![
            ("r0c0", &BlockKind::TableCell { row: 0, col: 0 }),
            ("r0c1", &BlockKind::TableCell { row: 0, col: 1 }),
            ("r1c0", &BlockKind::TableCell { row: 1, col: 0 }),
            ("r1c1", &BlockKind::TableCell { row: 1, col: 1 }),
        ]
    );
}

#[test]
fn a_section_break_starts_a_new_page_and_resets_block_ids() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        "{}{}{}",
        para("Section one body."),
        r#"<w:p><w:pPr><w:sectPr><w:type w:val="nextPage"/></w:sectPr></w:pPr><w:r><w:t>Last of section one.</w:t></w:r></w:p>"#,
        para("Section two body."),
    );
    let path = write_docx(dir.path(), "s.docx", &body);

    let mut source = DocxSource::open(&path).unwrap();
    let first = source.next_page().unwrap().unwrap();
    let second = source.next_page().unwrap().unwrap();
    assert!(source.next_page().is_none());

    assert_eq!(first.number, 1);
    assert_eq!(first.blocks.len(), 2);
    assert_eq!(second.number, 2);
    assert_eq!(second.blocks.len(), 1);
    assert_eq!(second.blocks[0].text, "Section two body.");
    assert_eq!(second.blocks[0].id, 0, "block ids restart per page");
}

#[test]
fn empty_paragraphs_produce_no_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!("{}{}{}", para(""), para("   "), para("Real content."));
    let path = write_docx(dir.path(), "e.docx", &body);

    let mut source = DocxSource::open(&path).unwrap();
    let page = source.next_page().unwrap().unwrap();

    assert_eq!(page.blocks.len(), 1);
    assert_eq!(page.blocks[0].text, "Real content.");
}

#[test]
fn a_zip_without_document_xml_is_a_malformed_error_not_an_empty_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.docx");
    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
    zip.start_file("unrelated.txt", opts).unwrap();
    write!(zip, "nothing useful").unwrap();
    zip.finish().unwrap();

    let err = DocxSource::open(&path).unwrap_err();
    assert!(
        matches!(err, docparse::Error::Malformed { .. }),
        "got {err:?}"
    );
}

#[test]
fn nested_table_does_not_corrupt_the_outer_tables_cells() {
    let dir = tempfile::tempdir().unwrap();
    let inner = format!("<w:tbl><w:tr><w:tc>{}</w:tc></w:tr></w:tbl>", para("inner"));
    let body = format!(
        "<w:tbl><w:tr><w:tc>{inner}<w:p/></w:tc><w:tc>{}</w:tc></w:tr><w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>",
        para("r0c1"),
        para("r1c0"),
        para("r1c1"),
    );
    let path = write_docx(dir.path(), "n.docx", &body);

    let mut source = DocxSource::open(&path).unwrap();
    let page = source.next_page().unwrap().unwrap();

    let cells: Vec<(&str, &BlockKind)> = page
        .blocks
        .iter()
        .map(|b| (b.text.as_str(), &b.kind))
        .collect();
    assert_eq!(
        cells,
        vec![
            ("inner", &BlockKind::TableCell { row: 0, col: 0 }),
            ("r0c1", &BlockKind::TableCell { row: 0, col: 1 }),
            ("r1c0", &BlockKind::TableCell { row: 1, col: 0 }),
            ("r1c1", &BlockKind::TableCell { row: 1, col: 1 }),
        ]
    );
}

#[test]
fn numpr_with_numid_zero_is_not_a_list_item() {
    let dir = tempfile::tempdir().unwrap();
    let body = concat!(
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="0"/></w:numPr></w:pPr><w:r><w:t>Not a list</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="3"/></w:numPr></w:pPr><w:r><w:t>A list</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/></w:numPr></w:pPr><w:r><w:t>No numId</w:t></w:r></w:p>"#,
    );
    let path = write_docx(dir.path(), "z.docx", body);

    let mut source = DocxSource::open(&path).unwrap();
    let page = source.next_page().unwrap().unwrap();
    let kinds: Vec<&BlockKind> = page.blocks.iter().map(|b| &b.kind).collect();
    assert_eq!(
        kinds,
        vec![
            &BlockKind::Paragraph,
            &BlockKind::ListItem,
            &BlockKind::Paragraph
        ]
    );
}

#[test]
fn heading_levels_other_than_one_are_parsed() {
    let dir = tempfile::tempdir().unwrap();
    let body =
        r#"<w:p><w:pPr><w:pStyle w:val="Heading3"/></w:pPr><w:r><w:t>Deep</w:t></w:r></w:p>"#;
    let path = write_docx(dir.path(), "h.docx", body);

    let mut source = DocxSource::open(&path).unwrap();
    let page = source.next_page().unwrap().unwrap();
    assert_eq!(page.blocks[0].kind, BlockKind::Heading { level: 3 });
}

#[test]
fn a_trailing_body_level_sectpr_adds_no_empty_page() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        "{}{}",
        para("Only content."),
        r#"<w:sectPr><w:type w:val="nextPage"/></w:sectPr>"#
    );
    let path = write_docx(dir.path(), "x.docx", &body);

    let mut source = DocxSource::open(&path).unwrap();
    let page = source.next_page().unwrap().unwrap();
    assert_eq!(page.blocks.len(), 1);
    assert!(source.next_page().is_none(), "no trailing empty page");
}

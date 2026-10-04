//! DOCX extraction. A `.docx` is a ZIP whose `word/document.xml` holds
//! the text; no C dependency is involved.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::error::{Error, Result};
use crate::model::{Block, BlockKind, Page, PageOrigin};
use crate::source::Source;

/// Reads a `.docx` file as a sequence of synthesized pages, one per section.
#[derive(Debug)]
pub struct DocxSource {
    pages: std::vec::IntoIter<Page>,
}

impl DocxSource {
    /// Opens `path` and parses its `word/document.xml` into pages.
    ///
    /// Returns `Error::Malformed` if the file is not a zip or lacks
    /// `word/document.xml`.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let mut archive =
            zip::ZipArchive::new(BufReader::new(file)).map_err(|e| Error::Malformed {
                path: path.to_path_buf(),
                detail: format!("not a readable zip archive: {e}"),
            })?;

        let entry = archive
            .by_name("word/document.xml")
            .map_err(|_| Error::Malformed {
                path: path.to_path_buf(),
                detail: "missing word/document.xml".to_string(),
            })?;

        let pages = parse_document(BufReader::new(entry), path)?;
        Ok(Self {
            pages: pages.into_iter(),
        })
    }
}

impl Source for DocxSource {
    fn next_page(&mut self) -> Option<Result<Page>> {
        self.pages.next().map(Ok)
    }
}

/// Accumulated state for the paragraph currently being read.
#[derive(Default)]
struct Para {
    text: String,
    style: Option<String>,
    is_list: bool,
    ends_section: bool,
}

fn parse_document<R: std::io::BufRead>(reader: R, path: &Path) -> Result<Vec<Page>> {
    let mut xml = Reader::from_reader(reader);
    let mut buf = Vec::new();

    let mut pages: Vec<Page> = Vec::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut next_block_id: u32 = 0;
    let mut page_number: u32 = 1;

    let mut para = Para::default();
    let mut in_text = false;
    // One (row, col) frame per open <w:tbl>, so nested tables do not
    // disturb the outer table's position.
    let mut tables: Vec<(u32, u32)> = Vec::new();

    let malformed = |e: quick_xml::Error| Error::Malformed {
        path: path.to_path_buf(),
        detail: e.to_string(),
    };

    loop {
        match xml.read_event_into(&mut buf).map_err(malformed)? {
            Event::Eof => break,

            Event::Start(e) => match e.local_name().as_ref() {
                b"p" => para = Para::default(),
                b"t" => in_text = true,
                b"tbl" => tables.push((0, 0)),
                b"tr" => {
                    if let Some(frame) = tables.last_mut() {
                        frame.1 = 0;
                    }
                }
                b"sectPr" => para.ends_section = true,
                _ => {}
            },

            Event::Empty(e) => match e.local_name().as_ref() {
                b"pStyle" => para.style = attr_value(&e, b"val"),
                // numId 0 means "no list" in Word; only a non-zero id counts.
                b"numId" => {
                    para.is_list = attr_value(&e, b"val")
                        .and_then(|v| v.trim().parse::<u32>().ok())
                        .is_some_and(|id| id != 0);
                }
                b"tab" => para.text.push(' '),
                b"br" | b"cr" => para.text.push('\n'),
                _ => {}
            },

            Event::Text(t) if in_text => {
                // `unescape` turns &amp; back into &.
                para.text.push_str(&t.unescape().map_err(malformed)?);
            }

            Event::End(e) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"tc" => {
                    if let Some(frame) = tables.last_mut() {
                        frame.1 += 1;
                    }
                }
                b"tr" => {
                    if let Some(frame) = tables.last_mut() {
                        frame.0 += 1;
                    }
                }
                b"tbl" => {
                    tables.pop();
                }

                b"p" => {
                    let text = crate::normalize::normalize_block_text(&para.text);
                    if !text.is_empty() {
                        let kind = if let Some(&(row, col)) = tables.last() {
                            BlockKind::TableCell { row, col }
                        } else if para.is_list {
                            BlockKind::ListItem
                        } else if let Some(level) = heading_level(para.style.as_deref()) {
                            BlockKind::Heading { level }
                        } else {
                            BlockKind::Paragraph
                        };
                        blocks.push(Block {
                            id: next_block_id,
                            bbox: None, // DOCX has no geometry; say so honestly.
                            kind,
                            text,
                        });
                        next_block_id += 1;
                    }

                    // A sectPr marks the end of a section, which is the
                    // closest thing DOCX has to a page boundary.
                    if para.ends_section && !blocks.is_empty() {
                        pages.push(Page {
                            number: page_number,
                            width: 0.0,
                            height: 0.0,
                            origin: PageOrigin::Synthesized,
                            blocks: std::mem::take(&mut blocks),
                        });
                        page_number += 1;
                        next_block_id = 0;
                    }
                    para = Para::default();
                }

                _ => {}
            },

            _ => {}
        }
        buf.clear();
    }

    if !blocks.is_empty() {
        pages.push(Page {
            number: page_number,
            width: 0.0,
            height: 0.0,
            origin: PageOrigin::Synthesized,
            blocks,
        });
    }

    Ok(pages)
}

fn attr_value(e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| String::from_utf8(a.value.to_vec()).ok())
}

/// `"Heading2"` -> `Some(2)`. Anything else -> `None`.
fn heading_level(style: Option<&str>) -> Option<u8> {
    let style = style?;
    let rest = style
        .strip_prefix("Heading")
        .or_else(|| style.strip_prefix("heading"))?;
    rest.trim()
        .parse::<u8>()
        .ok()
        .filter(|level| (1..=9).contains(level))
}

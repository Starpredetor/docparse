use serde::{Deserialize, Serialize};

/// A bounding box in PDF points, origin at the page's bottom-left.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockKind {
    Paragraph,
    /// A single line or text run, not a reconstructed paragraph. PDF
    /// extraction yields these; merging them into paragraphs would be a
    /// layout heuristic this crate deliberately does not attempt.
    Line,
    Heading {
        level: u8,
    },
    ListItem,
    TableCell {
        row: u32,
        col: u32,
    },
    Caption,
    Other,
}

/// Where a page's text actually came from. A consumer can distinguish
/// extracted text from an OCR guess; the Python pipeline concatenated a
/// BLIP caption with OCR output into one undifferentiated string.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum PageOrigin {
    /// Text extracted directly from the document.
    Native,
    /// No real page exists in the source format; this boundary was invented.
    Synthesized,
    Ocr {
        confidence: f32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// Sequential within its page, starting at 0.
    pub id: u32,
    /// `None` when the source format has no geometry, as with DOCX.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Rect>,
    #[serde(flatten)]
    pub kind: BlockKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page {
    /// 1-based, and always the true source page number.
    pub number: u32,
    /// Points. Zero when the format has no page geometry.
    pub width: f32,
    pub height: f32,
    #[serde(flatten)]
    pub origin: PageOrigin,
    pub blocks: Vec<Block>,
}

/// Provenance: which block on which page a chunk's text came from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BlockRef {
    pub page: u32,
    pub block: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: u64,
    pub source: String,
    /// Inclusive span of real page numbers.
    pub page_start: u32,
    pub page_end: u32,
    pub blocks: Vec<BlockRef>,
    pub text: String,
    #[serde(flatten)]
    pub origin: PageOrigin,
    /// True when a single sentence exceeded `max_chars` and had to be cut.
    pub hard_split: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json<T: Serialize>(v: &T) -> String {
        serde_json::to_string(v).unwrap()
    }

    #[test]
    fn block_without_geometry_omits_bbox_entirely() {
        let block = Block {
            id: 0,
            bbox: None,
            kind: BlockKind::Paragraph,
            text: "hello".to_string(),
        };

        // Absent, not null: a DOCX block has no coordinates, and the
        // output says so by omission rather than by fabricating zeroes.
        assert_eq!(
            json(&block),
            r#"{"id":0,"kind":"paragraph","text":"hello"}"#
        );
        assert_eq!(serde_json::from_str::<Block>(&json(&block)).unwrap(), block);
    }

    #[test]
    fn block_with_geometry_and_table_cell_kind_is_flat() {
        let block = Block {
            id: 4,
            bbox: Some(Rect {
                x0: 1.0,
                y0: 2.0,
                x1: 3.0,
                y1: 4.0,
            }),
            kind: BlockKind::TableCell { row: 1, col: 2 },
            text: "x".to_string(),
        };

        assert_eq!(
            json(&block),
            r#"{"id":4,"bbox":{"x0":1.0,"y0":2.0,"x1":3.0,"y1":4.0},"kind":"table_cell","row":1,"col":2,"text":"x"}"#
        );
        assert_eq!(serde_json::from_str::<Block>(&json(&block)).unwrap(), block);
    }

    #[test]
    fn ocr_page_flattens_origin_and_round_trips() {
        let page = Page {
            number: 3,
            width: 612.0,
            height: 792.0,
            origin: PageOrigin::Ocr { confidence: 0.82 },
            blocks: vec![],
        };

        assert_eq!(
            json(&page),
            r#"{"number":3,"width":612.0,"height":792.0,"origin":"ocr","confidence":0.82,"blocks":[]}"#
        );
        assert_eq!(serde_json::from_str::<Page>(&json(&page)).unwrap(), page);
    }

    #[test]
    fn native_page_origin_is_a_plain_string() {
        let page = Page {
            number: 1,
            width: 0.0,
            height: 0.0,
            origin: PageOrigin::Native,
            blocks: vec![],
        };

        assert_eq!(
            json(&page),
            r#"{"number":1,"width":0.0,"height":0.0,"origin":"native","blocks":[]}"#
        );
        assert_eq!(serde_json::from_str::<Page>(&json(&page)).unwrap(), page);
    }

    #[test]
    fn chunk_round_trips_with_flattened_origin() {
        let chunk = Chunk {
            id: 7,
            source: "a.pdf".to_string(),
            page_start: 1,
            page_end: 2,
            blocks: vec![
                BlockRef { page: 1, block: 0 },
                BlockRef { page: 2, block: 3 },
            ],
            text: "t".to_string(),
            origin: PageOrigin::Ocr { confidence: 0.5 },
            hard_split: false,
        };

        assert_eq!(
            json(&chunk),
            r#"{"id":7,"source":"a.pdf","page_start":1,"page_end":2,"blocks":[{"page":1,"block":0},{"page":2,"block":3}],"text":"t","origin":"ocr","confidence":0.5,"hard_split":false}"#
        );
        assert_eq!(serde_json::from_str::<Chunk>(&json(&chunk)).unwrap(), chunk);
    }

    #[test]
    fn heading_level_survives_a_round_trip() {
        let kind = BlockKind::Heading { level: 2 };
        assert_eq!(json(&kind), r#"{"kind":"heading","level":2}"#);
        assert_eq!(
            serde_json::from_str::<BlockKind>(&json(&kind)).unwrap(),
            kind
        );
    }
}

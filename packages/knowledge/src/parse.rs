//! Turning files into text with a locator.
//!
//! Every parser returns segments carrying a human-meaningful location — a page
//! for PDFs, a line range for text, a row range for CSV — so a citation can
//! point somewhere a person can actually look.

use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use otwono_types::knowledge::DocumentFormat;

/// Files larger than this are skipped rather than read into memory.
pub const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;

/// A parsed span of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    /// "page 4", "lines 1-40", "rows 2-51".
    pub locator: Option<String>,
}

impl Segment {
    pub fn new(text: impl Into<String>, locator: Option<String>) -> Self {
        Self {
            text: text.into(),
            locator,
        }
    }
}

/// Why a file could not be parsed. Recorded against the document so the
/// Knowledge screen can show the user what happened.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("this file is {size} bytes, larger than the {limit} byte limit")]
    TooLarge { size: u64, limit: u64 },

    #[error("this file is not valid text; it may be binary or use an unusual encoding")]
    NotText,

    #[error("this PDF could not be read: {0}")]
    Pdf(String),

    #[error("this Word document could not be read: {0}")]
    Docx(String),

    #[error("no readable text was found in this file")]
    Empty,
}

/// Read a file into segments according to its format.
pub fn parse(path: &Path, format: DocumentFormat) -> Result<Vec<Segment>> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ParseError::TooLarge {
            size: metadata.len(),
            limit: MAX_FILE_BYTES,
        }
        .into());
    }

    let segments = match format {
        DocumentFormat::Pdf => parse_pdf(path)?,
        DocumentFormat::Docx => parse_docx(path)?,
        DocumentFormat::Csv => parse_csv(path)?,
        DocumentFormat::Text | DocumentFormat::Markdown | DocumentFormat::SourceCode => {
            parse_text(path)?
        }
    };

    if segments.iter().all(|s| s.text.trim().is_empty()) {
        return Err(ParseError::Empty.into());
    }
    Ok(segments)
}

/// Plain text, Markdown and source files. Segmented into 200-line blocks so
/// that a citation points at a line range rather than a whole file.
fn parse_text(path: &Path) -> Result<Vec<Segment>> {
    let bytes = std::fs::read(path)?;
    let text = decode_utf8(&bytes)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok(vec![Segment::new(text, None)]);
    }

    const BLOCK: usize = 200;
    Ok(lines
        .chunks(BLOCK)
        .enumerate()
        .map(|(index, block)| {
            let first = index * BLOCK + 1;
            let last = first + block.len() - 1;
            Segment::new(
                block.join("\n"),
                Some(if first == last {
                    format!("line {first}")
                } else {
                    format!("lines {first}-{last}")
                }),
            )
        })
        .collect())
}

/// CSV and TSV. The header row is repeated into every segment so a chunk taken
/// from the middle of a large file is still interpretable.
fn parse_csv(path: &Path) -> Result<Vec<Segment>> {
    let bytes = std::fs::read(path)?;
    let text = decode_utf8(&bytes)?;
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return Err(ParseError::Empty.into());
    };
    let rows: Vec<&str> = lines.collect();
    if rows.is_empty() {
        return Ok(vec![Segment::new(header, Some("row 1".into()))]);
    }

    const BLOCK: usize = 50;
    Ok(rows
        .chunks(BLOCK)
        .enumerate()
        .map(|(index, block)| {
            let first = index * BLOCK + 2;
            let last = first + block.len() - 1;
            let mut body = String::from(header);
            body.push('\n');
            body.push_str(&block.join("\n"));
            Segment::new(body, Some(format!("rows {first}-{last}")))
        })
        .collect())
}

fn parse_pdf(path: &Path) -> Result<Vec<Segment>> {
    // `pdf_extract` panics on some malformed files; contain that so one bad
    // PDF cannot take down an indexing run.
    let path_owned = path.to_path_buf();
    let extracted = std::panic::catch_unwind(move || pdf_extract::extract_text(&path_owned))
        .map_err(|_| ParseError::Pdf("the file is malformed".to_string()))?
        .map_err(|e| ParseError::Pdf(e.to_string()))?;

    if extracted.trim().is_empty() {
        // A scanned PDF has no text layer. Say so precisely rather than
        // reporting a generic failure the user cannot act on.
        return Err(ParseError::Pdf(
            "no text layer was found; this looks like a scanned document, which needs optical \
             character recognition before it can be indexed"
                .to_string(),
        )
        .into());
    }

    // `extract_text` separates pages with a form feed.
    let pages: Vec<&str> = extracted.split('\u{c}').collect();
    Ok(pages
        .iter()
        .enumerate()
        .filter(|(_, page)| !page.trim().is_empty())
        .map(|(index, page)| Segment::new(page.trim(), Some(format!("page {}", index + 1))))
        .collect())
}

/// DOCX is a ZIP archive; the body lives in `word/document.xml`. Paragraphs are
/// `<w:p>` and text runs are `<w:t>`.
fn parse_docx(path: &Path) -> Result<Vec<Segment>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| ParseError::Docx(e.to_string()))?;
    let mut xml = String::new();
    archive
        .by_name("word/document.xml")
        .map_err(|_| {
            ParseError::Docx("the archive has no word/document.xml; it may not be a .docx".into())
        })?
        .read_to_string(&mut xml)
        .map_err(|e| ParseError::Docx(e.to_string()))?;

    let paragraphs = docx_paragraphs(&xml)?;
    if paragraphs.is_empty() {
        return Err(ParseError::Empty.into());
    }

    const BLOCK: usize = 40;
    Ok(paragraphs
        .chunks(BLOCK)
        .enumerate()
        .map(|(index, block)| {
            let first = index * BLOCK + 1;
            let last = first + block.len() - 1;
            Segment::new(
                block.join("\n\n"),
                Some(if first == last {
                    format!("paragraph {first}")
                } else {
                    format!("paragraphs {first}-{last}")
                }),
            )
        })
        .collect())
}

/// Extract paragraph text from WordprocessingML.
pub(crate) fn docx_paragraphs(xml: &str) -> Result<Vec<String>> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_text = false;
    let mut buffer = Vec::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) => match element.local_name().as_ref() {
                b"t" => in_text = true,
                b"tab" => current.push('\t'),
                _ => {}
            },
            Ok(Event::Empty(element)) => match element.local_name().as_ref() {
                b"br" | b"cr" => current.push('\n'),
                b"tab" => current.push('\t'),
                _ => {}
            },
            Ok(Event::End(element)) => match element.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" => {
                    let trimmed = current.trim();
                    if !trimmed.is_empty() {
                        paragraphs.push(trimmed.to_string());
                    }
                    current.clear();
                }
                _ => {}
            },
            Ok(Event::Text(text)) if in_text => {
                current.push_str(&text.unescape().unwrap_or_default());
            }
            Ok(Event::Eof) => break,
            Err(error) => bail!(ParseError::Docx(error.to_string())),
            _ => {}
        }
        buffer.clear();
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        paragraphs.push(trimmed.to_string());
    }
    Ok(paragraphs)
}

/// Decode as UTF-8, tolerating a byte-order mark, and refuse binary content.
fn decode_utf8(bytes: &[u8]) -> Result<String> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    // A NUL byte in the first kilobyte is the usual sign of a binary file.
    if bytes.iter().take(1024).any(|b| *b == 0) {
        return Err(ParseError::NotText.into());
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| anyhow!(ParseError::NotText))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn plain_text_is_segmented_with_line_locators() {
        let tmp = tempfile::tempdir().unwrap();
        let body: String = (1..=450).map(|i| format!("line {i}\n")).collect();
        let path = write(tmp.path(), "notes.txt", body.as_bytes());

        let segments = parse(&path, DocumentFormat::Text).unwrap();
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].locator.as_deref(), Some("lines 1-200"));
        assert_eq!(segments[1].locator.as_deref(), Some("lines 201-400"));
        assert_eq!(segments[2].locator.as_deref(), Some("lines 401-450"));
        assert!(segments[1].text.starts_with("line 201"));
    }

    #[test]
    fn markdown_and_source_files_parse_as_text() {
        let tmp = tempfile::tempdir().unwrap();
        let md = write(tmp.path(), "readme.md", b"# Title\n\nSome prose.");
        let rs = write(
            tmp.path(),
            "main.rs",
            b"fn main() {\n    println!(\"hi\");\n}",
        );
        assert!(parse(&md, DocumentFormat::Markdown).unwrap()[0]
            .text
            .contains("# Title"));
        assert!(parse(&rs, DocumentFormat::SourceCode).unwrap()[0]
            .text
            .contains("fn main"));
    }

    #[test]
    fn csv_repeats_its_header_so_a_middle_chunk_still_makes_sense() {
        let tmp = tempfile::tempdir().unwrap();
        let mut body = String::from("name,role,city\n");
        for i in 1..=120 {
            body.push_str(&format!("Person {i},Engineer,Leeds\n"));
        }
        let path = write(tmp.path(), "people.csv", body.as_bytes());

        let segments = parse(&path, DocumentFormat::Csv).unwrap();
        assert_eq!(segments.len(), 3);
        for segment in &segments {
            assert!(
                segment.text.starts_with("name,role,city"),
                "every chunk needs the header"
            );
        }
        assert_eq!(segments[1].locator.as_deref(), Some("rows 52-101"));
    }

    #[test]
    fn a_binary_file_is_refused_rather_than_indexed_as_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "image.txt", &[0x89, 0x50, 0x00, 0x4E, 0x47]);
        let error = parse(&path, DocumentFormat::Text).unwrap_err();
        assert!(error.to_string().contains("not valid text"), "{error}");
    }

    #[test]
    fn an_oversized_file_is_skipped_with_the_limit_named() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("huge.txt");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(file);

        let error = parse(&path, DocumentFormat::Text).unwrap_err();
        assert!(error.to_string().contains("larger than"), "{error}");
    }

    #[test]
    fn an_empty_file_is_reported_as_having_no_text() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "empty.txt", b"   \n  \n");
        let error = parse(&path, DocumentFormat::Text).unwrap_err();
        assert!(error.to_string().contains("no readable text"), "{error}");
    }

    #[test]
    fn a_byte_order_mark_does_not_corrupt_the_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"Hello");
        let path = write(tmp.path(), "bom.txt", &bytes);
        assert_eq!(parse(&path, DocumentFormat::Text).unwrap()[0].text, "Hello");
    }

    #[test]
    fn word_paragraphs_are_extracted_with_breaks_and_tabs_preserved() {
        let xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>First paragraph</w:t></w:r></w:p>
    <w:p><w:r><w:t>Second</w:t></w:r><w:r><w:tab/><w:t>with a tab</w:t></w:r></w:p>
    <w:p><w:r><w:t xml:space="preserve">Third with </w:t></w:r><w:r><w:t>two runs</w:t></w:r></w:p>
    <w:p></w:p>
  </w:body>
</w:document>"#;
        let paragraphs = docx_paragraphs(xml).unwrap();
        assert_eq!(
            paragraphs.len(),
            3,
            "empty paragraphs are dropped: {paragraphs:?}"
        );
        assert_eq!(paragraphs[0], "First paragraph");
        assert!(paragraphs[1].contains('\t'));
        assert_eq!(paragraphs[2], "Third with two runs");
    }

    #[test]
    fn xml_entities_in_word_text_are_decoded() {
        let xml = r#"<w:document xmlns:w="x"><w:body><w:p><w:r>
            <w:t>Tom &amp; Jerry &lt;3</w:t></w:r></w:p></w:body></w:document>"#;
        assert_eq!(docx_paragraphs(xml).unwrap()[0], "Tom & Jerry <3");
    }

    #[test]
    fn a_docx_that_is_not_really_a_docx_is_reported_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "fake.docx", b"this is not a zip archive");
        let error = parse(&path, DocumentFormat::Docx).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Word document could not be read"),
            "{error}"
        );
    }
}

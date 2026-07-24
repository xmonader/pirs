//! Office / document text extraction for the `read` tool and skills.
//!
//! Binary formats (docx/pptx/xlsx/pdf/odt/…) must never be returned as
//! lossy UTF-8 garbage. We extract structured text (or clear guidance).

use std::io::{Cursor, Read};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context as _};
use zip::ZipArchive;

/// Extensions we treat as office/binary documents (not plain text).
pub fn is_office_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "docx"
            | "dotx"
            | "docm"
            | "pptx"
            | "pptm"
            | "potx"
            | "xlsx"
            | "xlsm"
            | "xltx"
            | "ods"
            | "odt"
            | "odp"
            | "pdf"
            | "rtf"
            | "doc" // legacy — conversion guidance only
            | "ppt"
            | "xls"
    )
}

/// Extract a human/LLM-readable text preview of an office document.
pub fn extract_document(path: &Path) -> anyhow::Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !is_office_ext(&ext) {
        bail!("not an office document extension: {ext}");
    }
    match ext.as_str() {
        "docx" | "dotx" | "docm" => extract_docx(path),
        "pptx" | "pptm" | "potx" => extract_pptx(path),
        "xlsx" | "xlsm" | "xltx" => extract_xlsx(path),
        "odt" | "ods" | "odp" => extract_opendocument(path),
        "pdf" => extract_pdf(path),
        "rtf" => extract_rtf(path),
        "doc" | "ppt" | "xls" => Ok(format!(
            "[legacy binary Office format .{ext}]\n\
             Convert to OOXML first, e.g.:\n\
               libreoffice --headless --convert-to {target} {}\n\
             or install the office-documents skill and follow its scripts.\n\
             Then re-read the converted file.",
            path.display(),
            target = match ext.as_str() {
                "doc" => "docx",
                "ppt" => "pptx",
                _ => "xlsx",
            }
        )),
        _ => bail!("unsupported office ext .{ext}"),
    }
}

fn open_zip(path: &Path) -> anyhow::Result<ZipArchive<Cursor<Vec<u8>>>> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    ZipArchive::new(Cursor::new(bytes)).with_context(|| format!("open zip {}", path.display()))
}

fn read_zip_entry(archive: &mut ZipArchive<Cursor<Vec<u8>>>, name: &str) -> Option<String> {
    let mut file = archive.by_name(name).ok()?;
    let mut s = String::new();
    file.read_to_string(&mut s).ok()?;
    Some(s)
}

/// Strip XML tags and collapse whitespace for OOXML text nodes.
fn xml_text_content(xml: &str) -> String {
    // Prefer <w:t>, <a:t>, <t> text nodes when present.
    let mut out = String::new();
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        let before = &rest[..start];
        if !before.is_empty() {
            // Only keep text that looks like it came from text nodes (not attributes).
            // Full strip of tags is fine for preview.
        }
        if let Some(end) = rest[start..].find('>') {
            let tag = &rest[start + 1..start + end];
            rest = &rest[start + end + 1..];
            // Text after closing tags of common text elements is handled by
            // scanning for >text< patterns below.
            let _ = tag;
            let _ = before;
        } else {
            break;
        }
    }
    // Simple approach: remove tags, decode a few entities, normalize space.
    let mut plain = String::with_capacity(xml.len() / 4);
    let mut in_tag = false;
    for ch in xml.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => plain.push(ch),
            _ => {}
        }
    }
    let plain = plain
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#10;", "\n")
        .replace("&#13;", "\n");
    // OOXML often jams words together; keep newlines from paragraph markers we inject.
    let mut collapsed = String::new();
    let mut prev_space = false;
    for ch in plain.chars() {
        if ch == '\n' || ch == '\r' {
            if !collapsed.ends_with('\n') {
                collapsed.push('\n');
            }
            prev_space = true;
            continue;
        }
        if ch.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
                prev_space = true;
            }
        } else {
            collapsed.push(ch);
            prev_space = false;
        }
    }
    out.push_str(collapsed.trim());
    out
}

/// Insert newlines at paragraph boundaries before stripping tags.
fn prep_ooxml_paragraphs(xml: &str) -> String {
    xml.replace("</w:p>", "</w:p>\n")
        .replace("</a:p>", "</a:p>\n")
        .replace("</text:p>", "</text:p>\n")
        .replace("<w:tab/>", "\t")
        .replace("<w:br/>", "\n")
        .replace("<a:br/>", "\n")
}

fn extract_docx(path: &Path) -> anyhow::Result<String> {
    let mut archive = open_zip(path)?;
    let xml = read_zip_entry(&mut archive, "word/document.xml").ok_or_else(|| {
        anyhow::anyhow!("docx missing word/document.xml — file may be corrupt")
    })?;
    let text = xml_text_content(&prep_ooxml_paragraphs(&xml));
    let mut out = format!(
        "[docx extracted from {}]\n\n{}\n",
        path.display(),
        text.trim()
    );
    // Optional notes
    if let Some(notes) = read_zip_entry(&mut archive, "word/comments.xml") {
        let t = xml_text_content(&notes);
        if !t.trim().is_empty() {
            out.push_str("\n--- comments ---\n");
            out.push_str(t.trim());
            out.push('\n');
        }
    }
    if text.trim().is_empty() {
        out.push_str(
            "\n[no text extracted — document may be image-only or empty; \
             use office-documents skill / unpack for structure]\n",
        );
    }
    Ok(truncate_extract(&out))
}

fn extract_pptx(path: &Path) -> anyhow::Result<String> {
    let mut archive = open_zip(path)?;
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
    let mut slides: Vec<String> = names
        .into_iter()
        .filter(|n| {
            n.starts_with("ppt/slides/slide") && n.ends_with(".xml") && !n.contains("_rels")
        })
        .collect();
    slides.sort_by(|a, b| nat_slide(a).cmp(&nat_slide(b)));
    let mut out = format!("[pptx extracted from {}]\n", path.display());
    if slides.is_empty() {
        out.push_str("\n[no slides found]\n");
        return Ok(out);
    }
    for (i, name) in slides.iter().enumerate() {
        let xml = read_zip_entry(&mut archive, name).unwrap_or_default();
        let text = xml_text_content(&prep_ooxml_paragraphs(&xml));
        out.push_str(&format!("\n## Slide {}\n{}\n", i + 1, text.trim()));
    }
    // Notes if present
    let note_names: Vec<String> = archive
        .file_names()
        .filter(|n| n.starts_with("ppt/notesSlides/") && n.ends_with(".xml"))
        .map(|s| s.to_string())
        .collect();
    if !note_names.is_empty() {
        out.push_str("\n--- speaker notes ---\n");
        for name in note_names {
            if let Some(xml) = read_zip_entry(&mut archive, &name) {
                let t = xml_text_content(&prep_ooxml_paragraphs(&xml));
                if !t.trim().is_empty() {
                    out.push_str(t.trim());
                    out.push_str("\n\n");
                }
            }
        }
    }
    Ok(truncate_extract(&out))
}

fn nat_slide(name: &str) -> u32 {
    name.chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

fn extract_xlsx(path: &Path) -> anyhow::Result<String> {
    let mut archive = open_zip(path)?;
    let shared = read_zip_entry(&mut archive, "xl/sharedStrings.xml").unwrap_or_default();
    let strings = parse_shared_strings(&shared);
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
    let mut sheets: Vec<String> = names
        .into_iter()
        .filter(|n| n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml"))
        .collect();
    sheets.sort();
    let mut out = format!("[xlsx extracted from {}]\n", path.display());
    // Sheet names from workbook
    let wb = read_zip_entry(&mut archive, "xl/workbook.xml").unwrap_or_default();
    let sheet_titles = parse_sheet_names(&wb);
    for (i, name) in sheets.iter().enumerate() {
        let title = sheet_titles
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("Sheet{}", i + 1));
        let xml = read_zip_entry(&mut archive, name).unwrap_or_default();
        let table = sheet_to_tsv(&xml, &strings);
        out.push_str(&format!("\n## {title}\n```\n{table}\n```\n"));
    }
    if sheets.is_empty() {
        out.push_str("\n[no worksheets found]\n");
    }
    Ok(truncate_extract(&out))
}

fn parse_shared_strings(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Each <si>…</si> may contain one or more <t>…</t>
    let mut rest = xml;
    while let Some(si_start) = rest.find("<si") {
        let after = &rest[si_start..];
        let Some(si_end) = after.find("</si>") else {
            break;
        };
        let block = &after[..si_end];
        let mut s = String::new();
        let mut r = block;
        while let Some(t0) = r.find("<t") {
            let t_rest = &r[t0..];
            let Some(gt) = t_rest.find('>') else { break };
            let after_t = &t_rest[gt + 1..];
            let Some(close) = after_t.find("</t>") else { break };
            s.push_str(&after_t[..close]);
            r = &after_t[close + 4..];
        }
        out.push(
            s.replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        );
        rest = &after[si_end + 5..];
    }
    out
}

fn parse_sheet_names(workbook_xml: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = workbook_xml;
    while let Some(i) = rest.find("<sheet ") {
        let after = &rest[i..];
        if let Some(name_pos) = after.find("name=\"") {
            let n = &after[name_pos + 6..];
            if let Some(end) = n.find('"') {
                names.push(n[..end].to_string());
            }
        }
        rest = &after[1..];
    }
    names
}

fn sheet_to_tsv(sheet_xml: &str, shared: &[String]) -> String {
    // Collect cells as (row, col, value)
    let mut rows: std::collections::BTreeMap<u32, std::collections::BTreeMap<u32, String>> =
        std::collections::BTreeMap::new();
    let mut rest = sheet_xml;
    while let Some(c0) = rest.find("<c ") {
        let after = &rest[c0..];
        let Some(tag_end) = after.find('>') else { break };
        let open = &after[..tag_end];
        let self_close = open.ends_with('/');
        let r = attr(open, "r").unwrap_or("");
        let (col, row) = parse_cell_ref(r);
        let t = attr(open, "t").unwrap_or("");
        let value = if self_close {
            String::new()
        } else {
            let body = &after[tag_end + 1..];
            let Some(close) = body.find("</c>") else {
                rest = &after[1..];
                continue;
            };
            let inner = &body[..close];
            if let Some(v0) = inner.find("<v>") {
                let v = &inner[v0 + 3..];
                if let Some(ve) = v.find("</v>") {
                    let raw = &v[..ve];
                    if t == "s" {
                        // shared string index
                        raw.parse::<usize>()
                            .ok()
                            .and_then(|i| shared.get(i).cloned())
                            .unwrap_or_else(|| raw.to_string())
                    } else if t == "inlineStr" {
                        xml_text_content(inner)
                    } else {
                        raw.to_string()
                    }
                } else {
                    String::new()
                }
            } else if inner.contains("<is>") {
                xml_text_content(inner)
            } else {
                String::new()
            }
        };
        if row > 0 {
            rows.entry(row).or_default().insert(col, value);
        }
        rest = if self_close {
            &after[tag_end + 1..]
        } else {
            let body = &after[tag_end + 1..];
            match body.find("</c>") {
                Some(c) => &body[c + 4..],
                None => &after[1..],
            }
        };
        // Cap rows for preview
        if rows.len() > 200 {
            break;
        }
    }
    let mut lines = Vec::new();
    for (_r, cols) in rows.iter().take(200) {
        let max_c = *cols.keys().max().unwrap_or(&0);
        let mut cells = Vec::new();
        for c in 0..=max_c.min(40) {
            cells.push(cols.get(&c).cloned().unwrap_or_default());
        }
        lines.push(cells.join("\t"));
    }
    if lines.is_empty() {
        "(empty sheet)".into()
    } else {
        lines.join("\n")
    }
}

fn attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}=\"");
    let i = tag.find(&pat)?;
    let rest = &tag[i + pat.len()..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn parse_cell_ref(r: &str) -> (u32, u32) {
    let mut col = 0u32;
    let mut row = 0u32;
    for ch in r.chars() {
        if ch.is_ascii_alphabetic() {
            col = col * 26 + (ch.to_ascii_uppercase() as u32 - b'A' as u32 + 1);
        } else if ch.is_ascii_digit() {
            row = row * 10 + (ch as u32 - b'0' as u32);
        }
    }
    (col.saturating_sub(1), row)
}

fn extract_opendocument(path: &Path) -> anyhow::Result<String> {
    let mut archive = open_zip(path)?;
    let xml = read_zip_entry(&mut archive, "content.xml").ok_or_else(|| {
        anyhow::anyhow!("OpenDocument missing content.xml")
    })?;
    let text = xml_text_content(
        &xml.replace("</text:p>", "</text:p>\n")
            .replace("</text:h>", "</text:h>\n")
            .replace("<text:line-break/>", "\n")
            .replace("<text:tab/>", "\t"),
    );
    Ok(truncate_extract(&format!(
        "[opendocument extracted from {}]\n\n{}\n",
        path.display(),
        text.trim()
    )))
}

fn extract_pdf(path: &Path) -> anyhow::Result<String> {
    // Prefer system extractors; pure-Rust PDF is heavy and incomplete.
    for (bin, args) in [
        ("pdftotext", vec!["-layout", "-nopgbrk"]),
        ("mutool", vec!["draw", "-F", "txt"]),
    ] {
        let mut cmd = Command::new(bin);
        if bin == "pdftotext" {
            cmd.args(&args).arg(path).arg("-");
        } else {
            cmd.args(&args).arg(path);
        }
        if let Ok(out) = cmd.output() {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                if !text.trim().is_empty() {
                    return Ok(truncate_extract(&format!(
                        "[pdf via {bin} from {}]\n\n{}\n",
                        path.display(),
                        text.trim()
                    )));
                }
            }
        }
    }
    // Python fallbacks (avoid "##" inside raw strings — ends r##"…"## early)
    let py = r#"
import sys
path = sys.argv[1]
try:
    from pypdf import PdfReader
    r = PdfReader(path)
    for i, p in enumerate(r.pages):
        t = p.extract_text() or ""
        print("== Page %d ==" % (i + 1))
        print(t)
    sys.exit(0)
except Exception:
    pass
try:
    import pdfplumber
    with pdfplumber.open(path) as pdf:
        for i, p in enumerate(pdf.pages):
            print("== Page %d ==" % (i + 1))
            print(p.extract_text() or "")
    sys.exit(0)
except Exception as e:
    print("[pdf extract failed: %s]" % e, file=sys.stderr)
    sys.exit(1)
"#;
    if let Ok(out) = Command::new("python3")
        .args(["-c", py, &path.display().to_string()])
        .output()
    {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            if !text.trim().is_empty() {
                return Ok(truncate_extract(&format!(
                    "[pdf via python from {}]\n\n{}\n",
                    path.display(),
                    text.trim()
                )));
            }
        }
    }
    Ok(format!(
        "[pdf] could not extract text from {}\n\
         Install one of: poppler-utils (pdftotext), mupdf-tools (mutool), or pip install pypdf.\n\
         Then re-run read, or use bash with those tools.\n",
        path.display()
    ))
}

fn extract_rtf(path: &Path) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    // Very rough RTF strip: drop {\…} control words.
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                let h1 = chars.next().unwrap_or('0');
                let h2 = chars.next().unwrap_or('0');
                if let Ok(b) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                    out.push(b as char);
                }
            } else {
                while matches!(chars.peek(), Some(c) if c.is_ascii_alphabetic()) {
                    chars.next();
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
        } else if ch == '{' || ch == '}' {
            continue;
        } else {
            out.push(ch);
        }
    }
    Ok(truncate_extract(&format!(
        "[rtf extracted from {}]\n\n{}\n",
        path.display(),
        out.trim()
    )))
}

const MAX_EXTRACT_CHARS: usize = 120_000;

fn truncate_extract(s: &str) -> String {
    if s.chars().count() <= MAX_EXTRACT_CHARS {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_EXTRACT_CHARS).collect();
    format!(
        "{head}\n\n[truncated — extracted text exceeded {MAX_EXTRACT_CHARS} chars; \
         use bash/python for full extract or page with offset tools]\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    fn write_minimal_docx(path: &Path, paragraph: &str) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Types></Types>"#).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        let xml = format!(
            r#"<?xml version="1.0"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>{paragraph}</w:t></w:r></w:p></w:body>
            </w:document>"#
        );
        zip.write_all(xml.as_bytes()).unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn extracts_docx_paragraph() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.docx");
        write_minimal_docx(&path, "Hello office world");
        let text = extract_document(&path).unwrap();
        assert!(text.contains("Hello office world"), "{text}");
        assert!(text.contains("docx extracted"), "{text}");
    }

    #[test]
    fn is_office_detects_formats() {
        assert!(is_office_ext("docx"));
        assert!(is_office_ext("PPTX"));
        assert!(is_office_ext("xlsx"));
        assert!(is_office_ext("pdf"));
        assert!(!is_office_ext("rs"));
        assert!(!is_office_ext("md"));
    }

    fn write_minimal_pptx(path: &Path, title: &str) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Types></Types>"#).unwrap();
        zip.start_file("ppt/slides/slide1.xml", opts).unwrap();
        let xml = format!(
            r#"<?xml version="1.0"?>
            <p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                   xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
              <p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
            </p:sld>"#
        );
        zip.write_all(xml.as_bytes()).unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn extracts_pptx_slide_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.pptx");
        write_minimal_pptx(&path, "Deck Title Here");
        let text = extract_document(&path).unwrap();
        assert!(text.contains("Deck Title Here"), "{text}");
        assert!(text.contains("Slide 1"), "{text}");
    }
}

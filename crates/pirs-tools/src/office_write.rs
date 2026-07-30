//! First-class office document create/update (minimal valid OOXML).
//!
//! Complements [`crate::office::extract_document`]: agents use `office_document`
//! instead of ad-hoc bash + python-docx for the happy path. Packages are
//! intentionally minimal (body/sheet/slide text fidelity, not full layout).

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::paths;

// ── pure OOXML writers ──────────────────────────────────────────────────────

fn zip_opts() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Create a minimal `.docx` with one paragraph per line of `body` (or explicit paragraphs).
pub fn create_docx(path: &Path, paragraphs: &[String]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent {}", parent.display()))?;
    }
    let file = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut zip = ZipWriter::new(file);
    let opts = zip_opts();

    zip.start_file("[Content_Types].xml", opts)?;
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
    )?;

    zip.start_file("_rels/.rels", opts)?;
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#,
    )?;

    zip.start_file("word/_rels/document.xml.rels", opts)?;
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
</Relationships>"#,
    )?;

    let body = docx_body_xml(paragraphs);
    zip.start_file("word/document.xml", opts)?;
    zip.write_all(body.as_bytes())?;
    zip.finish()?;
    Ok(())
}

fn docx_body_xml(paragraphs: &[String]) -> String {
    let mut paras = String::new();
    if paragraphs.is_empty() {
        paras.push_str("<w:p><w:r><w:t></w:t></w:r></w:p>");
    } else {
        for p in paragraphs {
            // Split multi-line paragraphs into soft breaks within one run when needed.
            let escaped = xml_escape(p);
            paras.push_str(&format!(
                "<w:p><w:r><w:t xml:space=\"preserve\">{escaped}</w:t></w:r></w:p>"
            ));
        }
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    {paras}
    <w:sectPr/>
  </w:body>
</w:document>"#
    )
}

/// Replace document body paragraphs in an existing `.docx` (creates if missing).
pub fn update_docx(path: &Path, paragraphs: &[String], append: bool) -> anyhow::Result<()> {
    if !path.exists() {
        return create_docx(path, paragraphs);
    }
    if append {
        let existing = crate::office::extract_document(path).unwrap_or_default();
        // Keep only body lines (skip extract header).
        let mut merged: Vec<String> = existing
            .lines()
            .skip_while(|l| l.starts_with('[') || l.trim().is_empty())
            .filter(|l| !l.starts_with('[') && !l.starts_with("---"))
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .collect();
        merged.extend(paragraphs.iter().cloned());
        return rewrite_docx_body(path, &merged);
    }
    rewrite_docx_body(path, paragraphs)
}

fn rewrite_docx_body(path: &Path, paragraphs: &[String]) -> anyhow::Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut out_buf = Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut out_buf);
        let opts = zip_opts();
        let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
        let mut wrote_doc = false;
        for name in names {
            let mut f = archive.by_name(&name)?;
            let mut data = Vec::new();
            f.read_to_end(&mut data)?;
            if name == "word/document.xml" {
                writer.start_file(&name, opts)?;
                writer.write_all(docx_body_xml(paragraphs).as_bytes())?;
                wrote_doc = true;
            } else {
                writer.start_file(&name, opts)?;
                writer.write_all(&data)?;
            }
        }
        if !wrote_doc {
            writer.start_file("word/document.xml", opts)?;
            writer.write_all(docx_body_xml(paragraphs).as_bytes())?;
        }
        writer.finish()?;
    }
    std::fs::write(path, out_buf.into_inner())?;
    Ok(())
}

/// Create a minimal `.xlsx` from rows (first row often headers). `sheet` names the worksheet.
pub fn create_xlsx(path: &Path, sheet: &str, rows: &[Vec<String>]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let opts = zip_opts();

    // Build shared strings + sheet cells.
    let mut shared: Vec<String> = Vec::new();
    let mut shared_index = |s: &str| -> usize {
        if let Some(i) = shared.iter().position(|x| x == s) {
            return i;
        }
        shared.push(s.to_string());
        shared.len() - 1
    };

    let mut sheet_xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>"#,
    );
    for (ri, row) in rows.iter().enumerate() {
        let r = ri + 1;
        sheet_xml.push_str(&format!("<row r=\"{r}\">"));
        for (ci, cell) in row.iter().enumerate() {
            let ref_ = cell_ref(ci as u32, r as u32);
            if cell.parse::<f64>().is_ok() && !cell.contains('e') && !cell.contains('E') {
                sheet_xml.push_str(&format!("<c r=\"{ref_}\"><v>{}</v></c>", xml_escape(cell)));
            } else {
                let idx = shared_index(cell);
                sheet_xml.push_str(&format!("<c r=\"{ref_}\" t=\"s\"><v>{idx}</v></c>"));
            }
        }
        sheet_xml.push_str("</row>");
    }
    sheet_xml.push_str("</sheetData></worksheet>");

    let mut sst = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="{}" uniqueCount="{}">"#,
        shared.len(),
        shared.len()
    );
    for s in &shared {
        sst.push_str(&format!("<si><t>{}</t></si>", xml_escape(s)));
    }
    sst.push_str("</sst>");

    let sheet_name = xml_escape(if sheet.is_empty() { "Sheet1" } else { sheet });

    zip.start_file("[Content_Types].xml", opts)?;
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
  <Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>"#,
    )?;

    zip.start_file("_rels/.rels", opts)?;
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
    )?;

    zip.start_file("xl/workbook.xml", opts)?;
    zip.write_all(
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="{sheet_name}" sheetId="1" r:id="rId1"/>
  </sheets>
</workbook>"#
        )
        .as_bytes(),
    )?;

    zip.start_file("xl/_rels/workbook.xml.rels", opts)?;
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#,
    )?;

    zip.start_file("xl/worksheets/sheet1.xml", opts)?;
    zip.write_all(sheet_xml.as_bytes())?;

    zip.start_file("xl/sharedStrings.xml", opts)?;
    zip.write_all(sst.as_bytes())?;

    zip.start_file("xl/styles.xml", opts)?;
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
  <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
  <borders count="1"><border/></borders>
  <cellStyleXfs count="1"><xf/></cellStyleXfs>
  <cellXfs count="1"><xf/></cellXfs>
</styleSheet>"#,
    )?;

    zip.finish()?;
    Ok(())
}

fn cell_ref(col0: u32, row1: u32) -> String {
    // 0-based col → A, B, … Z, AA
    let mut n = col0 + 1;
    let mut s = String::new();
    while n > 0 {
        let rem = ((n - 1) % 26) as u8;
        s.insert(0, (b'A' + rem) as char);
        n = (n - 1) / 26;
    }
    format!("{s}{row1}")
}

/// Replace sheet rows in an existing `.xlsx` (creates if missing).
pub fn update_xlsx(path: &Path, sheet: &str, rows: &[Vec<String>]) -> anyhow::Result<()> {
    // Minimal valid rewrite: recreate package (same API surface; preserves path).
    create_xlsx(path, sheet, rows)
}

/// One slide for pptx create/update.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SlideSpec {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

/// Create a minimal `.pptx` with the given slides.
pub fn create_pptx(path: &Path, slides: &[SlideSpec]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let slides = if slides.is_empty() {
        vec![SlideSpec {
            title: Some("Untitled".into()),
            body: None,
        }]
    } else {
        slides.to_vec()
    };

    let file = std::fs::File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let opts = zip_opts();

    let mut overrides = String::new();
    overrides.push_str(
        r#"  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
"#,
    );
    for i in 1..=slides.len() {
        overrides.push_str(&format!(
            r#"  <Override PartName="/ppt/slides/slide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
"#
        ));
    }

    zip.start_file("[Content_Types].xml", opts)?;
    zip.write_all(
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
{overrides}</Types>"#
        )
        .as_bytes(),
    )?;

    zip.start_file("_rels/.rels", opts)?;
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#,
    )?;

    let mut sld_id_lst = String::new();
    let mut pres_rels = String::new();
    for (i, _) in slides.iter().enumerate() {
        let n = i + 1;
        let rid = n; // rId1.. for slides
        sld_id_lst.push_str(&format!(
            r#"    <p:sldId id="{}" r:id="rId{rid}"/>"#,
            256 + n as u32
        ));
        sld_id_lst.push('\n');
        pres_rels.push_str(&format!(
            r#"  <Relationship Id="rId{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{n}.xml"/>
"#
        ));
    }

    zip.start_file("ppt/presentation.xml", opts)?;
    zip.write_all(
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
                xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:sldIdLst>
{sld_id_lst}  </p:sldIdLst>
</p:presentation>"#
        )
        .as_bytes(),
    )?;

    zip.start_file("ppt/_rels/presentation.xml.rels", opts)?;
    zip.write_all(
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
{pres_rels}</Relationships>"#
        )
        .as_bytes(),
    )?;

    for (i, slide) in slides.iter().enumerate() {
        let n = i + 1;
        let title = slide.title.as_deref().unwrap_or("");
        let body = slide.body.as_deref().unwrap_or("");
        let xml = slide_xml(title, body);
        zip.start_file(format!("ppt/slides/slide{n}.xml"), opts)?;
        zip.write_all(xml.as_bytes())?;
    }

    zip.finish()?;
    Ok(())
}

fn slide_xml(title: &str, body: &str) -> String {
    let mut texts = String::new();
    if !title.is_empty() {
        texts.push_str(&format!(
            r#"<p:sp><p:txBody><a:p><a:r><a:t>{}</a:t></a:r></a:p></p:txBody></p:sp>"#,
            xml_escape(title)
        ));
    }
    if !body.is_empty() {
        for line in body.lines() {
            texts.push_str(&format!(
                r#"<p:sp><p:txBody><a:p><a:r><a:t>{}</a:t></a:r></a:p></p:txBody></p:sp>"#,
                xml_escape(line)
            ));
        }
    }
    if texts.is_empty() {
        texts.push_str(r#"<p:sp><p:txBody><a:p><a:r><a:t></a:t></a:r></a:p></p:txBody></p:sp>"#);
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree>
    <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
    <p:grpSpPr/>
    {texts}
  </p:spTree></p:cSld>
</p:sld>"#
    )
}

/// Replace all slides in a `.pptx` (creates if missing).
pub fn update_pptx(path: &Path, slides: &[SlideSpec]) -> anyhow::Result<()> {
    create_pptx(path, slides)
}

// ── agent tool ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum OfficeAction {
    /// Create a new office file (overwrites if present).
    Create,
    /// Update body/sheet/slides (creates if missing).
    Update,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum OfficeFormat {
    Docx,
    Xlsx,
    Pptx,
    /// Infer from path extension.
    Auto,
}

#[derive(Deserialize, JsonSchema)]
struct OfficeArgs {
    action: OfficeAction,
    /// Path to write (must end in .docx / .xlsx / .pptx unless format set).
    path: String,
    #[serde(default)]
    format: Option<OfficeFormat>,
    /// Plain body for docx (split on newlines into paragraphs).
    #[serde(default)]
    text: Option<String>,
    /// Explicit paragraphs for docx (overrides text when non-empty).
    #[serde(default)]
    paragraphs: Option<Vec<String>>,
    /// When updating docx, append paragraphs instead of replacing.
    #[serde(default)]
    append: Option<bool>,
    /// Sheet name for xlsx (default Sheet1).
    #[serde(default)]
    sheet: Option<String>,
    /// Rows for xlsx: array of arrays of cell strings.
    #[serde(default)]
    rows: Option<Vec<Vec<String>>>,
    /// Slides for pptx.
    #[serde(default)]
    slides: Option<Vec<SlideSpec>>,
}

pub struct OfficeDocumentTool {
    cwd: PathBuf,
}

impl OfficeDocumentTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

fn resolve_format(path: &Path, fmt: Option<OfficeFormat>) -> anyhow::Result<&'static str> {
    match fmt.unwrap_or(OfficeFormat::Auto) {
        OfficeFormat::Docx => Ok("docx"),
        OfficeFormat::Xlsx => Ok("xlsx"),
        OfficeFormat::Pptx => Ok("pptx"),
        OfficeFormat::Auto => {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            match ext.as_str() {
                "docx" | "dotx" | "docm" => Ok("docx"),
                "xlsx" | "xlsm" | "xltx" => Ok("xlsx"),
                "pptx" | "pptm" | "potx" => Ok("pptx"),
                _ => bail!(
                    "cannot infer office format from path {}; set format=docx|xlsx|pptx",
                    path.display()
                ),
            }
        }
    }
}

fn paragraphs_from_args(args: &OfficeArgs) -> Vec<String> {
    if let Some(ps) = &args.paragraphs {
        if !ps.is_empty() {
            return ps.clone();
        }
    }
    args.text
        .as_deref()
        .unwrap_or("")
        .lines()
        .map(|s| s.to_string())
        .collect()
}

#[async_trait]
impl AgentTool for OfficeDocumentTool {
    fn name(&self) -> &str {
        "office_document"
    }

    fn description(&self) -> &str {
        "Create or update Word (.docx), Excel (.xlsx), or PowerPoint (.pptx) files. \
         Use create/update with path + text/paragraphs (docx), rows (xlsx), or slides (pptx). \
         Prefer this over bash/python for the happy path; then verify with `read`."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(OfficeArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("office_document: create/update docx xlsx pptx (then read to verify)")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: OfficeArgs = serde_json::from_value(ctx.args)?;
        let path = paths::resolve_contained(&self.cwd, &args.path)?;
        let kind = resolve_format(&path, args.format)?;
        let _guard = crate::filelock::lock(&path).await;

        let action = args.action;
        match (action, kind) {
            (OfficeAction::Create, "docx") => {
                create_docx(&path, &paragraphs_from_args(&args))?;
            }
            (OfficeAction::Update, "docx") => {
                let append = args.append.unwrap_or(false);
                update_docx(&path, &paragraphs_from_args(&args), append)?;
            }
            (OfficeAction::Create | OfficeAction::Update, "xlsx") => {
                let sheet = args.sheet.as_deref().unwrap_or("Sheet1");
                let rows = args.rows.clone().unwrap_or_default();
                if rows.is_empty() {
                    bail!("xlsx create/update requires rows: [[\"A1\",\"B1\"],[\"A2\",\"B2\"]]");
                }
                match action {
                    OfficeAction::Create => create_xlsx(&path, sheet, &rows)?,
                    OfficeAction::Update => update_xlsx(&path, sheet, &rows)?,
                }
            }
            (OfficeAction::Create | OfficeAction::Update, "pptx") => {
                let slides = args.slides.clone().unwrap_or_default();
                match action {
                    OfficeAction::Create => create_pptx(&path, &slides)?,
                    OfficeAction::Update => update_pptx(&path, &slides)?,
                }
            }
            _ => bail!("unsupported format {kind}"),
        }

        // Round-trip verify via extract so the model sees what was written.
        let preview = crate::office::extract_document(&path)
            .unwrap_or_else(|e| format!("(extract failed: {e})"));
        let preview: String = preview.chars().take(4000).collect();
        Ok(ToolOutput::text(format!(
            "office_document wrote {} ({kind})\n\n{preview}",
            path.display()
        )))
    }
}

pub fn office_tools(cwd: PathBuf) -> Vec<std::sync::Arc<dyn AgentTool>> {
    vec![std::sync::Arc::new(OfficeDocumentTool::new(cwd))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::office::extract_document;

    #[test]
    fn docx_create_update_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memo.docx");
        create_docx(
            &path,
            &["Hello office create".into(), "Second paragraph".into()],
        )
        .unwrap();
        let t = extract_document(&path).unwrap();
        assert!(t.contains("Hello office create"), "{t}");
        assert!(t.contains("Second paragraph"), "{t}");

        update_docx(&path, &["Updated body only".into()], false).unwrap();
        let t2 = extract_document(&path).unwrap();
        assert!(t2.contains("Updated body only"), "{t2}");
        assert!(!t2.contains("Hello office create"), "{t2}");

        update_docx(&path, &["Appended line".into()], true).unwrap();
        let t3 = extract_document(&path).unwrap();
        assert!(t3.contains("Updated body only"), "{t3}");
        assert!(t3.contains("Appended line"), "{t3}");
    }

    #[test]
    fn xlsx_create_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.xlsx");
        create_xlsx(
            &path,
            "Sales",
            &[
                vec!["Name".into(), "Qty".into()],
                vec!["Widget".into(), "42".into()],
            ],
        )
        .unwrap();
        let t = extract_document(&path).unwrap();
        assert!(t.contains("Widget"), "{t}");
        assert!(t.contains("42"), "{t}");
        assert!(t.contains("Sales") || t.contains("Name"), "{t}");
    }

    #[test]
    fn pptx_create_update_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck.pptx");
        create_pptx(
            &path,
            &[
                SlideSpec {
                    title: Some("Deck Title Here".into()),
                    body: Some("Bullet one".into()),
                },
                SlideSpec {
                    title: Some("Slide Two".into()),
                    body: Some("More content".into()),
                },
            ],
        )
        .unwrap();
        let t = extract_document(&path).unwrap();
        assert!(t.contains("Deck Title Here"), "{t}");
        assert!(t.contains("Slide 1"), "{t}");
        assert!(t.contains("Slide Two"), "{t}");

        update_pptx(
            &path,
            &[SlideSpec {
                title: Some("Only Slide".into()),
                body: Some("Fresh".into()),
            }],
        )
        .unwrap();
        let t2 = extract_document(&path).unwrap();
        assert!(t2.contains("Only Slide"), "{t2}");
        assert!(t2.contains("Fresh"), "{t2}");
        assert!(!t2.contains("Deck Title Here"), "{t2}");
    }

    #[tokio::test]
    async fn office_document_tool_create_then_read_via_extract() {
        use pirs_agent::{AgentTool, ToolExecContext};
        use tokio_util::sync::CancellationToken;

        let dir = tempfile::tempdir().unwrap();
        let tool = OfficeDocumentTool::new(dir.path().to_path_buf());
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t1".into(),
                args: serde_json::json!({
                    "action": "create",
                    "path": "roundtrip.docx",
                    "text": "Viable alternative body"
                }),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("Viable alternative body"), "{text}");
        // Same path via extract_document (what `read` uses).
        let extracted = extract_document(&dir.path().join("roundtrip.docx")).unwrap();
        assert!(extracted.contains("Viable alternative body"), "{extracted}");
    }
}

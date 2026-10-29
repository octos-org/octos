use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use eyre::{Result, bail};
use serde::Deserialize;
use serde_json::Number;

use crate::ooxml::{output_path, validate_xml_text, write_package, xml, zip_text};

const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;
const MAX_ROWS: usize = 1_048_576;
const MAX_COLUMNS: usize = 16_384;

#[derive(Debug, Deserialize)]
pub struct XlsxInput {
    pub output: String,
    pub sheets: Vec<XlsxSheet>,
}

#[derive(Debug, Deserialize)]
pub struct XlsxSheet {
    pub name: String,
    pub rows: Vec<Vec<XlsxCell>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum XlsxCell {
    Value(XlsxValue),
    Options(XlsxCellOptions),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum XlsxValue {
    String(String),
    Number(Number),
    Bool(bool),
}

#[derive(Debug, Deserialize)]
pub struct XlsxCellOptions {
    pub value: Option<XlsxValue>,
    pub formula: Option<String>,
    #[serde(default)]
    pub bold: bool,
    pub fill: Option<String>,
    pub number_format: Option<String>,
}

type StyleKey = (bool, Option<String>, Option<String>);

#[derive(Default)]
struct Styles {
    formats: BTreeSet<String>,
    fills: BTreeSet<String>,
    xfs: BTreeSet<StyleKey>,
}

impl Styles {
    fn new(sheets: &[XlsxSheet]) -> Result<Self> {
        let mut styles = Self::default();
        styles.xfs.insert((false, None, None));
        for cell in sheets.iter().flat_map(|sheet| sheet.rows.iter().flatten()) {
            let key = style_key(cell)?;
            if let Some(fill) = &key.1 {
                styles.fills.insert(fill.clone());
            }
            if let Some(format) = &key.2 {
                styles.formats.insert(format.clone());
            }
            styles.xfs.insert(key);
        }
        Ok(styles)
    }

    fn style_id(&self, key: &StyleKey) -> usize {
        self.xfs
            .iter()
            .position(|candidate| candidate == key)
            .expect("every cell style is registered")
    }

    fn fill_id(&self, fill: &str) -> usize {
        self.fills
            .iter()
            .position(|candidate| candidate == fill)
            .expect("every fill is registered")
            + 2
    }

    fn format_id(&self, format: &str) -> usize {
        self.formats
            .iter()
            .position(|candidate| candidate == format)
            .expect("every number format is registered")
            + 164
    }
}

/// Generate an XLSX package and return its absolute workspace path.
pub fn generate(input: XlsxInput, workspace_root: &Path) -> Result<PathBuf> {
    validate_sheets(&input.sheets)?;
    let styles = Styles::new(&input.sheets)?;
    let final_path = output_path(workspace_root, &input.output, "xlsx")?;
    write_package(&final_path, |zip| {
        zip_text(
            zip,
            "[Content_Types].xml",
            &content_types(input.sheets.len()),
        )?;
        zip_text(zip, "_rels/.rels", &root_relationships())?;
        zip_text(zip, "xl/workbook.xml", &workbook_xml(&input.sheets))?;
        zip_text(
            zip,
            "xl/_rels/workbook.xml.rels",
            &workbook_relationships(input.sheets.len()),
        )?;
        zip_text(zip, "xl/styles.xml", &styles_xml(&styles))?;
        for (index, sheet) in input.sheets.iter().enumerate() {
            zip_text(
                zip,
                &format!("xl/worksheets/sheet{}.xml", index + 1),
                &sheet_xml(sheet, &styles)?,
            )?;
        }
        Ok(())
    })?;
    Ok(final_path)
}

fn validate_sheets(sheets: &[XlsxSheet]) -> Result<()> {
    if sheets.is_empty() {
        bail!("workbook must contain at least one sheet");
    }
    let mut names = BTreeSet::new();
    for sheet in sheets {
        let name = &sheet.name;
        validate_xml_text(name, "sheet name")?;
        if name.trim().is_empty() {
            bail!("sheet name must not be blank");
        }
        if name.chars().count() > 31 {
            bail!("sheet name exceeds Excel's 31-character limit: {name}");
        }
        if name.chars().any(|c| "[]:*?/\\".contains(c))
            || name.starts_with('\'')
            || name.ends_with('\'')
        {
            bail!("sheet name contains characters Excel does not allow: {name}");
        }
        if !names.insert(name.to_lowercase()) {
            bail!("sheet names must be unique (case-insensitive): {name}");
        }
        if sheet.rows.len() > MAX_ROWS {
            bail!("sheet {name} exceeds Excel's row limit");
        }
        for row in &sheet.rows {
            if row.len() > MAX_COLUMNS {
                bail!("sheet {name} exceeds Excel's column limit");
            }
            for cell in row {
                validate_cell(cell)?;
            }
        }
    }
    Ok(())
}

fn validate_cell(cell: &XlsxCell) -> Result<()> {
    let XlsxCell::Options(options) = cell else {
        if let XlsxCell::Value(XlsxValue::String(text)) = cell {
            validate_xml_text(text, "cell value")?;
        }
        return Ok(());
    };
    if options.value.is_some() && options.formula.is_some() {
        bail!("a cell cannot contain both value and formula");
    }
    if let Some(XlsxValue::String(text)) = &options.value {
        validate_xml_text(text, "cell value")?;
    }
    if let Some(formula) = &options.formula {
        validate_xml_text(formula, "formula")?;
        if formula
            .strip_prefix('=')
            .unwrap_or(formula)
            .trim()
            .is_empty()
        {
            bail!("formula must not be blank");
        }
    }
    if let Some(format) = &options.number_format {
        validate_xml_text(format, "number format")?;
    }
    Ok(())
}

fn style_key(cell: &XlsxCell) -> Result<StyleKey> {
    let XlsxCell::Options(options) = cell else {
        return Ok((false, None, None));
    };
    let fill = options
        .fill
        .as_ref()
        .map(|fill| {
            if fill.len() != 6 || !fill.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("fill must be a six-digit RGB value: {fill}");
            }
            Ok(fill.to_ascii_uppercase())
        })
        .transpose()?;
    Ok((options.bold, fill, options.number_format.clone()))
}

fn content_types(sheet_count: usize) -> String {
    let mut out = String::from(XML_DECL);
    out.push_str(
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>"#,
    );
    for index in 1..=sheet_count {
        write!(
            out,
            r#"<Override PartName="/xl/worksheets/sheet{index}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
        )
        .expect("write XML to String");
    }
    out.push_str("</Types>");
    out
}

fn root_relationships() -> String {
    format!(
        r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#
    )
}

fn workbook_xml(sheets: &[XlsxSheet]) -> String {
    let mut out = String::from(XML_DECL);
    out.push_str(
        r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>"#,
    );
    for (index, sheet) in sheets.iter().enumerate() {
        write!(
            out,
            r#"<sheet name="{}" sheetId="{}" r:id="rId{}"/>"#,
            xml(&sheet.name),
            index + 1,
            index + 1
        )
        .expect("write XML to String");
    }
    out.push_str(r#"</sheets><calcPr calcId="0" fullCalcOnLoad="1"/></workbook>"#);
    out
}

fn workbook_relationships(sheet_count: usize) -> String {
    let mut out = String::from(XML_DECL);
    out.push_str(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    for index in 1..=sheet_count {
        write!(
            out,
            r#"<Relationship Id="rId{index}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{index}.xml"/>"#
        )
        .expect("write XML to String");
    }
    write!(
        out,
        r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
        sheet_count + 1
    )
    .expect("write XML to String");
    out
}

fn styles_xml(styles: &Styles) -> String {
    let mut out = String::from(XML_DECL);
    out.push_str(
        r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
    );
    if !styles.formats.is_empty() {
        write!(out, r#"<numFmts count="{}">"#, styles.formats.len()).expect("write XML to String");
        for format in &styles.formats {
            write!(
                out,
                r#"<numFmt numFmtId="{}" formatCode="{}"/>"#,
                styles.format_id(format),
                xml(format)
            )
            .expect("write XML to String");
        }
        out.push_str("</numFmts>");
    }
    out.push_str(
        r#"<fonts count="2"><font><sz val="11"/><color theme="1"/><name val="Calibri"/><family val="2"/></font><font><b/><sz val="11"/><color theme="1"/><name val="Calibri"/><family val="2"/></font></fonts>"#,
    );
    write!(out, r#"<fills count="{}">"#, styles.fills.len() + 2).expect("write XML to String");
    out.push_str(
        r#"<fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill>"#,
    );
    for fill in &styles.fills {
        write!(
            out,
            r#"<fill><patternFill patternType="solid"><fgColor rgb="FF{fill}"/><bgColor indexed="64"/></patternFill></fill>"#
        )
        .expect("write XML to String");
    }
    out.push_str(
        r#"</fills><borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>"#,
    );
    write!(out, r#"<cellXfs count="{}">"#, styles.xfs.len()).expect("write XML to String");
    for key in &styles.xfs {
        let number_format = key
            .2
            .as_ref()
            .map(|format| styles.format_id(format))
            .unwrap_or(0);
        let fill = key.1.as_ref().map(|fill| styles.fill_id(fill)).unwrap_or(0);
        write!(
            out,
            r#"<xf numFmtId="{number_format}" fontId="{}" fillId="{fill}" borderId="0" xfId="0" applyFont="1" applyFill="1" applyNumberFormat="1"/>"#,
            usize::from(key.0)
        )
        .expect("write XML to String");
    }
    out.push_str(
        r#"</cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles><dxfs count="0"/><tableStyles count="0" defaultTableStyle="TableStyleMedium2" defaultPivotStyle="PivotStyleLight16"/></styleSheet>"#,
    );
    out
}

fn sheet_xml(sheet: &XlsxSheet, styles: &Styles) -> Result<String> {
    let mut out = String::from(XML_DECL);
    out.push_str(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
    );
    for (row_index, row) in sheet.rows.iter().enumerate() {
        write!(out, r#"<row r="{}">"#, row_index + 1).expect("write XML to String");
        for (column_index, cell) in row.iter().enumerate() {
            let reference = format!("{}{}", column_name(column_index + 1), row_index + 1);
            write!(out, r#"<c r="{reference}""#).expect("write XML to String");
            let style = styles.style_id(&style_key(cell)?);
            if style != 0 {
                write!(out, r#" s="{style}""#).expect("write XML to String");
            }
            match cell {
                XlsxCell::Value(value) => write_value(&mut out, value),
                XlsxCell::Options(options) => {
                    if let Some(formula) = &options.formula {
                        write!(
                            out,
                            "><f>{}</f></c>",
                            xml(formula.strip_prefix('=').unwrap_or(formula))
                        )
                        .expect("write XML to String");
                    } else if let Some(value) = &options.value {
                        write_value(&mut out, value);
                    } else {
                        out.push_str("/>");
                    }
                }
            }
        }
        out.push_str("</row>");
    }
    out.push_str("</sheetData></worksheet>");
    Ok(out)
}

fn write_value(out: &mut String, value: &XlsxValue) {
    match value {
        XlsxValue::String(text) => {
            out.push_str(r#" t="inlineStr"><is><t xml:space="preserve">"#);
            out.push_str(&xml(text));
            out.push_str("</t></is></c>");
        }
        XlsxValue::Number(number) => {
            write!(out, "><v>{number}</v></c>").expect("write XML to String");
        }
        XlsxValue::Bool(value) => {
            write!(out, r#" t="b"><v>{}</v></c>"#, u8::from(*value)).expect("write XML to String");
        }
    }
}

fn column_name(mut column: usize) -> String {
    let mut name = Vec::new();
    while column > 0 {
        column -= 1;
        name.push((b'A' + (column % 26) as u8) as char);
        column /= 26;
    }
    name.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Read;
    use std::path::Path;

    use serde_json::json;
    use zip::ZipArchive;

    use super::*;
    use crate::ooxml::assert_well_formed_xml;

    fn member(path: &Path, name: &str) -> String {
        let file = File::open(path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut text = String::new();
        archive
            .by_name(name)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    #[test]
    fn should_write_multiple_sheets_and_primitive_cells_when_generating() {
        let temp = tempfile::tempdir().unwrap();
        let input: XlsxInput = serde_json::from_value(json!({
            "output": "nested/book.xlsx",
            "sheets": [
                {"name": "Summary", "rows": [["East", 120, true, " <&> "]]},
                {"name": "Notes", "rows": [["Values are in USD."]]}
            ]
        }))
        .unwrap();

        let path = generate(input, temp.path()).unwrap();
        assert!(path.is_absolute());
        assert!(path.starts_with(temp.path()));
        assert!(path.ends_with("nested/book.xlsx"));

        let workbook = member(&path, "xl/workbook.xml");
        assert!(workbook.contains(r#"<sheet name="Summary" sheetId="1" r:id="rId1"/>"#));
        assert!(workbook.contains(r#"<sheet name="Notes" sheetId="2" r:id="rId2"/>"#));
        let first = member(&path, "xl/worksheets/sheet1.xml");
        assert!(
            first.contains(
                r#"<c r="A1" t="inlineStr"><is><t xml:space="preserve">East</t></is></c>"#
            )
        );
        assert!(first.contains(r#"<c r="B1"><v>120</v></c>"#));
        assert!(first.contains(r#"<c r="C1" t="b"><v>1</v></c>"#));
        assert!(first.contains(" &lt;&amp;&gt; "));
        let second = member(&path, "xl/worksheets/sheet2.xml");
        assert!(second.contains("Values are in USD."));
        let content_types = member(&path, "[Content_Types].xml");
        assert!(content_types.contains("/xl/worksheets/sheet1.xml"));
        assert!(content_types.contains("/xl/worksheets/sheet2.xml"));

        for name in [
            "[Content_Types].xml",
            "_rels/.rels",
            "xl/workbook.xml",
            "xl/_rels/workbook.xml.rels",
            "xl/styles.xml",
            "xl/worksheets/sheet1.xml",
            "xl/worksheets/sheet2.xml",
        ] {
            assert_well_formed_xml(&member(&path, name));
        }
    }

    #[test]
    fn should_write_formulas_and_styles_when_cells_are_formatted() {
        let temp = tempfile::tempdir().unwrap();
        let input: XlsxInput = serde_json::from_value(json!({
            "output": "styled.xlsx",
            "sheets": [{"name": "Summary", "rows": [
                [
                    {"value": "Region", "bold": true, "fill": "ddebF7"},
                    {"value": "Sales", "bold": true, "fill": "DDEBF7"}
                ],
                ["East", 120],
                ["West", 140],
                [{"value": "Total", "bold": true}, {"formula": "=SUM(B2:B3)", "number_format": "#,##0.00"}]
            ]}]
        }))
        .unwrap();

        let path = generate(input, temp.path()).unwrap();
        let sheet = member(&path, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains(r#"<c r="B4" s="1"><f>SUM(B2:B3)</f></c>"#));
        assert!(!sheet.contains("<f>="));
        let workbook = member(&path, "xl/workbook.xml");
        assert!(workbook.contains(r#"fullCalcOnLoad="1""#));
        let styles = member(&path, "xl/styles.xml");
        assert_well_formed_xml(&styles);
        assert!(styles.contains(r##"<numFmt numFmtId="164" formatCode="#,##0.00"/>"##));
        assert!(styles.contains("<b/>"));
        assert!(styles.contains(r#"<fgColor rgb="FFDDEBF7"/>"#));
        assert_eq!(styles.matches("FFDDEBF7").count(), 1);
    }

    #[test]
    fn should_reject_output_outside_workspace_when_generating() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().parent().unwrap().join(format!(
            "{}-escape.xlsx",
            temp.path().file_name().unwrap().to_string_lossy()
        ));
        assert!(!outside.exists());
        let input: XlsxInput = serde_json::from_value(json!({
            "output": format!("../{}", outside.file_name().unwrap().to_string_lossy()),
            "sheets": [{"name": "Sheet1", "rows": [["safe"]]}]
        }))
        .unwrap();

        let error = generate(input, temp.path()).unwrap_err();
        assert!(error.to_string().contains("workspace"));
        assert!(!outside.exists());
    }
}

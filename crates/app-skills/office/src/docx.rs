use std::path::{Path, PathBuf};

use eyre::{Context, Result, bail};
use serde::Deserialize;

use crate::ooxml::{
    output_path, read_workspace_file, validate_xml_text, write_package, xml, zip_bytes, zip_text,
};

#[derive(Debug, Deserialize)]
pub struct DocxInput {
    pub output: String,
    pub title: String,
    #[serde(default)]
    pub paragraphs: Vec<String>,
    #[serde(default)]
    pub tables: Vec<DocxTable>,
    #[serde(default)]
    pub images: Vec<DocxImage>,
}

#[derive(Debug, Deserialize)]
pub struct DocxTable {
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct DocxImage {
    pub path: String,
    #[serde(default = "default_image_width")]
    pub width_inches: f64,
}

fn default_image_width() -> f64 {
    6.0
}

struct ImagePart {
    member: String,
    relationship: String,
    bytes: Vec<u8>,
    width_emu: u32,
    height_emu: u32,
}

pub fn generate(input: DocxInput, workspace_root: &Path) -> Result<PathBuf> {
    if input.title.trim().is_empty() {
        bail!("DOCX title must not be blank");
    }
    validate_xml_text(&input.title, "DOCX title")?;
    for paragraph in &input.paragraphs {
        validate_xml_text(paragraph, "DOCX paragraph")?;
    }
    for table in &input.tables {
        for row in &table.rows {
            for cell in row {
                validate_xml_text(cell, "DOCX table cell")?;
            }
        }
    }
    let output = output_path(workspace_root, &input.output, "docx")?;
    let mut images = Vec::new();
    for (index, image) in input.images.iter().enumerate() {
        if !image.width_inches.is_finite() || image.width_inches <= 0.0 || image.width_inches > 20.0
        {
            bail!("DOCX image width_inches must be in (0, 20]");
        }
        let (path, bytes) = read_workspace_file(workspace_root, &image.path)?;
        let reader = image::ImageReader::open(&path)
            .wrap_err_with(|| format!("open image {}", path.display()))?
            .with_guessed_format()
            .wrap_err("detect image format")?;
        let format = reader.format();
        let (pixel_width, pixel_height) = reader.into_dimensions().wrap_err("read image size")?;
        if pixel_width == 0 || pixel_height == 0 {
            bail!("DOCX image has zero dimensions: {}", image.path);
        }
        let extension = match format {
            Some(image::ImageFormat::Png) => "png",
            Some(image::ImageFormat::Jpeg) => "jpg",
            _ => {
                bail!("DOCX images must be PNG or JPEG: {}", image.path);
            }
        };
        let width_emu = (image.width_inches * 914_400.0).round() as u32;
        let height_emu =
            u32::try_from((width_emu as u64 * pixel_height as u64 / pixel_width as u64).max(1))
                .wrap_err("DOCX image height exceeds the OOXML extent limit")?;
        images.push(ImagePart {
            member: format!("word/media/image{}.{}", index + 1, extension),
            relationship: format!("rId{}", index + 2),
            bytes,
            width_emu,
            height_emu,
        });
    }

    write_package(&output, |zip| {
        let mut overrides = String::new();
        for (part, content_type) in [
            (
                "/word/document.xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
            ),
            (
                "/word/styles.xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml",
            ),
            (
                "/docProps/core.xml",
                "application/vnd.openxmlformats-package.core-properties+xml",
            ),
            (
                "/docProps/app.xml",
                "application/vnd.openxmlformats-officedocument.extended-properties+xml",
            ),
        ] {
            overrides.push_str(&format!(
                "<Override PartName=\"{part}\" ContentType=\"{content_type}\"/>"
            ));
        }
        zip_text(
            zip,
            "[Content_Types].xml",
            &format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Default Extension=\"png\" ContentType=\"image/png\"/><Default Extension=\"jpg\" ContentType=\"image/jpeg\"/><Default Extension=\"jpeg\" ContentType=\"image/jpeg\"/>{overrides}</Types>"
            ),
        )?;
        zip_text(
            zip,
            "_rels/.rels",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/><Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/><Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\"/></Relationships>",
        )?;
        zip_text(
            zip,
            "docProps/core.xml",
            &format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>{}</dc:title></cp:coreProperties>",
                xml(&input.title)
            ),
        )?;
        zip_text(
            zip,
            "docProps/app.xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\"><Application>octos office</Application></Properties>",
        )?;
        zip_text(zip, "word/styles.xml", STYLES_XML)?;

        let mut body = String::new();
        body.push_str(&format!("<w:p><w:pPr><w:pStyle w:val=\"Title\"/></w:pPr><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>", xml(&input.title)));
        for paragraph in &input.paragraphs {
            body.push_str(&format!(
                "<w:p><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                xml(paragraph)
            ));
        }
        for table in &input.tables {
            body.push_str("<w:tbl><w:tblPr><w:tblW w:w=\"0\" w:type=\"auto\"/><w:tblBorders><w:top w:val=\"single\" w:sz=\"4\"/><w:left w:val=\"single\" w:sz=\"4\"/><w:bottom w:val=\"single\" w:sz=\"4\"/><w:right w:val=\"single\" w:sz=\"4\"/><w:insideH w:val=\"single\" w:sz=\"4\"/><w:insideV w:val=\"single\" w:sz=\"4\"/></w:tblBorders></w:tblPr>");
            for row in &table.rows {
                body.push_str("<w:tr>");
                for cell in row {
                    body.push_str(&format!("<w:tc><w:tcPr><w:tcW w:w=\"0\" w:type=\"auto\"/></w:tcPr><w:p><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p></w:tc>", xml(cell)));
                }
                body.push_str("</w:tr>");
            }
            body.push_str("</w:tbl>");
        }
        for (index, image) in images.iter().enumerate() {
            let id = index + 1;
            body.push_str(&format!("<w:p><w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\"><wp:extent cx=\"{}\" cy=\"{}\"/><wp:docPr id=\"{id}\" name=\"Picture {id}\"/><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:pic><pic:nvPicPr><pic:cNvPr id=\"{id}\" name=\"image{id}\"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed=\"{}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{}\" cy=\"{}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>", image.width_emu, image.height_emu, image.relationship, image.width_emu, image.height_emu));
        }
        body.push_str("<w:sectPr><w:pgSz w:w=\"12240\" w:h=\"15840\"/><w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\"/></w:sectPr>");
        zip_text(
            zip,
            "word/document.xml",
            &format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><w:body>{body}</w:body></w:document>"
            ),
        )?;

        let mut relationships = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
        );
        for image in &images {
            relationships.push_str(&format!("<Relationship Id=\"{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"media/{}\"/>", image.relationship, image.member.trim_start_matches("word/media/")));
        }
        relationships.push_str("</Relationships>");
        zip_text(zip, "word/_rels/document.xml.rels", &relationships)?;
        for image in &images {
            zip_bytes(zip, &image.member, &image.bytes)?;
        }
        Ok(())
    })?;
    Ok(output)
}

const STYLES_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:qFormat/></w:style><w:style w:type=\"paragraph\" w:styleId=\"Title\"><w:name w:val=\"Title\"/><w:basedOn w:val=\"Normal\"/><w:qFormat/><w:rPr><w:b/><w:sz w:val=\"56\"/></w:rPr></w:style></w:styles>";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ooxml::assert_well_formed_xml;
    use std::io::Read;

    fn part(package: &Path, name: &str) -> Vec<u8> {
        let file = std::fs::File::open(package).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        let mut part = zip.by_name(name).unwrap();
        let mut bytes = Vec::new();
        part.read_to_end(&mut bytes).unwrap();
        bytes
    }

    fn text(package: &Path, name: &str) -> String {
        let value = String::from_utf8(part(package, name)).unwrap();
        assert_well_formed_xml(&value);
        value
    }

    #[test]
    fn should_generate_title_paragraph_table_and_image_in_docx() {
        let temp = tempfile::tempdir().unwrap();
        let image_path = temp.path().join("chart.png");
        image::RgbImage::from_pixel(2, 1, image::Rgb([0, 128, 255]))
            .save(&image_path)
            .unwrap();
        let input: DocxInput = serde_json::from_value(serde_json::json!({
            "output": "reports/q3.docx",
            "title": "Q3 <summary>",
            "paragraphs": ["Revenue increased.", "A & B"],
            "tables": [{"rows": [["Region", "Sales"], ["East", "120"]]}],
            "images": [{"path": "chart.png", "width_inches": 2.0}]
        }))
        .unwrap();

        let output = generate(input, temp.path()).unwrap();
        assert!(output.ends_with("reports/q3.docx"));
        let document = text(&output, "word/document.xml");
        assert!(document.contains("Q3 &lt;summary&gt;"));
        assert!(document.contains("Revenue increased."));
        assert!(document.contains("A &amp; B"));
        assert!(document.contains("<w:tbl>"));
        assert!(document.contains("r:embed=\"rId2\""));
        assert!(document.contains("cx=\"1828800\""));
        assert!(text(&output, "word/styles.xml").contains("w:styleId=\"Title\""));
        assert!(
            text(&output, "word/_rels/document.xml.rels").contains("Target=\"media/image1.png\"")
        );
        assert_eq!(
            part(&output, "word/media/image1.png"),
            std::fs::read(image_path).unwrap()
        );
    }

    #[test]
    fn should_refuse_workspace_escape_and_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let input: DocxInput = serde_json::from_value(serde_json::json!({
            "output": "report.docx", "title": "Report"
        }))
        .unwrap();
        generate(input, temp.path()).unwrap();
        let duplicate: DocxInput = serde_json::from_value(serde_json::json!({
            "output": "report.docx", "title": "Replacement"
        }))
        .unwrap();
        assert!(generate(duplicate, temp.path()).is_err());
        let escape: DocxInput = serde_json::from_value(serde_json::json!({
            "output": "../escape.docx", "title": "Escape"
        }))
        .unwrap();
        assert!(generate(escape, temp.path()).is_err());
    }

    #[test]
    fn should_reject_xml_forbidden_control_characters() {
        let temp = tempfile::tempdir().unwrap();
        let input: DocxInput = serde_json::from_value(serde_json::json!({
            "output": "invalid.docx", "title": "bad\u{0001}title"
        }))
        .unwrap();
        let error = generate(input, temp.path()).unwrap_err();
        assert!(error.to_string().contains("not allowed in XML"));
        assert!(!temp.path().join("invalid.docx").exists());
    }
}

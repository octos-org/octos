use std::path::{Path, PathBuf};

use eyre::{Context, Result, bail};
use serde::Deserialize;

use crate::ooxml::{output_path, read_workspace_file, write_package, xml, zip_bytes, zip_text};

const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;
const EMU_PER_INCH: f64 = 914_400.0;
const SLIDE_WIDTH: u64 = 12_192_000;
const SLIDE_HEIGHT: u64 = 6_858_000;
const MARGIN: u64 = 502_920;
const GAP: u64 = 320_040;
const TITLE_Y: u64 = 320_040;
const TITLE_HEIGHT: u64 = 685_800;
const BODY_Y: u64 = 1_234_440;
const BODY_HEIGHT: u64 = 5_074_920;
const TEXT_WIDTH: u64 = SLIDE_WIDTH - 2 * MARGIN;
const MAX_IMAGE_WIDTH_INCHES: f64 = 5.5;

#[derive(Debug, Deserialize)]
pub struct PptxInput {
    pub output: String,
    pub slides: Vec<PptxSlide>,
}

#[derive(Debug, Deserialize)]
pub struct PptxSlide {
    pub title: String,
    #[serde(default)]
    pub bullets: Vec<String>,
    pub image: Option<PptxImage>,
}

#[derive(Debug, Deserialize)]
pub struct PptxImage {
    pub path: String,
    #[serde(default = "default_width")]
    pub width_inches: f64,
}

fn default_width() -> f64 {
    4.0
}

struct ImagePart {
    member: String,
    bytes: Vec<u8>,
    width: u64,
    height: u64,
}

/// Generate a 16:9 PPTX with editable title and bullet text on every slide.
pub fn generate(input: PptxInput, workspace_root: &Path) -> Result<PathBuf> {
    if input.slides.is_empty() {
        bail!("presentation must contain at least one slide");
    }
    for slide in &input.slides {
        if slide.title.trim().is_empty() {
            bail!("PPTX slide title must not be blank");
        }
        validate_xml_text(&slide.title, "slide title")?;
        for bullet in &slide.bullets {
            validate_xml_text(bullet, "bullet")?;
        }
    }

    let final_path = output_path(workspace_root, &input.output, "pptx")?;
    let mut images = Vec::with_capacity(input.slides.len());
    for (index, slide) in input.slides.iter().enumerate() {
        images.push(match &slide.image {
            Some(image) => Some(prepare_image(image, index, workspace_root)?),
            None => None,
        });
    }

    write_package(&final_path, |zip| {
        zip_text(
            zip,
            "[Content_Types].xml",
            &content_types(input.slides.len()),
        )?;
        zip_text(zip, "_rels/.rels", ROOT_RELATIONSHIPS)?;
        zip_text(zip, "docProps/core.xml", CORE_PROPERTIES)?;
        zip_text(
            zip,
            "docProps/app.xml",
            &format!(
                r#"{XML_DECL}<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>octos office</Application><PresentationFormat>On-screen Show (16:9)</PresentationFormat><Slides>{}</Slides></Properties>"#,
                input.slides.len()
            ),
        )?;
        zip_text(
            zip,
            "ppt/presentation.xml",
            &presentation_xml(input.slides.len()),
        )?;
        zip_text(
            zip,
            "ppt/_rels/presentation.xml.rels",
            &presentation_relationships(input.slides.len()),
        )?;
        zip_text(zip, "ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER)?;
        zip_text(
            zip,
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            SLIDE_MASTER_RELATIONSHIPS,
        )?;
        zip_text(zip, "ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT)?;
        zip_text(
            zip,
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            SLIDE_LAYOUT_RELATIONSHIPS,
        )?;
        zip_text(zip, "ppt/theme/theme1.xml", THEME)?;

        for (index, (slide, image)) in input.slides.iter().zip(&images).enumerate() {
            let number = index + 1;
            zip_text(
                zip,
                &format!("ppt/slides/slide{number}.xml"),
                &slide_xml(slide, image.as_ref()),
            )?;
            zip_text(
                zip,
                &format!("ppt/slides/_rels/slide{number}.xml.rels"),
                &slide_relationships(image.as_ref()),
            )?;
            if let Some(image) = image {
                zip_bytes(zip, &image.member, &image.bytes)?;
            }
        }
        Ok(())
    })?;
    Ok(final_path)
}

fn prepare_image(image: &PptxImage, index: usize, root: &Path) -> Result<ImagePart> {
    if !image.width_inches.is_finite()
        || image.width_inches <= 0.0
        || image.width_inches > MAX_IMAGE_WIDTH_INCHES
    {
        bail!("PPTX image width_inches must be in (0, {MAX_IMAGE_WIDTH_INCHES}]");
    }
    let (path, bytes) = read_workspace_file(root, &image.path)?;
    let reader = image::ImageReader::open(&path)
        .wrap_err_with(|| format!("open image {}", path.display()))?
        .with_guessed_format()
        .wrap_err("detect image format")?;
    let format = reader.format();
    let (pixel_width, pixel_height) = reader.into_dimensions().wrap_err("read image size")?;
    if pixel_width == 0 || pixel_height == 0 {
        bail!("PPTX image has zero dimensions: {}", image.path);
    }
    let extension = match format {
        Some(image::ImageFormat::Png) => "png",
        Some(image::ImageFormat::Jpeg) => "jpg",
        _ => {
            bail!("PPTX images must be PNG or JPEG: {}", image.path);
        }
    };

    let requested_width = (image.width_inches * EMU_PER_INCH).round() as u64;
    if requested_width == 0 {
        bail!("PPTX image width_inches is too small");
    }
    let mut width = requested_width;
    let mut height = (width * pixel_height as u64 / pixel_width as u64).max(1);
    if height > BODY_HEIGHT {
        width = (width * BODY_HEIGHT / height).max(1);
        height = BODY_HEIGHT;
    }
    Ok(ImagePart {
        member: format!("ppt/media/image{}.{}", index + 1, extension),
        bytes,
        width,
        height,
    })
}

fn content_types(slide_count: usize) -> String {
    let mut overrides = String::new();
    for (part, content_type) in [
        (
            "/ppt/presentation.xml",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        ),
        (
            "/ppt/slideMasters/slideMaster1.xml",
            "application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml",
        ),
        (
            "/ppt/slideLayouts/slideLayout1.xml",
            "application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml",
        ),
        (
            "/ppt/theme/theme1.xml",
            "application/vnd.openxmlformats-officedocument.theme+xml",
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
            r#"<Override PartName="{part}" ContentType="{content_type}"/>"#
        ));
    }
    for number in 1..=slide_count {
        overrides.push_str(&format!(r#"<Override PartName="/ppt/slides/slide{number}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#));
    }
    format!(
        r#"{XML_DECL}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Default Extension="jpg" ContentType="image/jpeg"/><Default Extension="jpeg" ContentType="image/jpeg"/>{overrides}</Types>"#
    )
}

fn presentation_xml(slide_count: usize) -> String {
    let mut slide_ids = String::new();
    for index in 0..slide_count {
        slide_ids.push_str(&format!(
            r#"<p:sldId id="{}" r:id="rId{}"/>"#,
            256 + index,
            index + 2
        ));
    }
    format!(
        r#"{XML_DECL}<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" saveSubsetFonts="1"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst>{slide_ids}</p:sldIdLst><p:sldSz cx="{SLIDE_WIDTH}" cy="{SLIDE_HEIGHT}"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#
    )
}

fn presentation_relationships(slide_count: usize) -> String {
    let mut relationships = String::from(
        r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>"#,
    );
    for number in 1..=slide_count {
        relationships.push_str(&format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{number}.xml"/>"#, number + 1));
    }
    format!(
        r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{relationships}</Relationships>"#
    )
}

fn slide_xml(slide: &PptxSlide, image: Option<&ImagePart>) -> String {
    let text_width = image.map_or(TEXT_WIDTH, |image| {
        SLIDE_WIDTH - MARGIN - image.width - GAP - MARGIN
    });
    let mut shapes = title_shape(&slide.title);
    shapes.push_str(&bullets_shape(&slide.bullets, text_width));
    if let Some(image) = image {
        let x = SLIDE_WIDTH - MARGIN - image.width;
        let y = BODY_Y + (BODY_HEIGHT - image.height) / 2;
        shapes.push_str(&format!(
            r#"<p:pic><p:nvPicPr><p:cNvPr id="4" name="Picture"/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{}" cy="{}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>"#,
            image.width, image.height
        ));
    }
    format!(
        r#"{XML_DECL}<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>{shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#
    )
}

fn title_shape(title: &str) -> String {
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr>{}<p:txBody><a:bodyPr wrap="square" rtlCol="0" anchor="t"><a:normAutofit/></a:bodyPr><a:lstStyle/><a:p><a:pPr marL="0" indent="0"><a:buNone/></a:pPr><a:r><a:rPr lang="en-US" sz="3200" b="1" dirty="0"/><a:t>{}</a:t></a:r><a:endParaRPr lang="en-US" sz="3200"/></a:p></p:txBody></p:sp>"#,
        shape_properties(MARGIN, TITLE_Y, TEXT_WIDTH, TITLE_HEIGHT),
        xml(title)
    )
}

fn bullets_shape(bullets: &[String], width: u64) -> String {
    let mut paragraphs = String::new();
    for bullet in bullets {
        paragraphs.push_str(&format!(
            r#"<a:p><a:pPr marL="228600" indent="-228600"><a:buFont typeface="Arial"/><a:buChar char="&#x2022;"/></a:pPr><a:r><a:rPr lang="en-US" sz="2000" dirty="0"/><a:t>{}</a:t></a:r><a:endParaRPr lang="en-US" sz="2000"/></a:p>"#,
            xml(bullet)
        ));
    }
    if paragraphs.is_empty() {
        paragraphs.push_str("<a:p/>");
    }
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="3" name="Bullets"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr>{}<p:txBody><a:bodyPr wrap="square" rtlCol="0" anchor="t"><a:normAutofit/></a:bodyPr><a:lstStyle/>{paragraphs}</p:txBody></p:sp>"#,
        shape_properties(MARGIN, BODY_Y, width, BODY_HEIGHT)
    )
}

fn shape_properties(x: u64, y: u64, width: u64, height: u64) -> String {
    format!(
        r#"<p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{width}" cy="{height}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/><a:ln><a:noFill/></a:ln></p:spPr>"#
    )
}

fn slide_relationships(image: Option<&ImagePart>) -> String {
    let mut relationships = String::from(
        r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>"#,
    );
    if let Some(image) = image {
        relationships.push_str(&format!(r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/{}"/>"#, image.member.trim_start_matches("ppt/media/")));
    }
    format!(
        r#"{XML_DECL}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{relationships}</Relationships>"#
    )
}

fn validate_xml_text(value: &str, label: &str) -> Result<()> {
    if value.chars().any(|c| {
        matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}')
    }) {
        bail!("{label} contains characters not allowed in XML");
    }
    Ok(())
}

const ROOT_RELATIONSHIPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/></Relationships>"#;
const CORE_PROPERTIES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Presentation</dc:title><dc:creator>octos</dc:creator></cp:coreProperties>"#;
const SLIDE_MASTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:bg><p:bgPr><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill><a:effectLst/></p:bgPr></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle/><p:bodyStyle/><p:otherStyle/></p:txStyles></p:sldMaster>"#;
const SLIDE_MASTER_RELATIONSHIPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/></Relationships>"#;
const SLIDE_LAYOUT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#;
const SLIDE_LAYOUT_RELATIONSHIPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#;
const THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office Theme"><a:themeElements><a:clrScheme name="Office"><a:dk1><a:srgbClr val="000000"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="1F497D"/></a:dk2><a:lt2><a:srgbClr val="EEECE1"/></a:lt2><a:accent1><a:srgbClr val="4F81BD"/></a:accent1><a:accent2><a:srgbClr val="C0504D"/></a:accent2><a:accent3><a:srgbClr val="9BBB59"/></a:accent3><a:accent4><a:srgbClr val="8064A2"/></a:accent4><a:accent5><a:srgbClr val="4BACC6"/></a:accent5><a:accent6><a:srgbClr val="F79646"/></a:accent6><a:hlink><a:srgbClr val="0000FF"/></a:hlink><a:folHlink><a:srgbClr val="800080"/></a:folHlink></a:clrScheme><a:fontScheme name="Office"><a:majorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Office"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>"#;

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
        String::from_utf8(part(package, name)).unwrap()
    }

    fn assert_all_xml_well_formed(package: &Path) {
        let file = std::fs::File::open(package).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        for index in 0..zip.len() {
            let mut part = zip.by_index(index).unwrap();
            if part.name().ends_with(".xml") || part.name().ends_with(".rels") {
                let mut value = String::new();
                part.read_to_string(&mut value).unwrap();
                assert_well_formed_xml(&value);
            }
        }
    }

    fn one_slide(output: &str) -> PptxInput {
        PptxInput {
            output: output.to_owned(),
            slides: vec![PptxSlide {
                title: "Title".to_owned(),
                bullets: vec![],
                image: None,
            }],
        }
    }

    #[test]
    fn should_generate_two_slides_with_bullets_image_and_relationships() {
        let temp = tempfile::tempdir().unwrap();
        let image_path = temp.path().join("chart.png");
        image::RgbImage::from_pixel(4, 2, image::Rgb([0, 128, 255]))
            .save(&image_path)
            .unwrap();
        let image_bytes = std::fs::read(&image_path).unwrap();
        let input: PptxInput = serde_json::from_value(serde_json::json!({
            "output": "decks/q3.pptx",
            "slides": [
                {
                    "title": "Q3 <Results> & Plan",
                    "bullets": ["Revenue & growth", "Second <point>"],
                    "image": {"path": "chart.png"}
                },
                {"title": "Next steps", "bullets": ["Ship & learn"]}
            ]
        }))
        .unwrap();

        let output = generate(input, temp.path()).unwrap();
        assert_eq!(output, temp.path().join("decks/q3.pptx"));
        assert_all_xml_well_formed(&output);

        let presentation = text(&output, "ppt/presentation.xml");
        assert!(presentation.contains(r#"<p:sldId id="256" r:id="rId2"/>"#));
        assert!(presentation.contains(r#"<p:sldId id="257" r:id="rId3"/>"#));
        assert!(presentation.contains(r#"<p:sldSz cx="12192000" cy="6858000"/>"#));
        assert!(presentation.contains(r#"<p:notesSz cx="6858000" cy="9144000"/>"#));

        let first = text(&output, "ppt/slides/slide1.xml");
        assert!(first.contains("Q3 &lt;Results&gt; &amp; Plan"));
        assert!(first.contains("Revenue &amp; growth"));
        assert!(first.contains("Second &lt;point&gt;"));
        assert_eq!(first.matches("<a:buChar").count(), 2);
        assert!(first.contains(r#"<a:blip r:embed="rId2"/>"#));
        assert!(first.contains(r#"<a:ext cx="3657600" cy="1828800"/>"#));

        let second = text(&output, "ppt/slides/slide2.xml");
        assert!(second.contains("Next steps"));
        assert!(second.contains("Ship &amp; learn"));
        assert!(!second.contains("<p:pic>"));

        let first_rels = text(&output, "ppt/slides/_rels/slide1.xml.rels");
        assert!(first_rels.contains("slideLayout1.xml"));
        assert!(first_rels.contains(r#"Target="../media/image1.png""#));
        let second_rels = text(&output, "ppt/slides/_rels/slide2.xml.rels");
        assert!(second_rels.contains("slideLayout1.xml"));
        assert!(!second_rels.contains("/image"));
        assert_eq!(part(&output, "ppt/media/image1.png"), image_bytes);
    }

    #[test]
    fn should_refuse_workspace_escape_for_output_and_image() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();

        let error = generate(one_slide("../escape.pptx"), &workspace).unwrap_err();
        assert!(error.to_string().contains("inside the session workspace"));
        assert!(!temp.path().join("escape.pptx").exists());

        image::RgbImage::from_pixel(1, 1, image::Rgb([0, 0, 0]))
            .save(temp.path().join("outside.png"))
            .unwrap();
        let mut input = one_slide("deck.pptx");
        input.slides[0].image = Some(PptxImage {
            path: "../outside.png".to_owned(),
            width_inches: default_width(),
        });
        let error = generate(input, &workspace).unwrap_err();
        assert!(error.to_string().contains("inside the session workspace"));
        assert!(!workspace.join("deck.pptx").exists());
    }

    #[test]
    fn should_require_at_least_one_slide() {
        let temp = tempfile::tempdir().unwrap();
        let input = PptxInput {
            output: "empty.pptx".to_owned(),
            slides: vec![],
        };
        let error = generate(input, temp.path()).unwrap_err();
        assert!(error.to_string().contains("at least one slide"));
    }
}

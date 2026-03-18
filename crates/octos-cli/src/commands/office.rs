//! Office file manipulation commands (DOCX/PPTX/XLSX).
//!
//! Replaces Python scripts (unpack.py, pack.py, clean.py, add_slide.py, markitdown)
//! with native Rust implementations using `zip` + `quick-xml`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::{Result, WrapErr, bail};

use super::Executable;

/// Office file manipulation tools.
#[derive(Debug, Args)]
pub struct OfficeCommand {
    #[command(subcommand)]
    pub action: OfficeAction,
}

#[derive(Debug, Subcommand)]
pub enum OfficeAction {
    /// Extract text from Office files as Markdown.
    Extract {
        /// Path to Office file (.pptx, .docx, .xlsx)
        file: PathBuf,
    },
    /// Unpack Office file into directory with pretty-printed XML.
    Unpack {
        /// Office file to unpack
        file: PathBuf,
        /// Output directory
        output: PathBuf,
    },
    /// Pack directory into Office file.
    Pack {
        /// Unpacked directory
        input: PathBuf,
        /// Output Office file (.pptx, .docx, .xlsx)
        output: PathBuf,
    },
    /// Remove orphaned files from unpacked PPTX.
    Clean {
        /// Unpacked PPTX directory
        dir: PathBuf,
    },
    /// Add slide by duplicating existing or creating from layout.
    AddSlide {
        /// Unpacked PPTX directory
        dir: PathBuf,
        /// Source: slideN.xml (duplicate) or slideLayoutN.xml (from layout)
        source: String,
    },
    /// Validate Office document XML (basic checks).
    Validate {
        /// Path to Office file or unpacked directory
        path: PathBuf,
        /// Automatically repair common issues
        #[arg(long)]
        auto_repair: bool,
    },
    /// Create slide thumbnail grid from PPTX (requires soffice + pdftoppm).
    Thumbnail {
        /// PPTX file
        file: PathBuf,
        /// Output filename prefix (default: thumbnails)
        #[arg(default_value = "thumbnails")]
        output_prefix: String,
        /// Number of columns in the grid (max 6)
        #[arg(long, default_value = "3")]
        cols: u32,
    },
    /// Add comment to unpacked DOCX document.
    Comment {
        /// Unpacked DOCX directory
        dir: PathBuf,
        /// Comment ID (unique integer)
        id: u32,
        /// Comment text (pre-escaped XML)
        text: String,
        /// Author name
        #[arg(long, default_value = "Claude")]
        author: String,
        /// Author initials
        #[arg(long, default_value = "C")]
        initials: String,
        /// Parent comment ID for replies
        #[arg(long)]
        parent: Option<u32>,
    },
    /// Accept all tracked changes in a DOCX (requires soffice).
    AcceptChanges {
        /// Input DOCX with tracked changes
        input: PathBuf,
        /// Output DOCX (clean)
        output: PathBuf,
    },
    /// Recalculate Excel formulas and check for errors (requires soffice).
    Recalc {
        /// Excel file (.xlsx)
        file: PathBuf,
        /// Timeout in seconds for LibreOffice
        #[arg(long, default_value = "30")]
        timeout: u32,
    },
    /// Run LibreOffice with sandbox-safe environment.
    Soffice {
        /// Arguments to pass to soffice
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Overlay text on an image file (PNG/JPEG) as burned pixels.
    OverlayText {
        /// Input image file
        image: PathBuf,
        /// Text to overlay
        text: String,
        /// X position (pixels from left)
        #[arg(long, default_value = "40")]
        x: u32,
        /// Y position (pixels from top)
        #[arg(long, default_value = "40")]
        y: u32,
        /// Scale factor (1=5x7 base, 8=40x56 per char)
        #[arg(long, default_value = "8")]
        scale: u32,
        /// Text color as R,G,B (0-255)
        #[arg(long, default_value = "255,255,255")]
        color: String,
        /// Shadow color as R,G,B (omit for no shadow)
        #[arg(long)]
        shadow: Option<String>,
        /// Output file (default: overwrites input)
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Create PPTX with image background + editable text overlays.
    ///
    /// Text spec is JSON: [{"text":"Hello","x":0.5,"y":0.5,"w":6,"h":1,
    /// "fontSize":36,"color":"FFFFFF","bold":true}]
    /// Coordinates are in inches (13.333 x 7.5 widescreen).
    MakeSlide {
        /// Background image (PNG/JPEG)
        image: PathBuf,
        /// Output PPTX file
        #[arg(long, short)]
        output: PathBuf,
        /// Text overlays as JSON array
        #[arg(long)]
        texts: Option<String>,
        /// Slide width in inches (default: 13.3333333333 for 16:9)
        #[arg(long, default_value = "13.3333333333")]
        width: f64,
        /// Slide height in inches (default: 7.5 for 16:9)
        #[arg(long, default_value = "7.5")]
        height: f64,
    },
}

impl Executable for OfficeCommand {
    fn execute(self) -> Result<()> {
        match self.action {
            OfficeAction::Extract { file } => cmd_extract(&file),
            OfficeAction::Unpack { file, output } => cmd_unpack(&file, &output),
            OfficeAction::Pack { input, output } => cmd_pack(&input, &output),
            OfficeAction::Clean { dir } => cmd_clean(&dir),
            OfficeAction::AddSlide { dir, source } => cmd_add_slide(&dir, &source),
            OfficeAction::Validate { path, auto_repair } => cmd_validate(&path, auto_repair),
            OfficeAction::Thumbnail {
                file,
                output_prefix,
                cols,
            } => cmd_thumbnail(&file, &output_prefix, cols),
            OfficeAction::Comment {
                dir,
                id,
                text,
                author,
                initials,
                parent,
            } => cmd_comment(&dir, id, &text, &author, &initials, parent),
            OfficeAction::AcceptChanges { input, output } => cmd_accept_changes(&input, &output),
            OfficeAction::Recalc { file, timeout } => cmd_recalc(&file, timeout),
            OfficeAction::Soffice { args } => cmd_soffice(&args),
            OfficeAction::OverlayText {
                image,
                text,
                x,
                y,
                scale,
                color,
                shadow,
                output,
            } => cmd_overlay_text(
                &image,
                &text,
                x,
                y,
                scale,
                &color,
                shadow.as_deref(),
                output.as_deref(),
            ),
            OfficeAction::MakeSlide {
                image,
                output,
                texts,
                width,
                height,
            } => cmd_make_slide(&image, &output, texts.as_deref(), width, height),
        }
    }
}

// ─── Smart quote replacement tables ───

const SMART_QUOTE_REPLACEMENTS: &[(&str, &str)] = &[
    ("\u{201C}", "&#x201C;"), // left double quote
    ("\u{201D}", "&#x201D;"), // right double quote
    ("\u{2018}", "&#x2018;"), // left single quote
    ("\u{2019}", "&#x2019;"), // right single quote
];

const SMART_QUOTE_RESTORE: &[(&str, &str)] = &[
    ("&#x201C;", "\u{201C}"),
    ("&#x201D;", "\u{201D}"),
    ("&#x2018;", "\u{2018}"),
    ("&#x2019;", "\u{2019}"),
];

// ─── extract command ───

fn cmd_extract(file: &Path) -> Result<()> {
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "pptx" => extract_pptx(file),
        "docx" => extract_docx(file),
        "xlsx" => extract_xlsx(file),
        _ => bail!("Unsupported file type: .{ext}. Expected .pptx, .docx, or .xlsx"),
    }
}

fn extract_pptx(file: &Path) -> Result<()> {
    let reader =
        fs::File::open(file).wrap_err_with(|| format!("failed to open {}", file.display()))?;
    let mut archive = zip::ZipArchive::new(reader).wrap_err("not a valid ZIP/Office file")?;

    // Parse presentation.xml.rels to map rId -> slide paths
    let rid_to_target = parse_rels_from_zip(&mut archive, "ppt/_rels/presentation.xml.rels")?;

    // Parse presentation.xml to get ordered slide rIds
    let ordered_rids = parse_slide_order_from_zip(&mut archive)?;

    // For each slide in order, extract text
    for (i, rid) in ordered_rids.iter().enumerate() {
        let target = match rid_to_target.get(rid.as_str()) {
            Some(t) => t,
            None => continue,
        };

        // Target is relative to ppt/, e.g. "slides/slide1.xml"
        let slide_path = format!("ppt/{target}");

        let xml = match read_zip_entry(&mut archive, &slide_path) {
            Ok(xml) => xml,
            Err(_) => continue,
        };

        let texts = extract_text_bodies(&xml);
        if texts.is_empty() {
            continue;
        }

        // First text body is typically the title
        let title = texts[0].first().map(|s| s.as_str()).unwrap_or("(untitled)");
        println!("## Slide {}: {}\n", i + 1, title.trim());

        for (j, body) in texts.iter().enumerate() {
            if j == 0 {
                // Title body - print remaining paragraphs after title
                for para in body.iter().skip(1) {
                    let trimmed = para.trim();
                    if !trimmed.is_empty() {
                        println!("{trimmed}\n");
                    }
                }
            } else {
                for para in body {
                    let trimmed = para.trim();
                    if !trimmed.is_empty() {
                        println!("{trimmed}\n");
                    }
                }
            }
        }
    }

    Ok(())
}

fn extract_docx(file: &Path) -> Result<()> {
    let reader =
        fs::File::open(file).wrap_err_with(|| format!("failed to open {}", file.display()))?;
    let mut archive = zip::ZipArchive::new(reader).wrap_err("not a valid ZIP/Office file")?;

    let xml = read_zip_entry(&mut archive, "word/document.xml")?;

    // Extract all <w:t> text nodes within <w:p> paragraphs
    let paragraphs = extract_docx_paragraphs(&xml);

    for para in &paragraphs {
        let trimmed = para.trim();
        if trimmed.is_empty() {
            println!();
        } else {
            println!("{trimmed}");
        }
    }

    Ok(())
}

fn extract_xlsx(file: &Path) -> Result<()> {
    let reader =
        fs::File::open(file).wrap_err_with(|| format!("failed to open {}", file.display()))?;
    let mut archive = zip::ZipArchive::new(reader).wrap_err("not a valid ZIP/Office file")?;

    // Read shared strings
    let shared_strings = match read_zip_entry(&mut archive, "xl/sharedStrings.xml") {
        Ok(xml) => parse_shared_strings(&xml),
        Err(_) => Vec::new(),
    };

    // Find all sheet files
    let rid_to_target = parse_rels_from_zip(&mut archive, "xl/_rels/workbook.xml.rels")?;

    // Parse workbook.xml for sheet names and order
    let sheets = parse_xlsx_sheets(&mut archive, &rid_to_target)?;

    for (name, sheet_path) in &sheets {
        println!("## {name}\n");

        let xml = match read_zip_entry(&mut archive, sheet_path) {
            Ok(xml) => xml,
            Err(_) => continue,
        };

        let rows = extract_xlsx_rows(&xml, &shared_strings);
        if rows.is_empty() {
            continue;
        }

        // Output as markdown table
        // Find max columns
        let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if max_cols == 0 {
            continue;
        }

        for (i, row) in rows.iter().enumerate() {
            let mut cells: Vec<String> = row.clone();
            cells.resize(max_cols, String::new());
            println!("| {} |", cells.join(" | "));
            if i == 0 {
                let sep: Vec<&str> = (0..max_cols).map(|_| "---").collect();
                println!("| {} |", sep.join(" | "));
            }
        }
        println!();
    }

    Ok(())
}

// ─── unpack command ───

fn cmd_unpack(file: &Path, output: &Path) -> Result<()> {
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if !matches!(ext.as_str(), "pptx" | "docx" | "xlsx") {
        bail!("Expected .pptx, .docx, or .xlsx file");
    }

    let reader =
        fs::File::open(file).wrap_err_with(|| format!("failed to open {}", file.display()))?;
    let mut archive = zip::ZipArchive::new(reader).wrap_err("not a valid ZIP/Office file")?;

    fs::create_dir_all(output)?;

    let mut xml_count = 0;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let entry_path = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };

        let dest = output.join(&entry_path);

        if entry.is_dir() {
            fs::create_dir_all(&dest)?;
            continue;
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;

        let is_xml = entry_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e == "xml" || e == "rels")
            .unwrap_or(false);

        if is_xml {
            // Pretty-print XML and escape smart quotes
            let content = String::from_utf8_lossy(&data);
            let pretty = pretty_print_xml(&content);
            let escaped = escape_smart_quotes(&pretty);
            fs::write(&dest, escaped)?;
            xml_count += 1;
        } else {
            fs::write(&dest, &data)?;
        }
    }

    println!(
        "Unpacked {} ({} XML files)",
        file.display().to_string().green(),
        xml_count
    );

    Ok(())
}

// ─── pack command ───

fn cmd_pack(input: &Path, output: &Path) -> Result<()> {
    if !input.is_dir() {
        bail!("{} is not a directory", input.display());
    }

    let ext = output
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if !matches!(ext.as_str(), "pptx" | "docx" | "xlsx") {
        bail!("Output must be .pptx, .docx, or .xlsx");
    }

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }

    let out_file = fs::File::create(output)
        .wrap_err_with(|| format!("failed to create {}", output.display()))?;
    let mut zip_writer = zip::ZipWriter::new(out_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // Collect all files, sorted for deterministic output
    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(input, &mut files)?;
    files.sort();

    for file_path in &files {
        let rel = file_path.strip_prefix(input)?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        let data = fs::read(file_path)?;

        let is_xml = file_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e == "xml" || e == "rels")
            .unwrap_or(false);

        if is_xml {
            // Condense XML: remove whitespace-only text nodes and comments,
            // restore smart quotes
            let content = String::from_utf8_lossy(&data);
            let restored = restore_smart_quotes(&content);
            let condensed = condense_xml(&restored);
            zip_writer.start_file(&rel_str, options)?;
            zip_writer.write_all(condensed.as_bytes())?;
        } else {
            zip_writer.start_file(&rel_str, options)?;
            zip_writer.write_all(&data)?;
        }
    }

    zip_writer.finish()?;

    println!(
        "Packed {} to {}",
        input.display().to_string().green(),
        output.display().to_string().green()
    );

    Ok(())
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

// ─── clean command ───

fn cmd_clean(dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    let mut all_removed: Vec<String> = Vec::new();

    // 1. Remove orphaned slides
    let slides_removed = remove_orphaned_slides(dir)?;
    all_removed.extend(slides_removed);

    // 2. Remove [trash] directory
    let trash_dir = dir.join("[trash]");
    if trash_dir.is_dir() {
        for entry in fs::read_dir(&trash_dir)? {
            let entry = entry?;
            if entry.path().is_file() {
                let rel = entry
                    .path()
                    .strip_prefix(dir)?
                    .to_string_lossy()
                    .to_string();
                all_removed.push(rel);
                fs::remove_file(entry.path())?;
            }
        }
        fs::remove_dir(&trash_dir).ok();
    }

    // 3. Iteratively remove orphaned rels and unreferenced files
    loop {
        let referenced = get_all_referenced_files(dir)?;
        let removed = remove_unreferenced_files(dir, &referenced)?;
        if removed.is_empty() {
            break;
        }
        all_removed.extend(removed);
    }

    // 4. Update [Content_Types].xml
    if !all_removed.is_empty() {
        update_content_types(dir, &all_removed)?;
    }

    if all_removed.is_empty() {
        println!("No unreferenced files found");
    } else {
        println!("Removed {} unreferenced files:", all_removed.len());
        for f in &all_removed {
            println!("  {f}");
        }
    }

    Ok(())
}

fn remove_orphaned_slides(dir: &Path) -> Result<Vec<String>> {
    let pres_path = dir.join("ppt/presentation.xml");
    let pres_rels_path = dir.join("ppt/_rels/presentation.xml.rels");

    if !pres_path.exists() || !pres_rels_path.exists() {
        return Ok(vec![]);
    }

    // Get slides referenced in sldIdLst
    let pres_xml = fs::read_to_string(&pres_path)?;
    let rels_xml = fs::read_to_string(&pres_rels_path)?;

    let rid_to_slide = parse_rels_targets(&rels_xml, "slide");
    let referenced_rids = parse_sld_id_rids(&pres_xml);

    let referenced_slides: HashSet<String> = referenced_rids
        .iter()
        .filter_map(|rid| rid_to_slide.get(rid.as_str()))
        .filter_map(|target| target.strip_prefix("slides/"))
        .map(|s| s.to_string())
        .collect();

    let slides_dir = dir.join("ppt/slides");
    let slides_rels_dir = slides_dir.join("_rels");
    let mut removed = Vec::new();

    if !slides_dir.exists() {
        return Ok(removed);
    }

    // Remove slide files not in referenced set
    for entry in fs::read_dir(&slides_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("slide") && name.ends_with(".xml") && !referenced_slides.contains(&name)
        {
            let rel = entry
                .path()
                .strip_prefix(dir)?
                .to_string_lossy()
                .to_string();
            fs::remove_file(entry.path())?;
            removed.push(rel);

            // Remove corresponding .rels
            let rels_file = slides_rels_dir.join(format!("{name}.rels"));
            if rels_file.exists() {
                let rels_rel = rels_file.strip_prefix(dir)?.to_string_lossy().to_string();
                fs::remove_file(&rels_file)?;
                removed.push(rels_rel);
            }
        }
    }

    // Remove orphaned relationships from presentation.xml.rels
    if !removed.is_empty() {
        let mut rels_content = fs::read_to_string(&pres_rels_path)?;
        for slide_name in &referenced_slides {
            // Keep referenced ones
            let _ = slide_name;
        }
        // Remove Relationship entries for slides not in referenced set
        let mut new_rels = String::new();
        for line in rels_content.lines() {
            if line.contains("Target=\"slides/") {
                // Check if this slide is referenced
                let is_referenced = referenced_slides
                    .iter()
                    .any(|s| line.contains(&format!("slides/{s}")));
                if !is_referenced {
                    continue;
                }
            }
            new_rels.push_str(line);
            new_rels.push('\n');
        }
        if new_rels != rels_content {
            rels_content = new_rels;
            fs::write(&pres_rels_path, rels_content)?;
        }
    }

    Ok(removed)
}

fn get_all_referenced_files(dir: &Path) -> Result<HashSet<PathBuf>> {
    let mut referenced = HashSet::new();

    for entry in walkdir(dir)? {
        let ext = entry.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "rels" {
            continue;
        }

        let xml = match fs::read_to_string(&entry) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // The rels file is in a _rels dir, targets are relative to parent of _rels
        let rels_parent = entry.parent().and_then(|p| p.parent()).unwrap_or(dir);

        for target in parse_all_rels_targets(&xml) {
            let resolved = rels_parent.join(&target);
            if let Ok(canonical) = resolved.canonicalize() {
                if let Ok(rel) = canonical.strip_prefix(dir.canonicalize()?) {
                    referenced.insert(rel.to_path_buf());
                }
            } else {
                // File might not exist, but record the relative path anyway
                let normed = normalize_path(&rels_parent.join(&target), dir);
                if let Some(p) = normed {
                    referenced.insert(p);
                }
            }
        }
    }

    Ok(referenced)
}

fn remove_unreferenced_files(dir: &Path, referenced: &HashSet<PathBuf>) -> Result<Vec<String>> {
    let resource_dirs = [
        "ppt/media",
        "ppt/embeddings",
        "ppt/charts",
        "ppt/diagrams",
        "ppt/tags",
        "ppt/drawings",
        "ppt/ink",
    ];

    let mut removed = Vec::new();

    for resource_dir in &resource_dirs {
        let full_dir = dir.join(resource_dir);
        if !full_dir.exists() {
            continue;
        }

        for entry in fs::read_dir(&full_dir)? {
            let entry = entry?;
            if !entry.path().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(dir)?.to_path_buf();
            if !referenced.contains(&rel) {
                fs::remove_file(entry.path())?;
                removed.push(rel.to_string_lossy().to_string());
            }
        }
    }

    // Notes slides
    let notes_dir = dir.join("ppt/notesSlides");
    if notes_dir.exists() {
        for entry in fs::read_dir(&notes_dir)? {
            let entry = entry?;
            if !entry.path().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(dir)?.to_path_buf();
            if !referenced.contains(&rel) {
                fs::remove_file(entry.path())?;
                removed.push(rel.to_string_lossy().to_string());
            }
        }
    }

    // Themes
    let theme_dir = dir.join("ppt/theme");
    if theme_dir.exists() {
        for entry in fs::read_dir(&theme_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !entry.path().is_file() || !name.starts_with("theme") || !name.ends_with(".xml") {
                continue;
            }
            let rel = entry.path().strip_prefix(dir)?.to_path_buf();
            if !referenced.contains(&rel) {
                fs::remove_file(entry.path())?;
                removed.push(rel.to_string_lossy().to_string());
                // Also remove theme rels
                let theme_rels = theme_dir.join("_rels").join(format!("{name}.rels"));
                if theme_rels.exists() {
                    let rels_rel = theme_rels.strip_prefix(dir)?.to_string_lossy().to_string();
                    fs::remove_file(&theme_rels)?;
                    removed.push(rels_rel);
                }
            }
        }
    }

    // Orphaned rels in resource dirs
    for sub in &["charts", "diagrams", "drawings"] {
        let rels_dir = dir.join(format!("ppt/{sub}/_rels"));
        if !rels_dir.exists() {
            continue;
        }
        for entry in fs::read_dir(&rels_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".rels") {
                continue;
            }
            // Check if parent resource exists
            let resource_name = name.strip_suffix(".rels").unwrap();
            let resource_path = dir.join(format!("ppt/{sub}/{resource_name}"));
            let resource_rel = PathBuf::from(format!("ppt/{sub}/{resource_name}"));
            if !resource_path.exists() || !referenced.contains(&resource_rel) {
                fs::remove_file(entry.path())?;
                removed.push(
                    entry
                        .path()
                        .strip_prefix(dir)?
                        .to_string_lossy()
                        .to_string(),
                );
            }
        }
    }

    Ok(removed)
}

fn update_content_types(dir: &Path, removed_files: &[String]) -> Result<()> {
    let ct_path = dir.join("[Content_Types].xml");
    if !ct_path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&ct_path)?;
    let mut new_content = String::new();
    let removed_set: HashSet<&str> = removed_files.iter().map(|s| s.as_str()).collect();

    for line in content.lines() {
        if line.contains("PartName=") {
            // Extract PartName value
            if let Some(start) = line.find("PartName=\"") {
                let after = &line[start + 10..];
                if let Some(end) = after.find('"') {
                    let part_name = after[..end].trim_start_matches('/');
                    if removed_set.contains(part_name) {
                        continue;
                    }
                }
            }
        }
        new_content.push_str(line);
        new_content.push('\n');
    }

    if new_content != content {
        fs::write(&ct_path, new_content)?;
    }

    Ok(())
}

// ─── add-slide command ───

fn cmd_add_slide(dir: &Path, source: &str) -> Result<()> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    if source.starts_with("slideLayout") && source.ends_with(".xml") {
        create_slide_from_layout(dir, source)
    } else {
        duplicate_slide(dir, source)
    }
}

fn get_next_slide_number(slides_dir: &Path) -> Result<u32> {
    let mut max_num = 0u32;
    if slides_dir.exists() {
        for entry in fs::read_dir(slides_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(num_str) = name
                .strip_prefix("slide")
                .and_then(|s| s.strip_suffix(".xml"))
            {
                if let Ok(n) = num_str.parse::<u32>() {
                    max_num = max_num.max(n);
                }
            }
        }
    }
    Ok(max_num + 1)
}

fn get_next_slide_id(dir: &Path) -> Result<u32> {
    let pres_path = dir.join("ppt/presentation.xml");
    let content = fs::read_to_string(&pres_path)?;

    let mut max_id = 255u32;
    let re = regex::Regex::new(r#"<p:sldId[^>]*id="(\d+)""#).unwrap();
    for cap in re.captures_iter(&content) {
        if let Ok(id) = cap[1].parse::<u32>() {
            max_id = max_id.max(id);
        }
    }
    Ok(max_id + 1)
}

fn get_next_rid(rels_content: &str) -> u32 {
    let re = regex::Regex::new(r#"Id="rId(\d+)""#).unwrap();
    let mut max_rid = 0u32;
    for cap in re.captures_iter(rels_content) {
        if let Ok(n) = cap[1].parse::<u32>() {
            max_rid = max_rid.max(n);
        }
    }
    max_rid + 1
}

fn add_to_content_types(dir: &Path, slide_name: &str) -> Result<()> {
    let ct_path = dir.join("[Content_Types].xml");
    let mut content = fs::read_to_string(&ct_path)?;

    let part = format!("/ppt/slides/{slide_name}");
    if !content.contains(&part) {
        let override_elem = format!(
            "  <Override PartName=\"{part}\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>\n"
        );
        content = content.replace("</Types>", &format!("{override_elem}</Types>"));
        fs::write(&ct_path, content)?;
    }
    Ok(())
}

fn add_to_presentation_rels(dir: &Path, slide_name: &str) -> Result<String> {
    let rels_path = dir.join("ppt/_rels/presentation.xml.rels");
    let mut content = fs::read_to_string(&rels_path)?;

    let next_rid = get_next_rid(&content);
    let rid = format!("rId{next_rid}");

    let target = format!("slides/{slide_name}");
    if !content.contains(&target) {
        let rel_elem = format!(
            "  <Relationship Id=\"{rid}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"{target}\"/>\n"
        );
        content = content.replace("</Relationships>", &format!("{rel_elem}</Relationships>"));
        fs::write(&rels_path, content)?;
    }

    Ok(rid)
}

fn create_slide_from_layout(dir: &Path, layout_file: &str) -> Result<()> {
    let layouts_dir = dir.join("ppt/slideLayouts");
    let layout_path = layouts_dir.join(layout_file);
    if !layout_path.exists() {
        bail!("{} not found", layout_path.display());
    }

    let slides_dir = dir.join("ppt/slides");
    fs::create_dir_all(&slides_dir)?;
    let rels_dir = slides_dir.join("_rels");
    fs::create_dir_all(&rels_dir)?;

    let next_num = get_next_slide_number(&slides_dir)?;
    let dest = format!("slide{next_num}.xml");

    let slide_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr>
        <p:cNvPr id="1" name=""/>
        <p:cNvGrpSpPr/>
        <p:nvPr/>
      </p:nvGrpSpPr>
      <p:grpSpPr>
        <a:xfrm>
          <a:off x="0" y="0"/>
          <a:ext cx="0" cy="0"/>
          <a:chOff x="0" y="0"/>
          <a:chExt cx="0" cy="0"/>
        </a:xfrm>
      </p:grpSpPr>
    </p:spTree>
  </p:cSld>
  <p:clrMapOvr>
    <a:masterClrMapping/>
  </p:clrMapOvr>
</p:sld>"#;

    fs::write(slides_dir.join(&dest), slide_xml)?;

    let rels_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/{layout_file}"/>
</Relationships>"#
    );
    fs::write(rels_dir.join(format!("{dest}.rels")), rels_xml)?;

    add_to_content_types(dir, &dest)?;
    let rid = add_to_presentation_rels(dir, &dest)?;
    let next_slide_id = get_next_slide_id(dir)?;

    println!("Created {dest} from {layout_file}");
    println!(
        "Add to presentation.xml <p:sldIdLst>: <p:sldId id=\"{next_slide_id}\" r:id=\"{rid}\"/>"
    );

    Ok(())
}

fn duplicate_slide(dir: &Path, source: &str) -> Result<()> {
    let slides_dir = dir.join("ppt/slides");
    let source_path = slides_dir.join(source);
    if !source_path.exists() {
        bail!("{} not found", source_path.display());
    }

    let rels_dir = slides_dir.join("_rels");
    let next_num = get_next_slide_number(&slides_dir)?;
    let dest = format!("slide{next_num}.xml");

    // Copy slide
    fs::copy(&source_path, slides_dir.join(&dest))?;

    // Copy rels (if exists), removing notesSlide references
    let source_rels = rels_dir.join(format!("{source}.rels"));
    if source_rels.exists() {
        let mut rels_content = fs::read_to_string(&source_rels)?;
        // Remove notesSlide relationship lines
        let re =
            regex::Regex::new(r#"(?m)^\s*<Relationship[^>]*Type="[^"]*notesSlide"[^>]*/>\s*$"#)
                .unwrap();
        rels_content = re.replace_all(&rels_content, "\n").to_string();
        fs::write(rels_dir.join(format!("{dest}.rels")), rels_content)?;
    }

    add_to_content_types(dir, &dest)?;
    let rid = add_to_presentation_rels(dir, &dest)?;
    let next_slide_id = get_next_slide_id(dir)?;

    println!("Created {dest} from {source}");
    println!(
        "Add to presentation.xml <p:sldIdLst>: <p:sldId id=\"{next_slide_id}\" r:id=\"{rid}\"/>"
    );

    Ok(())
}

// ─── validate command ───

fn cmd_validate(path: &Path, auto_repair: bool) -> Result<()> {
    // Determine if path is a file or directory
    let (dir, _temp_dir) = if path.is_file() {
        let temp = tempfile::tempdir()?;
        let reader = fs::File::open(path)?;
        let mut archive = zip::ZipArchive::new(reader)?;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let entry_path = match entry.enclosed_name() {
                Some(p) => p.to_path_buf(),
                None => continue,
            };
            let dest = temp.path().join(&entry_path);
            if entry.is_dir() {
                fs::create_dir_all(&dest)?;
            } else {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut data = Vec::new();
                entry.read_to_end(&mut data)?;
                fs::write(&dest, &data)?;
            }
        }
        (temp.path().to_path_buf(), Some(temp))
    } else if path.is_dir() {
        (path.to_path_buf(), None)
    } else {
        bail!("{} does not exist", path.display());
    };

    let mut issues = Vec::new();
    let mut repairs = 0;

    // 1. Check [Content_Types].xml exists
    let ct_path = dir.join("[Content_Types].xml");
    if !ct_path.exists() {
        issues.push("[Content_Types].xml is missing".to_string());
    }

    // 2. Check all .rels targets exist
    for entry in walkdir(&dir)? {
        let ext = entry.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "rels" {
            continue;
        }

        let xml = match fs::read_to_string(&entry) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let rels_parent = entry.parent().and_then(|p| p.parent()).unwrap_or(&dir);

        for target in parse_all_rels_targets(&xml) {
            let resolved = rels_parent.join(&target);
            if !resolved.exists() {
                let rel_from = entry.strip_prefix(&dir).unwrap_or(&entry);
                issues.push(format!(
                    "{}: target '{}' does not exist",
                    rel_from.display(),
                    target
                ));
            }
        }
    }

    // 3. Check Content_Types overrides match actual files
    if ct_path.exists() {
        let ct_xml = fs::read_to_string(&ct_path)?;
        let re = regex::Regex::new(r#"PartName="(/[^"]+)""#).unwrap();
        for cap in re.captures_iter(&ct_xml) {
            let part = cap[1].trim_start_matches('/');
            let full = dir.join(part);
            if !full.exists() {
                if auto_repair {
                    // Remove the override
                    repairs += 1;
                } else {
                    issues.push(format!(
                        "[Content_Types].xml: PartName '/{part}' does not exist"
                    ));
                }
            }
        }

        if auto_repair && repairs > 0 {
            // Re-read and filter
            let content = fs::read_to_string(&ct_path)?;
            let mut new_content = String::new();
            for line in content.lines() {
                if let Some(start) = line.find("PartName=\"") {
                    let after = &line[start + 10..];
                    if let Some(end) = after.find('"') {
                        let part = after[..end].trim_start_matches('/');
                        if !dir.join(part).exists() {
                            continue;
                        }
                    }
                }
                new_content.push_str(line);
                new_content.push('\n');
            }
            fs::write(&ct_path, new_content)?;
        }
    }

    // 4. Check presentation.xml slide IDs
    let pres_path = dir.join("ppt/presentation.xml");
    if pres_path.exists() {
        let pres_xml = fs::read_to_string(&pres_path)?;
        let re = regex::Regex::new(r#"<p:sldId[^>]*id="(\d+)""#).unwrap();
        let mut slide_ids: Vec<u32> = Vec::new();
        for cap in re.captures_iter(&pres_xml) {
            if let Ok(id) = cap[1].parse::<u32>() {
                if slide_ids.contains(&id) {
                    issues.push(format!("Duplicate slide ID: {id}"));
                }
                slide_ids.push(id);
            }
        }
    }

    if auto_repair && repairs > 0 {
        println!("Auto-repaired {repairs} issue(s)");
    }

    if issues.is_empty() {
        println!("{}", "All validations PASSED!".green());
        Ok(())
    } else {
        for issue in &issues {
            eprintln!("{}: {issue}", "FAIL".red());
        }
        bail!("{} validation issue(s) found", issues.len());
    }
}

// ─── XML utility functions ───

fn pretty_print_xml(input: &str) -> String {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    use quick_xml::writer::Writer;

    let mut reader = Reader::from_str(input);
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(event) => {
                if writer.write_event(event).is_err() {
                    return input.to_string();
                }
            }
            Err(_) => return input.to_string(),
        }
    }

    String::from_utf8(writer.into_inner()).unwrap_or_else(|_| input.to_string())
}

fn condense_xml(input: &str) -> String {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    use quick_xml::writer::Writer;

    let mut reader = Reader::from_str(input);
    let mut writer = Writer::new(Vec::new());

    // Track if we're inside a text element (ending with :t)
    let mut in_text_elem = false;
    let mut elem_stack: Vec<String> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                in_text_elem = name.ends_with(":t") || name == "t";
                elem_stack.push(name);
                let _ = writer.write_event(Event::Start(e.clone()));
            }
            Ok(Event::End(ref e)) => {
                elem_stack.pop();
                in_text_elem = elem_stack
                    .last()
                    .map(|n| n.ends_with(":t") || n == "t")
                    .unwrap_or(false);
                let _ = writer.write_event(Event::End(e.clone()));
            }
            Ok(Event::Text(ref e)) => {
                if in_text_elem {
                    // Preserve text in :t elements
                    let _ = writer.write_event(Event::Text(e.clone()));
                } else {
                    // Strip whitespace-only text nodes
                    let text = e.unescape().unwrap_or_default();
                    if !text.trim().is_empty() {
                        let _ = writer.write_event(Event::Text(e.clone()));
                    }
                }
            }
            Ok(Event::Comment(_)) => {
                // Strip comments
            }
            Ok(event) => {
                let _ = writer.write_event(event);
            }
            Err(_) => return input.to_string(),
        }
    }

    String::from_utf8(writer.into_inner()).unwrap_or_else(|_| input.to_string())
}

fn escape_smart_quotes(input: &str) -> String {
    let mut result = input.to_string();
    for (from, to) in SMART_QUOTE_REPLACEMENTS {
        result = result.replace(from, to);
    }
    result
}

fn restore_smart_quotes(input: &str) -> String {
    let mut result = input.to_string();
    for (from, to) in SMART_QUOTE_RESTORE {
        result = result.replace(from, to);
    }
    result
}

// ─── ZIP/XML parsing helpers ───

fn read_zip_entry(archive: &mut zip::ZipArchive<fs::File>, name: &str) -> Result<String> {
    let mut entry = archive
        .by_name(name)
        .wrap_err_with(|| format!("entry '{name}' not found"))?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf)?;
    Ok(buf)
}

fn parse_rels_from_zip(
    archive: &mut zip::ZipArchive<fs::File>,
    rels_path: &str,
) -> Result<HashMap<String, String>> {
    let xml = read_zip_entry(archive, rels_path)?;
    Ok(parse_rels_targets_all(&xml))
}

fn parse_rels_targets_all(xml: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let re =
        regex::Regex::new(r#"<Relationship[^>]*Id="([^"]+)"[^>]*Target="([^"]+)"[^>]*/?"#).unwrap();
    // Also handle reversed attribute order
    let re2 =
        regex::Regex::new(r#"<Relationship[^>]*Target="([^"]+)"[^>]*Id="([^"]+)"[^>]*/?"#).unwrap();

    for cap in re.captures_iter(xml) {
        map.insert(cap[1].to_string(), cap[2].to_string());
    }
    for cap in re2.captures_iter(xml) {
        map.entry(cap[2].to_string())
            .or_insert_with(|| cap[1].to_string());
    }
    map
}

fn parse_rels_targets(xml: &str, type_filter: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let re = regex::Regex::new(
        r#"<Relationship[^>]*Id="([^"]+)"[^>]*Type="([^"]+)"[^>]*Target="([^"]+)"[^>]*/?"#,
    )
    .unwrap();

    for cap in re.captures_iter(xml) {
        if cap[2].contains(type_filter) {
            map.insert(cap[1].to_string(), cap[3].to_string());
        }
    }
    map
}

fn parse_sld_id_rids(xml: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"<p:sldId[^>]*r:id="([^"]+)""#).unwrap();
    re.captures_iter(xml).map(|c| c[1].to_string()).collect()
}

fn parse_slide_order_from_zip(archive: &mut zip::ZipArchive<fs::File>) -> Result<Vec<String>> {
    let xml = read_zip_entry(archive, "ppt/presentation.xml")?;
    Ok(parse_sld_id_rids(&xml))
}

fn parse_all_rels_targets(xml: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"Target="([^"]+)""#).unwrap();
    re.captures_iter(xml)
        .map(|c| c[1].to_string())
        .filter(|t| !t.starts_with("http://") && !t.starts_with("https://"))
        .collect()
}

/// Extract text bodies from a PPTX slide XML.
/// Returns Vec<Vec<String>> where each outer vec is a txBody and inner vec is paragraphs.
fn extract_text_bodies(xml: &str) -> Vec<Vec<String>> {
    let mut bodies = Vec::new();
    let mut current_body: Option<Vec<String>> = None;
    let mut current_para = String::new();
    let mut in_text = false;
    let mut depth = 0;
    let mut body_depth = 0;
    let mut para_depth = 0;

    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                depth += 1;
                if name.ends_with(":txBody") || name == "txBody" {
                    current_body = Some(Vec::new());
                    body_depth = depth;
                } else if (name.ends_with(":p") || name == "p") && current_body.is_some() {
                    current_para.clear();
                    para_depth = depth;
                } else if (name.ends_with(":t") || name == "t") && current_body.is_some() {
                    in_text = true;
                }
            }
            Ok(Event::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if (name.ends_with(":t") || name == "t") && in_text {
                    in_text = false;
                } else if (name.ends_with(":p") || name == "p")
                    && current_body.is_some()
                    && depth == para_depth
                {
                    if let Some(ref mut body) = current_body {
                        body.push(current_para.clone());
                    }
                    current_para.clear();
                } else if (name.ends_with(":txBody") || name == "txBody") && depth == body_depth {
                    if let Some(body) = current_body.take() {
                        if body.iter().any(|p| !p.trim().is_empty()) {
                            bodies.push(body);
                        }
                    }
                }
                depth -= 1;
            }
            Ok(Event::Text(ref e)) => {
                if in_text {
                    if let Ok(text) = e.unescape() {
                        current_para.push_str(&text);
                    }
                }
            }
            _ => {}
        }
    }

    bodies
}

/// Extract paragraphs from DOCX document.xml
fn extract_docx_paragraphs(xml: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current_para = String::new();
    let mut in_text = false;
    let mut in_para = false;

    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name.ends_with(":p") || name == "p" {
                    in_para = true;
                    current_para.clear();
                } else if (name.ends_with(":t") || name == "t") && in_para {
                    in_text = true;
                }
            }
            Ok(Event::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if (name.ends_with(":t") || name == "t") && in_text {
                    in_text = false;
                } else if (name.ends_with(":p") || name == "p") && in_para {
                    paragraphs.push(current_para.clone());
                    current_para.clear();
                    in_para = false;
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_text {
                    if let Ok(text) = e.unescape() {
                        current_para.push_str(&text);
                    }
                }
            }
            _ => {}
        }
    }

    paragraphs
}

/// Parse shared strings from XLSX
fn parse_shared_strings(xml: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_si = false;
    let mut in_t = false;

    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "si" || name.ends_with(":si") {
                    in_si = true;
                    current.clear();
                } else if (name == "t" || name.ends_with(":t")) && in_si {
                    in_t = true;
                }
            }
            Ok(Event::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "t" || name.ends_with(":t") {
                    in_t = false;
                } else if (name == "si" || name.ends_with(":si")) && in_si {
                    strings.push(current.clone());
                    in_si = false;
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_t {
                    if let Ok(text) = e.unescape() {
                        current.push_str(&text);
                    }
                }
            }
            _ => {}
        }
    }

    strings
}

/// Parse sheet names and paths from XLSX workbook
fn parse_xlsx_sheets(
    archive: &mut zip::ZipArchive<fs::File>,
    rid_to_target: &HashMap<String, String>,
) -> Result<Vec<(String, String)>> {
    let xml = read_zip_entry(archive, "xl/workbook.xml")?;
    let mut sheets = Vec::new();

    let re = regex::Regex::new(r#"<sheet[^>]*name="([^"]+)"[^>]*r:id="([^"]+)""#).unwrap();
    let re2 = regex::Regex::new(r#"<sheet[^>]*r:id="([^"]+)"[^>]*name="([^"]+)""#).unwrap();

    for cap in re.captures_iter(&xml) {
        let name = cap[1].to_string();
        let rid = &cap[2];
        if let Some(target) = rid_to_target.get(rid) {
            sheets.push((name, format!("xl/{target}")));
        }
    }
    // Also try reversed attribute order
    if sheets.is_empty() {
        for cap in re2.captures_iter(&xml) {
            let rid = &cap[1];
            let name = cap[2].to_string();
            if let Some(target) = rid_to_target.get(rid) {
                sheets.push((name, format!("xl/{target}")));
            }
        }
    }

    Ok(sheets)
}

/// Extract rows from XLSX sheet XML
fn extract_xlsx_rows(xml: &str, shared_strings: &[String]) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_value = String::new();
    let mut in_row = false;
    let mut in_cell = false;
    let mut in_value = false;
    let mut cell_type = String::new();

    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "row" || name.ends_with(":row") {
                    in_row = true;
                    current_row.clear();
                } else if (name == "c" || name.ends_with(":c")) && in_row {
                    in_cell = true;
                    cell_type.clear();
                    current_value.clear();
                    // Check for t="s" (shared string)
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"t" {
                            cell_type = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                } else if (name == "v" || name.ends_with(":v")) && in_cell {
                    in_value = true;
                }
            }
            Ok(Event::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "v" || name.ends_with(":v") {
                    in_value = false;
                } else if (name == "c" || name.ends_with(":c")) && in_cell {
                    let value = if cell_type == "s" {
                        // Shared string reference
                        if let Ok(idx) = current_value.trim().parse::<usize>() {
                            shared_strings.get(idx).cloned().unwrap_or_default()
                        } else {
                            current_value.clone()
                        }
                    } else {
                        current_value.clone()
                    };
                    current_row.push(value);
                    in_cell = false;
                } else if (name == "row" || name.ends_with(":row")) && in_row {
                    if !current_row.is_empty() {
                        rows.push(current_row.clone());
                    }
                    in_row = false;
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_value {
                    if let Ok(text) = e.unescape() {
                        current_value.push_str(&text);
                    }
                }
            }
            _ => {}
        }
    }

    rows
}

// ─── Filesystem helpers ───

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walkdir_inner(dir, &mut files)?;
    Ok(files)
}

fn walkdir_inner(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walkdir_inner(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

fn normalize_path(path: &Path, base: &Path) -> Option<PathBuf> {
    // Simple normalization: resolve .. and .
    let mut components = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::CurDir => {}
            other => components.push(other),
        }
    }
    let normalized: PathBuf = components.iter().collect();
    normalized.strip_prefix(base).ok().map(|p| p.to_path_buf())
}

// ═══════════════════════════════════════════════════════════════════
// Phase 2: soffice helper, thumbnail, comment, accept-changes, recalc
// ═══════════════════════════════════════════════════════════════════

// ─── LibreOffice (soffice) helpers ───

/// C source for the AF_UNIX socket shim (LD_PRELOAD interposer).
/// Allows LibreOffice to run in sandboxed environments where AF_UNIX is blocked
/// by replacing socket() calls with socketpair() and simulating listen/accept.
const SOFFICE_SHIM_C: &str = r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>

#define MAX_FD 1024
static int (*real_socket)(int,int,int);
static int (*real_socketpair)(int,int,int,int[2]);
static int (*real_listen)(int,int);
static int (*real_accept)(int,struct sockaddr*,socklen_t*);
static int (*real_close)(int);
static ssize_t (*real_read)(int,void*,size_t);
static int is_shimmed[MAX_FD];
static int peer_of[MAX_FD];
static int wake_r[MAX_FD];
static int wake_w[MAX_FD];
static int listener_fd = -1;

__attribute__((constructor))
static void init(void) {
    real_socket     = dlsym(RTLD_NEXT,"socket");
    real_socketpair = dlsym(RTLD_NEXT,"socketpair");
    real_listen     = dlsym(RTLD_NEXT,"listen");
    real_accept     = dlsym(RTLD_NEXT,"accept");
    real_close      = dlsym(RTLD_NEXT,"close");
    real_read       = dlsym(RTLD_NEXT,"read");
    memset(is_shimmed,0,sizeof(is_shimmed));
    memset(peer_of,-1,sizeof(peer_of));
    memset(wake_r,-1,sizeof(wake_r));
    memset(wake_w,-1,sizeof(wake_w));
}

int socket(int domain, int type, int protocol) {
    if (domain != AF_UNIX) return real_socket(domain,type,protocol);
    int fd = real_socket(domain,type,protocol);
    if (fd >= 0) return fd;
    int sv[2];
    if (real_socketpair(AF_UNIX,type,protocol,sv) < 0) { errno=EPERM; return -1; }
    if (sv[0] >= MAX_FD || sv[1] >= MAX_FD) { real_close(sv[0]); real_close(sv[1]); errno=EMFILE; return -1; }
    is_shimmed[sv[0]] = 1;
    peer_of[sv[0]] = sv[1];
    int p[2]; if (pipe(p)==0) { wake_r[sv[0]]=p[0]; wake_w[sv[0]]=p[1]; }
    return sv[0];
}
int listen(int fd, int backlog) {
    if (fd>=0 && fd<MAX_FD && is_shimmed[fd]) { listener_fd=fd; return 0; }
    return real_listen(fd,backlog);
}
int accept(int fd, struct sockaddr *addr, socklen_t *len) {
    if (fd>=0 && fd<MAX_FD && is_shimmed[fd]) {
        char buf; real_read(wake_r[fd],&buf,1);
        errno=ECONNABORTED; return -1;
    }
    return real_accept(fd,addr,len);
}
int close(int fd) {
    if (fd>=0 && fd<MAX_FD && is_shimmed[fd]) {
        is_shimmed[fd]=0;
        if (wake_w[fd]>=0) { char c='x'; write(wake_w[fd],&c,1); real_close(wake_w[fd]); wake_w[fd]=-1; }
        if (wake_r[fd]>=0) { real_close(wake_r[fd]); wake_r[fd]=-1; }
        if (peer_of[fd]>=0) { real_close(peer_of[fd]); peer_of[fd]=-1; }
        if (fd == listener_fd) { _exit(0); }
    }
    return real_close(fd);
}
"#;

/// Check if AF_UNIX sockets are available (they may be blocked in sandbox).
#[cfg(unix)]
fn af_unix_available() -> bool {
    use std::os::unix::net::UnixListener;
    let path = std::env::temp_dir().join(format!(".octos_af_unix_test_{}", std::process::id()));
    let result = UnixListener::bind(&path).is_ok();
    let _ = fs::remove_file(&path);
    result
}

#[cfg(not(unix))]
fn af_unix_available() -> bool {
    true // Non-Unix platforms don't need the shim
}

/// Ensure the LD_PRELOAD shim is compiled and return its path.
fn ensure_soffice_shim() -> Result<PathBuf> {
    let shim_path = std::env::temp_dir().join("lo_socket_shim.so");
    if shim_path.exists() {
        return Ok(shim_path);
    }
    let src_path = std::env::temp_dir().join("lo_socket_shim.c");
    fs::write(&src_path, SOFFICE_SHIM_C)?;
    let output = Command::new("gcc")
        .args([
            "-shared",
            "-fPIC",
            "-o",
            shim_path.to_str().unwrap(),
            src_path.to_str().unwrap(),
            "-ldl",
        ])
        .output()
        .wrap_err("failed to compile soffice shim (is gcc installed?)")?;
    let _ = fs::remove_file(&src_path);
    if !output.status.success() {
        bail!(
            "gcc failed to compile shim: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(shim_path)
}

/// Build environment variables for running soffice in sandbox-safe mode.
fn soffice_env() -> Result<Vec<(String, String)>> {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.push(("SAL_USE_VCLPLUGIN".into(), "svp".into()));
    if !af_unix_available() {
        let shim = ensure_soffice_shim()?;
        // Append to LD_PRELOAD if it already exists
        let existing = std::env::var("LD_PRELOAD").unwrap_or_default();
        let new_val = if existing.is_empty() {
            shim.to_string_lossy().to_string()
        } else {
            format!("{}:{}", existing, shim.to_string_lossy())
        };
        // Remove existing LD_PRELOAD from env vec if present
        env.retain(|(k, _)| k != "LD_PRELOAD");
        env.push(("LD_PRELOAD".into(), new_val));
    }
    Ok(env)
}

/// Run soffice with sandbox-safe environment.
fn run_soffice_cmd(args: &[&str], timeout_secs: Option<u32>) -> Result<std::process::Output> {
    let env = soffice_env()?;
    let mut cmd = Command::new("soffice");
    cmd.args(args);
    cmd.env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    if let Some(secs) = timeout_secs {
        // Use timeout/gtimeout wrapper
        let timeout_bin = if cfg!(target_os = "linux") {
            Some("timeout")
        } else {
            // macOS: check for gtimeout (GNU coreutils)
            Command::new(if cfg!(windows) { "where" } else { "which" })
                .arg("gtimeout")
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        Some("gtimeout")
                    } else {
                        None
                    }
                })
        };
        if let Some(bin) = timeout_bin {
            let mut timeout_cmd = Command::new(bin);
            timeout_cmd.arg(secs.to_string());
            timeout_cmd.arg("soffice");
            timeout_cmd.args(args);
            timeout_cmd.env_clear();
            for (k, v) in &env {
                timeout_cmd.env(k, v);
            }
            return timeout_cmd
                .output()
                .wrap_err("failed to run soffice via timeout");
        }
    }
    cmd.output().wrap_err("failed to run soffice")
}

/// Set up a LibreOffice macro in a profile directory.
/// Returns the profile dir path.
fn setup_macro(
    profile_dir: &Path,
    macro_name: &str,
    macro_body: &str,
    check_marker: &str,
) -> Result<()> {
    let macro_dir = profile_dir.join("user/basic/Standard");
    let module_path = macro_dir.join("Module1.xba");

    // Check if macro already exists
    if module_path.exists() {
        let content = fs::read_to_string(&module_path)?;
        if content.contains(check_marker) {
            return Ok(());
        }
    }

    // Initialize profile if macro dir doesn't exist
    if !macro_dir.exists() {
        let env_uri = format!(
            "file://{}",
            profile_dir.to_string_lossy().replace(' ', "%20")
        );
        let _ = run_soffice_cmd(
            &[
                "--headless",
                &format!("-env:UserInstallation={env_uri}"),
                "--terminate_after_init",
            ],
            Some(15),
        );
        fs::create_dir_all(&macro_dir)?;
    }

    let macro_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE script:module PUBLIC "-//OpenOffice.org//DTD OfficeDocument 1.0//EN" "module.dtd">
<script:module xmlns:script="http://openoffice.org/2000/script" script:name="Module1" script:language="StarBasic">
{macro_name}
{macro_body}
End Sub
</script:module>"#,
    );
    fs::write(&module_path, macro_xml)?;
    Ok(())
}

// ─── soffice command ───

fn cmd_soffice(args: &[String]) -> Result<()> {
    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = run_soffice_cmd(&str_args, None)?;
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    if output.status.success() || output.status.code() == Some(124) {
        Ok(())
    } else {
        bail!(
            "soffice exited with code {}",
            output.status.code().unwrap_or(-1)
        );
    }
}

// ─── accept-changes command ───

const ACCEPT_CHANGES_MACRO: &str = r#"Sub AcceptAllTrackedChanges
    dim dispatcher as object
    dispatcher = createUnoService("com.sun.star.frame.DispatchHelper")
    dispatcher.executeDispatch(ThisComponent.CurrentController.Frame, ".uno:AcceptAllTrackedChanges", "", 0, Array())
    ThisComponent.store()
    ThisComponent.close(True)"#;

fn cmd_accept_changes(input: &Path, output: &Path) -> Result<()> {
    if !input.exists() {
        bail!("input file not found: {}", input.display());
    }
    let ext = input.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext != "docx" {
        bail!("expected .docx file, got .{ext}");
    }
    // Copy input to output
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(input, output)?;

    let profile_dir = PathBuf::from("/tmp/libreoffice_docx_profile");
    setup_macro(
        &profile_dir,
        "Sub AcceptAllTrackedChanges",
        &ACCEPT_CHANGES_MACRO[ACCEPT_CHANGES_MACRO.find('\n').unwrap() + 1..],
        "AcceptAllTrackedChanges",
    )?;

    let abs_output = fs::canonicalize(output)?;
    let env_uri = format!(
        "file://{}",
        profile_dir.to_string_lossy().replace(' ', "%20")
    );
    let macro_url = "vnd.sun.star.script:Standard.Module1.AcceptAllTrackedChanges?language=Basic&location=application";

    let output_result = run_soffice_cmd(
        &[
            "--headless",
            &format!("-env:UserInstallation={env_uri}"),
            "--norestore",
            macro_url,
            abs_output.to_str().unwrap(),
        ],
        Some(30),
    )?;

    // Timeout (124) is treated as success
    let code = output_result.status.code().unwrap_or(0);
    if code != 0 && code != 124 {
        bail!(
            "soffice failed (exit {}): {}",
            code,
            String::from_utf8_lossy(&output_result.stderr)
        );
    }

    println!("Accepted all tracked changes: {}", output.display());
    Ok(())
}

// ─── recalc command ───

const RECALC_MACRO: &str = r#"Sub RecalculateAndSave
    ThisComponent.calculateAll()
    ThisComponent.store()
    ThisComponent.close(True)"#;

fn cmd_recalc(file: &Path, timeout: u32) -> Result<()> {
    if !file.exists() {
        bail!("file not found: {}", file.display());
    }
    let abs_path = fs::canonicalize(file)?;

    // Platform-specific macro profile dir
    let profile_dir = if cfg!(target_os = "macos") {
        dirs::home_dir()
            .unwrap()
            .join("Library/Application Support/LibreOffice/4")
    } else {
        dirs::home_dir().unwrap().join(".config/libreoffice/4")
    };

    setup_macro(
        &profile_dir,
        "Sub RecalculateAndSave",
        &RECALC_MACRO[RECALC_MACRO.find('\n').unwrap() + 1..],
        "RecalculateAndSave",
    )?;

    let macro_url = "vnd.sun.star.script:Standard.Module1.RecalculateAndSave?language=Basic&location=application";
    let output = run_soffice_cmd(
        &[
            "--headless",
            "--norestore",
            macro_url,
            abs_path.to_str().unwrap(),
        ],
        Some(timeout),
    )?;

    let code = output.status.code().unwrap_or(0);
    if code != 0 && code != 124 {
        bail!(
            "soffice recalc failed (exit {}): {}",
            code,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Scan for formula errors using zip + quick-xml
    let result = scan_formula_errors(&abs_path)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

/// Scan an XLSX file for formula errors after recalculation.
fn scan_formula_errors(file: &Path) -> Result<serde_json::Value> {
    let zip_file = fs::File::open(file)?;
    let mut archive = zip::ZipArchive::new(zip_file)?;

    // Find sheet files
    let sheet_names: Vec<String> = (0..archive.len())
        .filter_map(|i| {
            let entry = archive.by_index(i).ok()?;
            let name = entry.name().to_string();
            if name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml") {
                Some(name)
            } else {
                None
            }
        })
        .collect();

    let error_types = [
        "#VALUE!", "#DIV/0!", "#REF!", "#NAME?", "#NULL!", "#NUM!", "#N/A",
    ];

    let mut total_formulas = 0u64;
    let mut total_errors = 0u64;
    let mut error_summary: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    let cell_re = regex::Regex::new(r#"<c\s[^>]*r="([^"]+)"[^>]*>"#).unwrap();
    let formula_re = regex::Regex::new(r"<f[> ]").unwrap();
    let value_re = regex::Regex::new(r"<v>([^<]*)</v>").unwrap();
    let error_attr_re = regex::Regex::new(r#"t="e""#).unwrap();

    for sheet_name in &sheet_names {
        let mut xml = String::new();
        archive
            .by_name(sheet_name)
            .wrap_err_with(|| format!("reading {sheet_name}"))?
            .read_to_string(&mut xml)?;

        // Extract sheet short name for error locations
        let sheet_label = sheet_name
            .rsplit('/')
            .next()
            .unwrap_or(sheet_name)
            .trim_end_matches(".xml");

        // Process each cell
        for cell_match in cell_re.find_iter(&xml) {
            let cell_start = cell_match.start();
            // Find cell end
            let cell_end = xml[cell_start..]
                .find("</c>")
                .map(|pos| cell_start + pos + 4)
                .unwrap_or(xml.len());
            let cell_xml = &xml[cell_start..cell_end];

            // Extract cell reference
            let cell_ref = cell_re
                .captures(cell_xml)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str())
                .unwrap_or("?");

            // Count formulas
            if formula_re.is_match(cell_xml) {
                total_formulas += 1;
            }

            // Check for errors (t="e" attribute or error string in value)
            if error_attr_re.is_match(cell_xml) {
                if let Some(val_cap) = value_re.captures(cell_xml) {
                    let val = val_cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    for et in &error_types {
                        if val.contains(et) {
                            total_errors += 1;
                            let entry = error_summary.entry(et.to_string()).or_insert_with(
                                || serde_json::json!({"count": 0, "locations": []}),
                            );
                            if let Some(obj) = entry.as_object_mut() {
                                *obj.get_mut("count").unwrap() =
                                    serde_json::json!(obj["count"].as_u64().unwrap_or(0) + 1);
                                if let Some(locs) = obj.get_mut("locations") {
                                    if let Some(arr) = locs.as_array_mut() {
                                        if arr.len() < 20 {
                                            arr.push(serde_json::json!(format!(
                                                "{sheet_label}!{cell_ref}"
                                            )));
                                        }
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(serde_json::json!({
        "status": if total_errors > 0 { "errors_found" } else { "success" },
        "total_errors": total_errors,
        "total_formulas": total_formulas,
        "error_summary": error_summary,
    }))
}

// ─── comment command ───

/// DOCX comment namespace declarations (shared across all 4 comment files).
const COMMENT_NAMESPACES: &str = r#"xmlns:wpc="http://schemas.microsoft.com/office/word/2010/wordprocessingCanvas" xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:o="urn:schemas-microsoft-com:office:office" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math" xmlns:v="urn:schemas-microsoft-com:vml" xmlns:wp14="http://schemas.microsoft.com/office/word/2010/wordprocessingDrawing" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:w10="urn:schemas-microsoft-com:office:word" xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml" xmlns:w16cex="http://schemas.microsoft.com/office/word/2018/wordml/cex" xmlns:w16cid="http://schemas.microsoft.com/office/word/2016/wordml/cid" xmlns:w16="http://schemas.microsoft.com/office/word/2018/wordml" xmlns:w16se="http://schemas.microsoft.com/office/word/2015/wordml/symex" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" mc:Ignorable="w14 w15 w16se w16cid w16 w16cex wp14""#;

fn cmd_comment(
    dir: &Path,
    id: u32,
    text: &str,
    author: &str,
    initials: &str,
    parent: Option<u32>,
) -> Result<()> {
    let word_dir = dir.join("word");
    if !word_dir.exists() {
        bail!("not an unpacked DOCX: {}/word/ not found", dir.display());
    }

    // Generate random IDs
    let para_id = format!("{:08X}", rand_u32());
    let durable_id = format!("{:08X}", rand_u32() & 0x7FFFFFFF);
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let comments_path = word_dir.join("comments.xml");
    let first_comment = !comments_path.exists();

    // Create template files if first comment
    if first_comment {
        fs::write(
            &comments_path,
            format!("<?xml version=\"1.0\" ?>\n<w:comments {COMMENT_NAMESPACES}>\n</w:comments>\n"),
        )?;
        fs::write(
            word_dir.join("commentsExtended.xml"),
            format!(
                "<?xml version=\"1.0\" ?>\n<w15:commentsEx {COMMENT_NAMESPACES}>\n</w15:commentsEx>\n"
            ),
        )?;
        fs::write(
            word_dir.join("commentsIds.xml"),
            format!(
                "<?xml version=\"1.0\" ?>\n<w16cid:commentsIds {COMMENT_NAMESPACES}>\n</w16cid:commentsIds>\n"
            ),
        )?;
        fs::write(
            word_dir.join("commentsExtensible.xml"),
            format!(
                "<?xml version=\"1.0\" ?>\n<w16cex:commentsExtensible {COMMENT_NAMESPACES} xmlns:cr=\"http://schemas.microsoft.com/office/comments/2020/reactions\">\n</w16cex:commentsExtensible>\n"
            ),
        )?;
    }

    // Build comment XML
    let comment_xml = format!(
        r#"<w:comment w:id="{id}" w:author="{author}" w:date="{timestamp}" w:initials="{initials}">
<w:p w14:paraId="{para_id}" w14:textId="77777777" w:rsidR="00000000" w:rsidRDefault="00000000">
<w:pPr><w:pStyle w:val="CommentText"/></w:pPr>
<w:r><w:rPr><w:rStyle w:val="CommentReference"/></w:rPr><w:annotationRef/></w:r>
<w:r><w:rPr><w:color w:val="000000"/><w:sz w:val="20"/><w:szCs w:val="20"/></w:rPr><w:t xml:space="preserve">{text}</w:t></w:r>
</w:p>
</w:comment>"#,
    );

    // Append to comments.xml (insert before closing tag)
    append_before_closing_tag(&comments_path, "</w:comments>", &comment_xml)?;

    // Append to commentsExtended.xml
    let ext_xml = if let Some(parent_id) = parent {
        // Find parent's paraId
        let comments_content = fs::read_to_string(&comments_path)?;
        let parent_para_id = find_comment_para_id(&comments_content, parent_id)
            .ok_or_else(|| eyre::eyre!("parent comment {parent_id} not found"))?;
        format!(r#"<w15:commentEx w15:paraId="{para_id}" w15:paraIdParent="{parent_para_id}"/>"#,)
    } else {
        format!(r#"<w15:commentEx w15:paraId="{para_id}" w15:done="0"/>"#,)
    };
    append_before_closing_tag(
        &word_dir.join("commentsExtended.xml"),
        "</w15:commentsEx>",
        &ext_xml,
    )?;

    // Append to commentsIds.xml
    let cid_xml = format!(
        r#"<w16cid:commentId w16cid:paraId="{para_id}" w16cid:durableId="{durable_id}"/>"#,
    );
    append_before_closing_tag(
        &word_dir.join("commentsIds.xml"),
        "</w16cid:commentsIds>",
        &cid_xml,
    )?;

    // Append to commentsExtensible.xml
    let cex_xml = format!(
        r#"<w16cex:commentExtensible w16cex:durableId="{durable_id}" w16cex:dateUtc="{timestamp}"/>"#,
    );
    append_before_closing_tag(
        &word_dir.join("commentsExtensible.xml"),
        "</w16cex:commentsExtensible>",
        &cex_xml,
    )?;

    // Set up relationships and content types if first comment
    if first_comment {
        setup_comment_rels(dir)?;
        setup_comment_content_types(dir)?;
    }

    let kind = if parent.is_some() { "reply" } else { "comment" };
    println!("Added {kind} {id} (para_id={para_id})");
    println!();
    println!("Add markers to document.xml around the target text:");
    println!(
        r#"  <w:commentRangeStart w:id="{id}"/>  ...text...  <w:commentRangeEnd w:id="{id}"/>"#
    );
    println!(
        r#"  <w:r><w:rPr><w:rStyle w:val="CommentReference"/></w:rPr><w:commentReference w:id="{id}"/></w:r>"#
    );

    Ok(())
}

/// Insert XML text before a closing tag in a file.
fn append_before_closing_tag(path: &Path, closing_tag: &str, xml: &str) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let new_content = content.replace(closing_tag, &format!("{xml}\n{closing_tag}"));
    fs::write(path, new_content)?;
    Ok(())
}

/// Find the paraId of a comment by its w:id attribute.
fn find_comment_para_id(xml: &str, comment_id: u32) -> Option<String> {
    let id_str = format!(r#"w:id="{comment_id}""#);
    let pos = xml.find(&id_str)?;
    // Look for w14:paraId in the paragraph within this comment
    let after = &xml[pos..];
    let para_id_prefix = "w14:paraId=\"";
    let para_pos = after.find(para_id_prefix)?;
    let start = para_pos + para_id_prefix.len();
    let end = after[start..].find('"')? + start;
    Some(after[start..end].to_string())
}

/// Generate a pseudo-random u32 (no external RNG dependency needed).
fn rand_u32() -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::SystemTime;
    let mut hasher = DefaultHasher::new();
    SystemTime::now().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    // Mix with address of a stack variable for more entropy
    let stack_var = 0u8;
    (std::ptr::addr_of!(stack_var) as u64).hash(&mut hasher);
    hasher.finish() as u32
}

/// Add comment relationship entries to word/_rels/document.xml.rels
fn setup_comment_rels(dir: &Path) -> Result<()> {
    let rels_path = dir.join("word/_rels/document.xml.rels");
    let mut content = fs::read_to_string(&rels_path)?;

    // Find max rId
    let re = regex::Regex::new(r#"Id="rId(\d+)""#).unwrap();
    let max_id = re
        .captures_iter(&content)
        .filter_map(|c| c.get(1)?.as_str().parse::<u32>().ok())
        .max()
        .unwrap_or(0);

    let rels = [
        (
            "comments.xml",
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments",
        ),
        (
            "commentsExtended.xml",
            "http://schemas.microsoft.com/office/2011/relationships/commentsExtended",
        ),
        (
            "commentsIds.xml",
            "http://schemas.microsoft.com/office/2016/09/relationships/commentsIds",
        ),
        (
            "commentsExtensible.xml",
            "http://schemas.microsoft.com/office/2018/08/relationships/commentsExtensible",
        ),
    ];

    for (i, (target, rel_type)) in rels.iter().enumerate() {
        let rid = max_id + 1 + i as u32;
        let rel_xml =
            format!(r#"<Relationship Id="rId{rid}" Type="{rel_type}" Target="{target}"/>"#,);
        content = content.replace("</Relationships>", &format!("{rel_xml}\n</Relationships>"));
    }

    fs::write(&rels_path, content)?;
    Ok(())
}

/// Add comment content type entries to [Content_Types].xml
fn setup_comment_content_types(dir: &Path) -> Result<()> {
    let ct_path = dir.join("[Content_Types].xml");
    let mut content = fs::read_to_string(&ct_path)?;

    let overrides = [
        (
            "/word/comments.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml",
        ),
        (
            "/word/commentsExtended.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.commentsExtended+xml",
        ),
        (
            "/word/commentsIds.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.commentsIds+xml",
        ),
        (
            "/word/commentsExtensible.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.commentsExtensible+xml",
        ),
    ];

    for (part, content_type) in &overrides {
        let override_xml =
            format!(r#"<Override PartName="{part}" ContentType="{content_type}"/>"#,);
        content = content.replace("</Types>", &format!("{override_xml}\n</Types>"));
    }

    fs::write(&ct_path, content)?;
    Ok(())
}

// ─── thumbnail command ───

/// Embedded 5x7 bitmap font for ASCII characters 32-126 (space through ~).
/// Each character is 5 pixels wide and 7 pixels tall, stored as 7 bytes per char.
/// Each byte represents one row, with bits 4..0 being left-to-right pixels.
const FONT_5X7: &[[u8; 7]; 95] = &[
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // space
    [0x04, 0x04, 0x04, 0x04, 0x00, 0x04, 0x00], // !
    [0x0A, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x00], // "
    [0x0A, 0x1F, 0x0A, 0x1F, 0x0A, 0x00, 0x00], // #
    [0x04, 0x0F, 0x14, 0x0E, 0x05, 0x1E, 0x04], // $
    [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03], // %
    [0x08, 0x14, 0x08, 0x15, 0x12, 0x0D, 0x00], // &
    [0x04, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00], // '
    [0x02, 0x04, 0x04, 0x04, 0x04, 0x02, 0x00], // (
    [0x08, 0x04, 0x04, 0x04, 0x04, 0x08, 0x00], // )
    [0x04, 0x15, 0x0E, 0x15, 0x04, 0x00, 0x00], // *
    [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00], // +
    [0x00, 0x00, 0x00, 0x00, 0x04, 0x04, 0x08], // ,
    [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00], // -
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00], // .
    [0x01, 0x02, 0x04, 0x08, 0x10, 0x00, 0x00], // /
    [0x0E, 0x11, 0x13, 0x15, 0x19, 0x0E, 0x00], // 0
    [0x04, 0x0C, 0x04, 0x04, 0x04, 0x0E, 0x00], // 1
    [0x0E, 0x11, 0x01, 0x06, 0x08, 0x1F, 0x00], // 2
    [0x0E, 0x11, 0x02, 0x01, 0x11, 0x0E, 0x00], // 3
    [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x00], // 4
    [0x1F, 0x10, 0x1E, 0x01, 0x11, 0x0E, 0x00], // 5
    [0x06, 0x08, 0x1E, 0x11, 0x11, 0x0E, 0x00], // 6
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x00], // 7
    [0x0E, 0x11, 0x0E, 0x11, 0x11, 0x0E, 0x00], // 8
    [0x0E, 0x11, 0x11, 0x0F, 0x02, 0x0C, 0x00], // 9
    [0x00, 0x04, 0x00, 0x00, 0x04, 0x00, 0x00], // :
    [0x00, 0x04, 0x00, 0x00, 0x04, 0x04, 0x08], // ;
    [0x02, 0x04, 0x08, 0x04, 0x02, 0x00, 0x00], // <
    [0x00, 0x00, 0x1F, 0x00, 0x1F, 0x00, 0x00], // =
    [0x08, 0x04, 0x02, 0x04, 0x08, 0x00, 0x00], // >
    [0x0E, 0x11, 0x02, 0x04, 0x00, 0x04, 0x00], // ?
    [0x0E, 0x11, 0x17, 0x15, 0x17, 0x10, 0x0E], // @
    [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x00], // A
    [0x1E, 0x11, 0x1E, 0x11, 0x11, 0x1E, 0x00], // B
    [0x0E, 0x11, 0x10, 0x10, 0x11, 0x0E, 0x00], // C
    [0x1E, 0x11, 0x11, 0x11, 0x11, 0x1E, 0x00], // D
    [0x1F, 0x10, 0x1E, 0x10, 0x10, 0x1F, 0x00], // E
    [0x1F, 0x10, 0x1E, 0x10, 0x10, 0x10, 0x00], // F
    [0x0E, 0x11, 0x10, 0x17, 0x11, 0x0E, 0x00], // G
    [0x11, 0x11, 0x1F, 0x11, 0x11, 0x11, 0x00], // H
    [0x0E, 0x04, 0x04, 0x04, 0x04, 0x0E, 0x00], // I
    [0x01, 0x01, 0x01, 0x01, 0x11, 0x0E, 0x00], // J
    [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11], // K
    [0x10, 0x10, 0x10, 0x10, 0x10, 0x1F, 0x00], // L
    [0x11, 0x1B, 0x15, 0x11, 0x11, 0x11, 0x00], // M
    [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x00], // N
    [0x0E, 0x11, 0x11, 0x11, 0x11, 0x0E, 0x00], // O
    [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x00], // P
    [0x0E, 0x11, 0x11, 0x15, 0x12, 0x0D, 0x00], // Q
    [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x00], // R
    [0x0E, 0x11, 0x10, 0x0E, 0x01, 0x1E, 0x00], // S
    [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x00], // T
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x0E, 0x00], // U
    [0x11, 0x11, 0x11, 0x0A, 0x0A, 0x04, 0x00], // V
    [0x11, 0x11, 0x11, 0x15, 0x1B, 0x11, 0x00], // W
    [0x11, 0x0A, 0x04, 0x04, 0x0A, 0x11, 0x00], // X
    [0x11, 0x0A, 0x04, 0x04, 0x04, 0x04, 0x00], // Y
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x1F, 0x00], // Z
    [0x0E, 0x08, 0x08, 0x08, 0x08, 0x0E, 0x00], // [
    [0x10, 0x08, 0x04, 0x02, 0x01, 0x00, 0x00], // backslash
    [0x0E, 0x02, 0x02, 0x02, 0x02, 0x0E, 0x00], // ]
    [0x04, 0x0A, 0x11, 0x00, 0x00, 0x00, 0x00], // ^
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x1F, 0x00], // _
    [0x08, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00], // `
    [0x00, 0x0E, 0x01, 0x0F, 0x11, 0x0F, 0x00], // a
    [0x10, 0x10, 0x1E, 0x11, 0x11, 0x1E, 0x00], // b
    [0x00, 0x0E, 0x11, 0x10, 0x11, 0x0E, 0x00], // c
    [0x01, 0x01, 0x0F, 0x11, 0x11, 0x0F, 0x00], // d
    [0x00, 0x0E, 0x11, 0x1F, 0x10, 0x0E, 0x00], // e
    [0x06, 0x08, 0x1C, 0x08, 0x08, 0x08, 0x00], // f
    [0x00, 0x0F, 0x11, 0x0F, 0x01, 0x0E, 0x00], // g
    [0x10, 0x10, 0x1E, 0x11, 0x11, 0x11, 0x00], // h
    [0x04, 0x00, 0x0C, 0x04, 0x04, 0x0E, 0x00], // i
    [0x02, 0x00, 0x02, 0x02, 0x12, 0x0C, 0x00], // j
    [0x10, 0x12, 0x14, 0x18, 0x14, 0x12, 0x00], // k
    [0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E, 0x00], // l
    [0x00, 0x1A, 0x15, 0x15, 0x11, 0x11, 0x00], // m
    [0x00, 0x1E, 0x11, 0x11, 0x11, 0x11, 0x00], // n
    [0x00, 0x0E, 0x11, 0x11, 0x11, 0x0E, 0x00], // o
    [0x00, 0x1E, 0x11, 0x1E, 0x10, 0x10, 0x00], // p
    [0x00, 0x0F, 0x11, 0x0F, 0x01, 0x01, 0x00], // q
    [0x00, 0x16, 0x19, 0x10, 0x10, 0x10, 0x00], // r
    [0x00, 0x0F, 0x10, 0x0E, 0x01, 0x1E, 0x00], // s
    [0x08, 0x1C, 0x08, 0x08, 0x09, 0x06, 0x00], // t
    [0x00, 0x11, 0x11, 0x11, 0x13, 0x0D, 0x00], // u
    [0x00, 0x11, 0x11, 0x0A, 0x0A, 0x04, 0x00], // v
    [0x00, 0x11, 0x11, 0x15, 0x15, 0x0A, 0x00], // w
    [0x00, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x00], // x
    [0x00, 0x11, 0x11, 0x0F, 0x01, 0x0E, 0x00], // y
    [0x00, 0x1F, 0x02, 0x04, 0x08, 0x1F, 0x00], // z
    [0x02, 0x04, 0x0C, 0x04, 0x04, 0x02, 0x00], // {
    [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x00], // |
    [0x08, 0x04, 0x06, 0x04, 0x04, 0x08, 0x00], // }
    [0x00, 0x00, 0x0D, 0x12, 0x00, 0x00, 0x00], // ~
];

const FONT_CHAR_W: u32 = 6; // 5 pixels + 1 spacing
const FONT_CHAR_H: u32 = 9; // 7 pixels + 2 spacing

/// Draw text at arbitrary scale using the embedded 5x7 bitmap font.
/// Scale 1 = 5x7 pixels per char, scale 2 = 10x14, scale 8 = 40x56, etc.
fn draw_text_scaled(
    img: &mut image::RgbImage,
    x: u32,
    y: u32,
    text: &str,
    color: image::Rgb<u8>,
    scale: u32,
) {
    let (w, h) = img.dimensions();
    let scale = scale.max(1);
    for (ci, ch) in text.chars().enumerate() {
        let idx = ch as u32;
        if !(32..=126).contains(&idx) {
            continue;
        }
        let glyph = &FONT_5X7[(idx - 32) as usize];
        for row in 0..7u32 {
            for col in 0..5u32 {
                if glyph[row as usize] & (0x10 >> col) != 0 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let px = x + ci as u32 * FONT_CHAR_W * scale + col * scale + dx;
                            let py = y + row * scale + dy;
                            if px < w && py < h {
                                img.put_pixel(px, py, color);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Draw text at 2x scale (convenience wrapper for thumbnails).
fn draw_text_2x(img: &mut image::RgbImage, x: u32, y: u32, text: &str, color: image::Rgb<u8>) {
    draw_text_scaled(img, x, y, text, color, 2);
}

const THUMBNAIL_WIDTH: u32 = 300;
const CONVERSION_DPI: u32 = 100;
const MAX_COLS: u32 = 6;
const GRID_PADDING: u32 = 20;
const BORDER_WIDTH: u32 = 2;
const LABEL_HEIGHT: u32 = 20;

fn cmd_thumbnail(file: &Path, output_prefix: &str, cols: u32) -> Result<()> {
    if !file.exists() {
        bail!("file not found: {}", file.display());
    }
    let cols = cols.clamp(1, MAX_COLS);

    // Parse presentation to get slide info and hidden status
    let zip_file = fs::File::open(file)?;
    let mut archive = zip::ZipArchive::new(zip_file)?;

    // Get slide order from presentation.xml
    let pres_xml = read_zip_entry(&mut archive, "ppt/presentation.xml")?;
    let rels_xml = read_zip_entry(&mut archive, "ppt/_rels/presentation.xml.rels")?;

    let rid_to_slide = parse_rels_to_slides(&rels_xml);
    let slide_info = parse_slide_order(&pres_xml, &rid_to_slide);

    if slide_info.is_empty() {
        bail!("no slides found in presentation");
    }

    // Convert PPTX to PDF then to images using soffice + pdftoppm
    let temp_dir = tempfile::tempdir()?;
    let temp_pptx = temp_dir.path().join("input.pptx");
    fs::copy(file, &temp_pptx)?;

    // soffice --headless --convert-to pdf
    let pdf_output = run_soffice_cmd(
        &[
            "--headless",
            "--convert-to",
            "pdf",
            "--outdir",
            temp_dir.path().to_str().unwrap(),
            temp_pptx.to_str().unwrap(),
        ],
        Some(60),
    )?;

    let pdf_path = temp_dir.path().join("input.pdf");
    if !pdf_path.exists() {
        let stderr = String::from_utf8_lossy(&pdf_output.stderr);
        bail!("soffice failed to convert to PDF: {stderr}");
    }

    // pdftoppm -jpeg -r DPI pdf prefix
    let img_prefix = temp_dir.path().join("slide");
    let pdftoppm_output = Command::new("pdftoppm")
        .args([
            "-jpeg",
            "-r",
            &CONVERSION_DPI.to_string(),
            pdf_path.to_str().unwrap(),
            img_prefix.to_str().unwrap(),
        ])
        .output()
        .wrap_err("pdftoppm not found (install poppler)")?;

    if !pdftoppm_output.status.success() {
        bail!(
            "pdftoppm failed: {}",
            String::from_utf8_lossy(&pdftoppm_output.stderr)
        );
    }

    // Collect converted images (pdftoppm names them slide-01.jpg, slide-02.jpg, etc.)
    let mut slide_images: Vec<(String, Option<image::RgbImage>)> = Vec::new();

    for (i, (slide_name, hidden)) in slide_info.iter().enumerate() {
        if *hidden {
            slide_images.push((slide_name.clone(), None)); // placeholder for hidden
        } else {
            // pdftoppm outputs slide-01.jpg, slide-02.jpg, ... (1-indexed)
            let page_num = i + 1;
            let img_path = temp_dir.path().join(format!("slide-{page_num:02}.jpg"));
            // Try alternate naming (some versions use different padding)
            let img = if img_path.exists() {
                Some(image::open(&img_path)?.to_rgb8())
            } else {
                let alt = temp_dir.path().join(format!("slide-{page_num}.jpg"));
                if alt.exists() {
                    Some(image::open(&alt)?.to_rgb8())
                } else {
                    // Skip this slide if image not found
                    None
                }
            };
            slide_images.push((slide_name.clone(), img));
        }
    }

    // Filter to only slides with images or hidden placeholders
    let visible_count = slide_images.iter().filter(|(_, img)| img.is_some()).count();
    if visible_count == 0 {
        bail!("no slide images were generated (check soffice and pdftoppm)");
    }

    // Determine thumbnail dimensions from first available image
    let (thumb_w, thumb_h) = {
        let first_img = slide_images
            .iter()
            .find_map(|(_, img)| img.as_ref())
            .unwrap();
        let aspect = first_img.height() as f64 / first_img.width() as f64;
        (THUMBNAIL_WIDTH, (THUMBNAIL_WIDTH as f64 * aspect) as u32)
    };

    // Create placeholder for hidden slides
    let placeholder = create_hidden_placeholder(thumb_w, thumb_h);

    // Build grid(s)
    let max_per_grid = (cols * (cols + 1)) as usize;
    let chunks: Vec<_> = slide_images.chunks(max_per_grid).collect();
    let mut output_files = Vec::new();

    for (chunk_idx, chunk) in chunks.iter().enumerate() {
        let rows = (chunk.len() as u32).div_ceil(cols);
        let cell_w = GRID_PADDING + thumb_w + BORDER_WIDTH * 2;
        let cell_h = LABEL_HEIGHT + thumb_h + BORDER_WIDTH * 2;
        let grid_w = GRID_PADDING + cols * cell_w;
        let grid_h = GRID_PADDING + rows * cell_h;

        let mut grid = image::RgbImage::from_pixel(grid_w, grid_h, image::Rgb([255, 255, 255]));

        for (i, (name, img)) in chunk.iter().enumerate() {
            let col = i as u32 % cols;
            let row = i as u32 / cols;
            let x = GRID_PADDING + col * cell_w;
            let y = GRID_PADDING + row * cell_h;

            // Draw label
            draw_text_2x(&mut grid, x, y, name, image::Rgb([80, 80, 80]));

            // Get thumbnail image
            let thumb = if let Some(img) = img {
                image::imageops::resize(
                    img,
                    thumb_w,
                    thumb_h,
                    image::imageops::FilterType::Lanczos3,
                )
            } else {
                placeholder.clone()
            };

            // Draw border
            let bx = x;
            let by = y + LABEL_HEIGHT;
            let border_color = image::Rgb([180, 180, 180]);
            for bw in 0..BORDER_WIDTH {
                // Top and bottom borders
                for px in bx..bx + thumb_w + BORDER_WIDTH * 2 {
                    if px < grid_w {
                        if by + bw < grid_h {
                            grid.put_pixel(px, by + bw, border_color);
                        }
                        let bot = by + BORDER_WIDTH + thumb_h + bw;
                        if bot < grid_h {
                            grid.put_pixel(px, bot, border_color);
                        }
                    }
                }
                // Left and right borders
                for py in by..by + thumb_h + BORDER_WIDTH * 2 {
                    if py < grid_h {
                        if bx + bw < grid_w {
                            grid.put_pixel(bx + bw, py, border_color);
                        }
                        let right = bx + BORDER_WIDTH + thumb_w + bw;
                        if right < grid_w {
                            grid.put_pixel(right, py, border_color);
                        }
                    }
                }
            }

            // Draw thumbnail
            let tx = bx + BORDER_WIDTH;
            let ty = by + BORDER_WIDTH;
            for py in 0..thumb_h {
                for px in 0..thumb_w {
                    if tx + px < grid_w && ty + py < grid_h {
                        grid.put_pixel(tx + px, ty + py, *thumb.get_pixel(px, py));
                    }
                }
            }
        }

        // Save grid
        let filename = if chunks.len() == 1 {
            format!("{output_prefix}.jpg")
        } else {
            format!("{output_prefix}-{}.jpg", chunk_idx + 1)
        };
        grid.save_with_format(&filename, image::ImageFormat::Jpeg)?;
        output_files.push(filename);
    }

    for f in &output_files {
        println!("{f}");
    }
    Ok(())
}

/// Parse .rels XML to build rId -> slide filename mapping.
fn parse_rels_to_slides(rels_xml: &str) -> HashMap<String, String> {
    let re =
        regex::Regex::new(r#"<Relationship[^>]*Id="([^"]+)"[^>]*Target="slides/([^"]+)"[^/]*/?>"#)
            .unwrap();
    let re2 =
        regex::Regex::new(r#"<Relationship[^>]*Target="slides/([^"]+)"[^>]*Id="([^"]+)"[^/]*/?>"#)
            .unwrap();
    let mut map = HashMap::new();
    for cap in re.captures_iter(rels_xml) {
        map.insert(cap[1].to_string(), cap[2].to_string());
    }
    for cap in re2.captures_iter(rels_xml) {
        map.entry(cap[2].to_string())
            .or_insert_with(|| cap[1].to_string());
    }
    map
}

/// Parse presentation.xml to get slide order and hidden status.
/// Returns Vec<(slide_filename, is_hidden)>.
fn parse_slide_order(
    pres_xml: &str,
    rid_to_slide: &HashMap<String, String>,
) -> Vec<(String, bool)> {
    let re = regex::Regex::new(r#"<p:sldId[^>]*r:id="([^"]+)"[^>]*/?\s*>"#).unwrap();
    let show_re = regex::Regex::new(r#"show="0""#).unwrap();

    let mut slides = Vec::new();
    for cap in re.captures_iter(pres_xml) {
        let rid = &cap[1];
        let full_match = cap.get(0).unwrap().as_str();
        let hidden = show_re.is_match(full_match);
        if let Some(slide_name) = rid_to_slide.get(rid) {
            slides.push((slide_name.clone(), hidden));
        }
    }
    slides
}

/// Create a placeholder image for hidden slides (gray with X pattern).
fn create_hidden_placeholder(w: u32, h: u32) -> image::RgbImage {
    let mut img = image::RgbImage::from_pixel(w, h, image::Rgb([220, 220, 220]));
    let line_color = image::Rgb([180, 180, 180]);
    // Draw X pattern
    for i in 0..w.max(h) {
        let x1 = (i as f64 * w as f64 / w.max(h) as f64) as u32;
        let y1 = (i as f64 * h as f64 / w.max(h) as f64) as u32;
        if x1 < w && y1 < h {
            img.put_pixel(x1, y1, line_color);
        }
        let x2 = w.saturating_sub(1).saturating_sub(x1);
        if x2 < w && y1 < h {
            img.put_pixel(x2, y1, line_color);
        }
    }
    // Draw "HIDDEN" text centered
    let text = "HIDDEN";
    let text_w = text.len() as u32 * FONT_CHAR_W * 2;
    let tx = w.saturating_sub(text_w) / 2;
    let ty = h.saturating_sub(FONT_CHAR_H * 2) / 2;
    draw_text_2x(&mut img, tx, ty, text, image::Rgb([120, 120, 120]));
    img
}

// ─── PPTX generation (make-slide) ───

/// 1 inch = 914400 EMU (English Metric Units)
const EMU_PER_INCH: f64 = 914_400.0;
/// PowerPoint font size unit: 1 pt = 100 half-points
const PT_TO_HPTS: f64 = 100.0;

/// Text overlay specification matching mofa-pptx `texts` API.
#[derive(serde::Deserialize, Debug)]
struct TextOverlay {
    text: Option<String>,
    runs: Option<Vec<TextRun>>,
    #[serde(default = "default_x")]
    x: f64,
    #[serde(default = "default_y")]
    y: f64,
    #[serde(default = "default_w")]
    w: f64,
    #[serde(default = "default_h")]
    h: f64,
    #[serde(rename = "fontFace")]
    font_face: Option<String>,
    #[serde(rename = "fontSize")]
    font_size: Option<f64>,
    #[serde(default = "default_color")]
    color: String,
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    italic: bool,
    #[serde(default = "default_align")]
    align: String,
    #[serde(default = "default_valign")]
    valign: String,
    rotate: Option<f64>,
}

#[derive(serde::Deserialize, Debug)]
struct TextRun {
    text: String,
    color: Option<String>,
    bold: Option<bool>,
    italic: Option<bool>,
    #[serde(rename = "fontSize")]
    font_size: Option<f64>,
    #[serde(rename = "fontFace")]
    font_face: Option<String>,
    #[serde(rename = "breakLine")]
    break_line: Option<bool>,
}

fn default_x() -> f64 {
    0.5
}
fn default_y() -> f64 {
    0.5
}
fn default_w() -> f64 {
    6.0
}
fn default_h() -> f64 {
    1.0
}
fn default_color() -> String {
    "FFFFFF".into()
}
fn default_align() -> String {
    "l".into()
}
fn default_valign() -> String {
    "t".into()
}

fn inches_to_emu(inches: f64) -> i64 {
    (inches * EMU_PER_INCH).round() as i64
}

fn pptx_align(a: &str) -> &str {
    match a {
        "center" | "c" | "ctr" => "ctr",
        "right" | "r" => "r",
        "justify" | "j" | "just" => "just",
        _ => "l",
    }
}

fn pptx_valign(a: &str) -> &str {
    match a {
        "middle" | "m" | "ctr" => "ctr",
        "bottom" | "b" => "b",
        _ => "t",
    }
}

fn build_run_xml(
    text: &str,
    font_face: &str,
    font_size: f64,
    color: &str,
    bold: bool,
    italic: bool,
) -> String {
    let sz = (font_size * PT_TO_HPTS) as i64;
    let b = if bold { r#" b="1""# } else { "" };
    let i = if italic { r#" i="1""# } else { "" };
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        r#"<a:r><a:rPr lang="en-US" sz="{sz}"{b}{i} dirty="0"><a:solidFill><a:srgbClr val="{color}"/></a:solidFill><a:latin typeface="{font_face}" pitchFamily="34" charset="0"/><a:ea typeface="{font_face}" pitchFamily="34" charset="-122"/><a:cs typeface="{font_face}" pitchFamily="34" charset="-120"/></a:rPr><a:t>{escaped}</a:t></a:r>"#
    )
}

fn build_text_shape_xml(overlay: &TextOverlay, shape_id: u32) -> String {
    let x = inches_to_emu(overlay.x);
    let y = inches_to_emu(overlay.y);
    let w = inches_to_emu(overlay.w);
    let h = inches_to_emu(overlay.h);
    let align = pptx_align(&overlay.align);
    let anchor = pptx_valign(&overlay.valign);
    let font_face = overlay.font_face.as_deref().unwrap_or("Arial");
    let font_size = overlay.font_size.unwrap_or(18.0);

    let rotation = overlay
        .rotate
        .map(|deg| format!(r#" rot="{}""#, (deg * 60000.0) as i64))
        .unwrap_or_default();

    // Build paragraph runs
    let para_content = if let Some(runs) = &overlay.runs {
        let mut xml = String::new();
        for run in runs {
            let rf = run.font_face.as_deref().unwrap_or(font_face);
            let rs = run.font_size.unwrap_or(font_size);
            let rc = run.color.as_deref().unwrap_or(&overlay.color);
            let rb = run.bold.unwrap_or(overlay.bold);
            let ri = run.italic.unwrap_or(overlay.italic);
            if run.break_line == Some(true) {
                xml.push_str(&format!(r#"</a:p><a:p><a:pPr algn="{align}"/>"#));
            }
            xml.push_str(&build_run_xml(&run.text, rf, rs, rc, rb, ri));
        }
        xml
    } else {
        let text = overlay.text.as_deref().unwrap_or("");
        build_run_xml(
            text,
            font_face,
            font_size,
            &overlay.color,
            overlay.bold,
            overlay.italic,
        )
    };

    let end_sz = (font_size * PT_TO_HPTS) as i64;

    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{shape_id}" name="Text {shape_id}"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm{rotation}><a:off x="{x}" y="{y}"/><a:ext cx="{w}" cy="{h}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/><a:ln/></p:spPr><p:txBody><a:bodyPr wrap="square" rtlCol="0" anchor="{anchor}"/><a:lstStyle/><a:p><a:pPr algn="{align}" indent="0" marL="0"><a:buNone/></a:pPr>{para_content}<a:endParaRPr lang="en-US" sz="{end_sz}" dirty="0"/></a:p></p:txBody></p:sp>"#
    )
}

fn cmd_make_slide(
    image_path: &Path,
    output: &Path,
    texts_json: Option<&str>,
    slide_w: f64,
    slide_h: f64,
) -> Result<()> {
    if !image_path.exists() {
        bail!("image not found: {}", image_path.display());
    }

    let overlays: Vec<TextOverlay> = if let Some(json) = texts_json {
        serde_json::from_str(json).wrap_err("invalid texts JSON")?
    } else {
        Vec::new()
    };

    let img_data = fs::read(image_path)?;
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_lowercase();
    let (media_name, content_type) = match ext.as_str() {
        "jpg" | "jpeg" => ("image1.jpeg", "image/jpeg"),
        _ => ("image1.png", "image/png"),
    };

    let sw = inches_to_emu(slide_w);
    let sh = inches_to_emu(slide_h);

    // Build text shape XML
    let mut shapes_xml = String::new();
    for (i, overlay) in overlays.iter().enumerate() {
        shapes_xml.push_str(&build_text_shape_xml(overlay, (i as u32) + 3));
    }

    // ─── Generate all PPTX XML files ───

    let img_ext = if content_type == "image/jpeg" {
        "jpeg"
    } else {
        "png"
    };

    let content_types = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="xml" ContentType="application/xml"/>
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="{img_ext}" ContentType="{content_type}"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
<Override PartName="/ppt/presProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presProps+xml"/>
<Override PartName="/ppt/viewProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.viewProps+xml"/>
<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
<Override PartName="/ppt/tableStyles.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"/>
<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>
<Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>
</Types>"#
    );

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#;

    let presentation = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" saveSubsetFonts="1" autoCompressPictures="0">
<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
<p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst>
<p:sldSz cx="{sw}" cy="{sh}"/>
<p:notesSz cx="{sh}" cy="{sw}"/>
<p:defaultTextStyle><a:defPPr><a:defRPr lang="en-US"/></a:defPPr><a:lvl1pPr marL="0" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl1pPr><a:lvl2pPr marL="457200" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl2pPr><a:lvl3pPr marL="914400" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl3pPr><a:lvl4pPr marL="1371600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl4pPr><a:lvl5pPr marL="1828800" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl5pPr><a:lvl6pPr marL="2286000" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl6pPr><a:lvl7pPr marL="2743200" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl7pPr><a:lvl8pPr marL="3200400" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl8pPr><a:lvl9pPr marL="3657600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl9pPr></p:defaultTextStyle>
</p:presentation>"#
    );

    let pres_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/presProps" Target="presProps.xml"/>
<Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/viewProps" Target="viewProps.xml"/>
<Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/>
<Relationship Id="rId6" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles" Target="tableStyles.xml"/>
</Relationships>"#;

    let slide = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld name="Slide 1">
<p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
<p:pic><p:nvPicPr><p:cNvPr id="2" name="Background"/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{sw}" cy="{sh}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>
{shapes_xml}
</p:spTree>
</p:cSld>
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>
</p:sld>"#
    );

    let slide_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/{media_name}"/>
</Relationships>"#
    );

    let slide_layout = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#;

    let layout_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"#;

    let slide_master = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:bg><p:bgPr><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill><a:effectLst/></p:bgPr></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle><a:lvl1pPr algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:lnSpc><a:spcPct val="90000"/></a:lnSpc><a:spcBef><a:spcPct val="0"/></a:spcBef><a:buNone/><a:defRPr sz="4400" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mj-lt"/><a:ea typeface="+mj-ea"/><a:cs typeface="+mj-cs"/></a:defRPr></a:lvl1pPr></p:titleStyle><p:bodyStyle><a:lvl1pPr marL="228600" indent="-228600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:lnSpc><a:spcPct val="90000"/></a:lnSpc><a:spcBef><a:spcPts val="1000"/></a:spcBef><a:buFont typeface="Arial" panose="020B0604020202020204" pitchFamily="34" charset="0"/><a:buChar char="&#x2022;"/><a:defRPr sz="2800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl1pPr><a:lvl2pPr marL="685800" indent="-228600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:lnSpc><a:spcPct val="90000"/></a:lnSpc><a:spcBef><a:spcPts val="500"/></a:spcBef><a:buFont typeface="Arial" panose="020B0604020202020204" pitchFamily="34" charset="0"/><a:buChar char="&#x2022;"/><a:defRPr sz="2400" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl2pPr><a:lvl3pPr marL="1143000" indent="-228600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:lnSpc><a:spcPct val="90000"/></a:lnSpc><a:spcBef><a:spcPts val="500"/></a:spcBef><a:buFont typeface="Arial" panose="020B0604020202020204" pitchFamily="34" charset="0"/><a:buChar char="&#x2022;"/><a:defRPr sz="2000" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl3pPr><a:lvl4pPr marL="1600200" indent="-228600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:lnSpc><a:spcPct val="90000"/></a:lnSpc><a:spcBef><a:spcPts val="500"/></a:spcBef><a:buFont typeface="Arial" panose="020B0604020202020204" pitchFamily="34" charset="0"/><a:buChar char="&#x2022;"/><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl4pPr><a:lvl5pPr marL="2057400" indent="-228600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:lnSpc><a:spcPct val="90000"/></a:lnSpc><a:spcBef><a:spcPts val="500"/></a:spcBef><a:buFont typeface="Arial" panose="020B0604020202020204" pitchFamily="34" charset="0"/><a:buChar char="&#x2022;"/><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl5pPr><a:lvl6pPr marL="2514600" indent="-228600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:lnSpc><a:spcPct val="90000"/></a:lnSpc><a:spcBef><a:spcPts val="500"/></a:spcBef><a:buFont typeface="Arial" panose="020B0604020202020204" pitchFamily="34" charset="0"/><a:buChar char="&#x2022;"/><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl6pPr><a:lvl7pPr marL="2971800" indent="-228600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:lnSpc><a:spcPct val="90000"/></a:lnSpc><a:spcBef><a:spcPts val="500"/></a:spcBef><a:buFont typeface="Arial" panose="020B0604020202020204" pitchFamily="34" charset="0"/><a:buChar char="&#x2022;"/><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl7pPr><a:lvl8pPr marL="3429000" indent="-228600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:lnSpc><a:spcPct val="90000"/></a:lnSpc><a:spcBef><a:spcPts val="500"/></a:spcBef><a:buFont typeface="Arial" panose="020B0604020202020204" pitchFamily="34" charset="0"/><a:buChar char="&#x2022;"/><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl8pPr><a:lvl9pPr marL="3886200" indent="-228600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:lnSpc><a:spcPct val="90000"/></a:lnSpc><a:spcBef><a:spcPts val="500"/></a:spcBef><a:buFont typeface="Arial" panose="020B0604020202020204" pitchFamily="34" charset="0"/><a:buChar char="&#x2022;"/><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl9pPr></p:bodyStyle><p:otherStyle><a:defPPr><a:defRPr lang="en-US"/></a:defPPr><a:lvl1pPr marL="0" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl1pPr><a:lvl2pPr marL="457200" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl2pPr><a:lvl3pPr marL="914400" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl3pPr><a:lvl4pPr marL="1371600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl4pPr><a:lvl5pPr marL="1828800" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl5pPr><a:lvl6pPr marL="2286000" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl6pPr><a:lvl7pPr marL="2743200" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl7pPr><a:lvl8pPr marL="3200400" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl8pPr><a:lvl9pPr marL="3657600" algn="l" defTabSz="914400" rtl="0" eaLnBrk="1" latinLnBrk="0" hangingPunct="1"><a:defRPr sz="1800" kern="1200"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/><a:ea typeface="+mn-ea"/><a:cs typeface="+mn-cs"/></a:defRPr></a:lvl9pPr></p:otherStyle></p:txStyles></p:sldMaster>"#;

    let master_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>"#;

    let theme = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office Theme">
<a:themeElements>
<a:clrScheme name="Office"><a:dk1><a:srgbClr val="000000"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="1F497D"/></a:dk2><a:lt2><a:srgbClr val="EEECE1"/></a:lt2><a:accent1><a:srgbClr val="4F81BD"/></a:accent1><a:accent2><a:srgbClr val="C0504D"/></a:accent2><a:accent3><a:srgbClr val="9BBB59"/></a:accent3><a:accent4><a:srgbClr val="8064A2"/></a:accent4><a:accent5><a:srgbClr val="4BACC6"/></a:accent5><a:accent6><a:srgbClr val="F79646"/></a:accent6><a:hlink><a:srgbClr val="0000FF"/></a:hlink><a:folHlink><a:srgbClr val="800080"/></a:folHlink></a:clrScheme>
<a:fontScheme name="Office"><a:majorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme>
<a:fmtScheme name="Office"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme>
</a:themeElements>
<a:objectDefaults/><a:extraClrSchemeLst/>
</a:theme>"#;

    let pres_props = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentationPr xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>"#;

    let view_props = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:viewPr xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:normalViewPr horzBarState="maximized"><p:restoredLeft sz="15611"/><p:restoredTop sz="94610"/></p:normalViewPr>
<p:slideViewPr><p:cSldViewPr snapToGrid="0" snapToObjects="1"><p:cViewPr varScale="1"><p:scale><a:sx n="136" d="100"/><a:sy n="136" d="100"/></p:scale><p:origin x="216" y="312"/></p:cViewPr><p:guideLst/></p:cSldViewPr></p:slideViewPr>
<p:notesTextViewPr><p:cViewPr><p:scale><a:sx n="1" d="1"/><a:sy n="1" d="1"/></p:scale><p:origin x="0" y="0"/></p:cViewPr></p:notesTextViewPr>
<p:gridSpacing cx="76200" cy="76200"/>
</p:viewPr>"#;

    let table_styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:tblStyleLst xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" def="{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}"/>"#;

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");

    let core_props = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:dcmitype="http://purl.org/dc/dcmitype/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
<dc:title>Presentation</dc:title>
<dc:creator>octos</dc:creator>
<cp:lastModifiedBy>octos</cp:lastModifiedBy>
<cp:revision>1</cp:revision>
<dcterms:created xsi:type="dcterms:W3CDTF">{now}</dcterms:created>
<dcterms:modified xsi:type="dcterms:W3CDTF">{now}</dcterms:modified>
</cp:coreProperties>"#
    );

    let app_props = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes">
<TotalTime>0</TotalTime>
<Words>0</Words>
<Application>octos office</Application>
<PresentationFormat>On-screen Show (16:9)</PresentationFormat>
<Paragraphs>0</Paragraphs>
<Slides>1</Slides>
<Notes>0</Notes>
<HiddenSlides>0</HiddenSlides>
<MMClips>0</MMClips>
<ScaleCrop>false</ScaleCrop>
<LinksUpToDate>false</LinksUpToDate>
<SharedDoc>false</SharedDoc>
<HyperlinksChanged>false</HyperlinksChanged>
<AppVersion>16.0000</AppVersion>
</Properties>"#;

    // ─── Pack into ZIP ───
    let out_file = fs::File::create(output)?;
    let mut zip = zip::ZipWriter::new(out_file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let xml_files: &[(&str, &str)] = &[
        ("[Content_Types].xml", &content_types),
        ("_rels/.rels", root_rels),
        ("docProps/app.xml", app_props),
        ("docProps/core.xml", &core_props),
        ("ppt/presentation.xml", &presentation),
        ("ppt/_rels/presentation.xml.rels", pres_rels),
        ("ppt/presProps.xml", pres_props),
        ("ppt/viewProps.xml", view_props),
        ("ppt/tableStyles.xml", table_styles),
        ("ppt/slides/slide1.xml", &slide),
        ("ppt/slides/_rels/slide1.xml.rels", &slide_rels),
        ("ppt/slideLayouts/slideLayout1.xml", slide_layout),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", layout_rels),
        ("ppt/slideMasters/slideMaster1.xml", slide_master),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", master_rels),
        ("ppt/theme/theme1.xml", theme),
    ];

    for (name, content) in xml_files {
        zip.start_file(*name, opts)?;
        zip.write_all(content.as_bytes())?;
    }

    // Media file
    zip.start_file(format!("ppt/media/{media_name}"), opts)?;
    zip.write_all(&img_data)?;

    zip.finish()?;
    println!("{}", output.display());
    Ok(())
}

fn parse_rgb(s: &str) -> Result<image::Rgb<u8>> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        bail!("color must be R,G,B (e.g. 255,255,255)");
    }
    Ok(image::Rgb([
        parts[0].trim().parse::<u8>().wrap_err("bad red value")?,
        parts[1].trim().parse::<u8>().wrap_err("bad green value")?,
        parts[2].trim().parse::<u8>().wrap_err("bad blue value")?,
    ]))
}

#[allow(clippy::too_many_arguments)]
fn cmd_overlay_text(
    image_path: &Path,
    text: &str,
    x: u32,
    y: u32,
    scale: u32,
    color: &str,
    shadow: Option<&str>,
    output: Option<&Path>,
) -> Result<()> {
    if !image_path.exists() {
        bail!("image not found: {}", image_path.display());
    }
    let color = parse_rgb(color)?;
    let shadow_color = shadow.map(parse_rgb).transpose()?;
    let scale = scale.clamp(1, 32);

    let mut img = image::open(image_path)
        .wrap_err_with(|| format!("failed to open image: {}", image_path.display()))?
        .to_rgb8();

    // Handle multi-line text (split on \n literal or actual newlines)
    let lines: Vec<&str> = text.split("\\n").flat_map(|s| s.split('\n')).collect();
    let line_height = FONT_CHAR_H * scale;

    for (li, line) in lines.iter().enumerate() {
        let ly = y + li as u32 * line_height;
        // Draw shadow first (offset +2 scaled pixels)
        if let Some(sc) = shadow_color {
            let offset = (scale / 2).max(1);
            draw_text_scaled(&mut img, x + offset, ly + offset, line, sc, scale);
        }
        draw_text_scaled(&mut img, x, ly, line, color, scale);
    }

    let out_path = output.unwrap_or(image_path);
    let fmt = match out_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "png" => image::ImageFormat::Png,
        _ => image::ImageFormat::Jpeg,
    };
    img.save_with_format(out_path, fmt)?;
    println!("{}", out_path.display());
    Ok(())
}

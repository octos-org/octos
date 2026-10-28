//! Native, no-runtime Office document generation for the bundled `office` skill.

use std::path::{Path, PathBuf};

use eyre::Result;
use serde::Deserialize;

pub mod docx;
mod ooxml;
pub mod pptx;
pub mod xlsx;

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum OfficeInput {
    Docx(docx::DocxInput),
    Xlsx(xlsx::XlsxInput),
    Pptx(pptx::PptxInput),
}

/// Generate one OOXML package and return its absolute workspace path.
pub fn generate(input: OfficeInput, workspace_root: &Path) -> Result<PathBuf> {
    match input {
        OfficeInput::Docx(input) => docx::generate(input, workspace_root),
        OfficeInput::Xlsx(input) => xlsx::generate(input, workspace_root),
        OfficeInput::Pptx(input) => pptx::generate(input, workspace_root),
    }
}

pub fn workspace_root() -> Result<PathBuf> {
    match std::env::var_os("OCTOS_SESSION_WORKSPACE") {
        Some(path) if !path.is_empty() => Ok(PathBuf::from(path)),
        _ => Ok(std::env::current_dir()?),
    }
}

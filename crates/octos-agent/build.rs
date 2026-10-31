//! Embed portable skill resources without relying on the installer's file layout.
use std::{env, fs, path::Path};

fn collect(root: &Path, dir: &Path, files: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read resource skill directory") {
        let entry = entry.expect("resource entry");
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "__pycache__" || name.ends_with(".pyc") {
            continue;
        }
        let kind = entry.file_type().expect("resource type");
        assert!(!kind.is_symlink(), "resource symlinks must not be shipped");
        if kind.is_dir() {
            collect(root, &path, files);
        } else if kind.is_file() {
            assert!(
                matches!(
                    path.extension().and_then(|s| s.to_str()),
                    Some("md" | "py" | "txt" | "yaml")
                ),
                "unexpected resource: {}",
                path.display()
            );
            files.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .replace('\\', "/"),
            );
        }
    }
}

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let root = Path::new(&manifest).join("resource-skills");
    println!("cargo:rerun-if-changed=resource-skills");
    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    files.sort();
    let mut source = String::from("pub(crate) const RESOURCE_FILES: &[(&str, &[u8])] = &[\n");
    for relative in files {
        let absolute = root.join(&relative);
        source.push_str(&format!(
            "({relative:?}, include_bytes!({:?})),\n",
            absolute.to_str().unwrap()
        ));
    }
    source.push_str("];\n");
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("resource_skills.rs"),
        source,
    )
    .unwrap();
}

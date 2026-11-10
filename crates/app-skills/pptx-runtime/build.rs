fn main() {
    println!("cargo:rerun-if-env-changed=CRAFT_FONTS_DIR");
    println!("cargo:rerun-if-env-changed=CRAFT_FONTS_REQUIRED");
    let ready = std::env::var_os("CRAFT_FONTS_DIR").is_some_and(|p| {
        [
            "manifest.txt",
            "latin-manifest.txt",
            "simplified-chinese-manifest.txt",
        ]
        .iter()
        .all(|name| std::path::Path::new(&p).join("fonts").join(name).is_file())
    });
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        assert!(
            ready && std::env::var_os("CRAFT_FONTS_REQUIRED").is_some(),
            "release PPTX builds require: source scripts/prepare-pptx-fonts.sh"
        );
    }
    println!("cargo:rustc-env=OCTOS_PPTX_BUNDLED_FONTS={ready}");
}

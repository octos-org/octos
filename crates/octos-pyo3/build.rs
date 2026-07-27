//! Build script: let `cargo test --features python` run against a libpython
//! whose install name is `@rpath/...` — e.g. Anaconda/Miniconda (and Homebrew)
//! on macOS, whose `Py_ENABLE_SHARED=0` sysconfig makes pyo3 skip emitting an
//! rpath even though a `.dylib` is shipped, so the test binary aborts at load
//! with "Library not loaded".
//!
//! It is a strict NO-OP unless the `python` feature is on, so the default
//! `cargo build/test/clippy --workspace` (including octos CI's Python-less
//! Windows lane) does ZERO Python probing and pulls no libpython.
//!
//! ## Interpreter resolution (review item C.1)
//!
//! The review suggested `pyo3_build_config::get().lib_dir` for the rpath.
//! `get()` requires pyo3-build-config's `resolve-config` feature, whose OWN
//! build script resolves an interpreter AT COMPILE TIME and `exit(1)`s when
//! none is found. As a (non-optional) build-dependency that would make the
//! default `cargo build --workspace` FAIL on a Python-less lane — precisely
//! what the feature-gating (Change A) exists to prevent — and a build script
//! cannot feature-gate an optional dependency (features reach build scripts
//! only as `CARGO_FEATURE_*` env vars, never as `cfg`). So instead we replicate
//! pyo3's OWN resolution ORDER here, guarded so it runs only when `python` is
//! on: an explicit `PYO3_CONFIG_FILE`, else `PYO3_PYTHON`, else `python3`. That
//! points the rpath at the same libpython pyo3 links, with no extra dependency
//! and no compile-time probing.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // pyo3's interpreter resolution keys off these; rebuild the rpath if they
    // change.
    println!("cargo:rerun-if-env-changed=PYO3_PYTHON");
    println!("cargo:rerun-if-env-changed=PYO3_CONFIG_FILE");

    // (Change A) Default workspace build (no `python` feature): do NOTHING — no
    // Python probing, no rpath. Keeps a Python-less CI lane green.
    if std::env::var_os("CARGO_FEATURE_PYTHON").is_none() {
        return;
    }
    // Wheel builds (extension-module) resolve Python symbols from the host
    // interpreter at import; no libpython is linked, so no rpath is needed and
    // the shippable cdylib stays free of a builder-absolute rpath.
    if std::env::var_os("CARGO_FEATURE_EXTENSION_MODULE").is_some() {
        return;
    }
    // The `-Wl,-rpath,` flag is a Unix (clang/gcc) concept; the MSVC linker
    // rejects it and pyo3 discovers pythonXX.dll another way.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        return;
    }

    let Some(lib_dir) = resolve_lib_dir() else {
        // abi3 config file without a lib_dir, or no resolvable interpreter:
        // nothing to point an rpath at.
        return;
    };

    // (Change C.3) Validate before emitting: an absolute path with no CR/LF
    // (a newline could inject a second `cargo:` directive) and no comma
    // (which could smuggle extra `-Wl` args into the linker invocation). On any
    // doubt, skip quietly rather than panic the build.
    if !is_safe_rpath(&lib_dir) {
        println!("cargo:warning=octos-pyo3: skipping test rpath; unexpected lib_dir");
        return;
    }

    // Add lib_dir as an rpath so dyld/ld.so resolves `@rpath/libpython3.x.dylib`
    // (or the Linux equivalent) at load time for the test binary.
    //
    // (Change C.2 note) The review asked to scope this to test binaries via
    // `cargo:rustc-link-arg-tests`. Empirically that variant applies ONLY to
    // integration `[[test]]`/bench targets, NOT to the inline lib unit-test
    // binary (verified: it still aborts with `@rpath/libpython... not loaded`).
    // Converting these unit tests to integration tests would force a public
    // `pub use pyo3` re-export plus pub-ifying the internal mappers/constructors
    // — a worse API-hygiene outcome than the issue it fixes. So we keep plain
    // `rustc-link-arg`. Its only extra footprint is that a `cargo build
    // --features python` dev `cdylib` also carries this rpath — an ephemeral,
    // git-ignored, never-shipped artifact. Every artifact that matters is
    // already clean: the wheel skips this whole script (extension-module guard
    // above) and the default no-`python` build skips it too (guard at top).
    println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}");
}

/// Resolve the libpython directory pyo3 links, following pyo3's OWN order:
/// an explicit `PYO3_CONFIG_FILE` (its `lib_dir=` line), else the `PYO3_PYTHON`
/// / `python3` interpreter's `sysconfig` `LIBDIR`.
fn resolve_lib_dir() -> Option<String> {
    if let Some(cfg_file) = std::env::var_os("PYO3_CONFIG_FILE") {
        // pyo3 prefers an explicit config file over any interpreter; match that.
        let contents = std::fs::read_to_string(&cfg_file).ok()?;
        return contents.lines().find_map(|line| {
            line.strip_prefix("lib_dir=")
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        });
    }
    let python = std::env::var("PYO3_PYTHON").unwrap_or_else(|_| "python3".to_string());
    let output = Command::new(&python)
        .arg("-c")
        .arg("import sysconfig; print(sysconfig.get_config_var('LIBDIR') or '')")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let lib_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!lib_dir.is_empty()).then_some(lib_dir)
}

/// An rpath is safe to emit only if it is an absolute path free of characters
/// that could break out of the single `cargo:`/linker directive.
fn is_safe_rpath(p: &str) -> bool {
    std::path::Path::new(p).is_absolute() && !p.contains(['\r', '\n', ','])
}

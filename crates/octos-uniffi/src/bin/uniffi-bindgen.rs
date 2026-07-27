//! In-crate `uniffi-bindgen` CLI.
//!
//! Delegates to uniffi's own bindgen entry point (behind the `cli` feature), so
//! the exact uniffi version this crate compiles against is the one that
//! generates the bindings — no separately-installed `uniffi-bindgen` to keep in
//! lockstep. Generate the Python bindings with:
//!
//! ```text
//! cargo build -p octos-uniffi
//! cargo run -p octos-uniffi --bin uniffi-bindgen -- generate \
//!     --library target/debug/liboctos_uniffi.dylib \
//!     --language python \
//!     --out-dir crates/octos-uniffi/bindings/python
//! ```
//!
//! Swift and Kotlin generate identically — swap `--language swift` / `kotlin`.

fn main() {
    uniffi::uniffi_bindgen_main()
}

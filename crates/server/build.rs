//! Build script: guard the embedded WebUI contract.
//!
//! When the `webui` feature is enabled, `src/webui.rs` embeds
//! `../../webui/dist` via `rust-embed`. If the frontend has not been
//! built yet that directory (and its `index.html`) will be missing,
//! producing a confusing runtime 404 instead of a clear signal. We
//! emit a loud `cargo:warning` here so the operator knows to run
//! `cd webui && npm run build` before compiling.

use std::path::Path;

fn main() {
    // Only relevant when the webui feature is active.
    if std::env::var_os("CARGO_FEATURE_WEBUI").is_none() {
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let index = Path::new(&manifest_dir).join("../../webui/dist/index.html");

    // Re-run if the built index appears/disappears.
    println!("cargo:rerun-if-changed=../../webui/dist/index.html");

    if !index.exists() {
        println!(
            "cargo:warning=webui feature is enabled but webui/dist/index.html is missing. \
             Build the frontend first: `cd webui && npm install && npm run build`."
        );
    }
}

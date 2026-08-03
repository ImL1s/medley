use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=GROK_VERSION");
    println!("cargo:rerun-if-env-changed=MEDLEY_CHANNEL");

    // The triple this binary is being built for. Cargo sets `TARGET` for every
    // build script; there is no runtime equivalent that reports the full
    // triple, only the pieces (`std::env::consts::{ARCH, OS}`), which cannot
    // distinguish gnu from musl or darwin from ios.
    if let Ok(target) = std::env::var("TARGET") {
        println!("cargo:rustc-env=MEDLEY_BUILD_TARGET={target}");
    }

    // The upstream commit this tree was synced from. Recorded in `SOURCE_REV`
    // at the workspace root and embedded here so a published binary can name
    // its base without the repository being present — which is the whole point
    // of asking a *binary* what it was built from.
    if let Some(path) = upstream_rev_path() {
        // Only a path that resolves, and only one Cargo can print. A missing
        // `rerun-if-changed` entry is treated as permanently dirty, which
        // rebuilds this crate and everything downstream on every invocation
        // (see #87 for what that cost).
        if let Some(printable) = path.to_str() {
            println!("cargo:rerun-if-changed={printable}");
        }
        if let Ok(rev) = std::fs::read_to_string(&path) {
            let rev = rev.trim();
            if !rev.is_empty() {
                println!("cargo:rustc-env=MEDLEY_UPSTREAM_BASE={rev}");
            }
        }
    }
}

/// `SOURCE_REV`, found by walking up from this package to the workspace root.
///
/// Walked rather than hardcoded as `../../../SOURCE_REV`: a relative literal
/// silently resolves to nothing if this crate ever moves, and "silently
/// resolves to nothing" is exactly how the version stamp would go stale
/// without anyone noticing. Returns `None` outside a checkout — a vendored or
/// packaged build has no `SOURCE_REV`, and reporting no upstream base is
/// correct there.
fn upstream_rev_path() -> Option<PathBuf> {
    let mut dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    loop {
        let candidate = dir.join("SOURCE_REV");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

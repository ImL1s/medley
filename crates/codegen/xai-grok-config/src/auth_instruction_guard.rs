//! A reusable source scan that stops auth instructions naming a hardcoded
//! program.
//!
//! Lives in this crate so each crate that carries user-facing auth copy can run
//! the same scan over its own `src/`, rather than one crate trying to guard
//! source it cannot see.
//!
//! Every one of these strings is upstream's, so an upstream sync reintroduces
//! them by construction. Without a guard the sweep in #117 regresses silently
//! at the next merge.

/// A scan result. Non-empty `hits` fails the calling test.
pub struct Scan {
    /// `file:line -> source line`, one per offending line.
    pub hits: Vec<String>,
    /// Files actually read. Zero means the scan proved nothing.
    pub files_scanned: usize,
}

/// Marker a line may carry to opt out, for text that legitimately names the
/// other program — a fixture standing in for server-supplied copy, say, which
/// this product does not rewrite.
pub const EXEMPTION: &str = "auth-instruction-guard: exempt";

/// Scan `src_dir` recursively for auth instructions naming a literal program.
///
/// The needles are assembled at runtime so this file does not match itself.
pub fn scan(src_dir: &std::path::Path) -> Scan {
    // Split so the guard's own source carries none of them whole.
    let g = "grok";
    let needles: Vec<String> = ["login", "logout", "auth ", "setup"]
        .iter()
        .flat_map(|verb| {
            // Backtick, single-quote, and bare forms: the bare form is how
            // `grok login --device-code` appeared on a continuation line, which
            // a backtick-only needle misses.
            [
                format!("`{g} {verb}"),
                format!("'{g} {verb}"),
                format!("{g} {verb}"),
            ]
        })
        .collect();

    let mut out = Scan {
        hits: Vec::new(),
        files_scanned: 0,
    };
    walk(src_dir, &needles, &mut out);
    out
}

fn walk(dir: &std::path::Path, needles: &[String], out: &mut Scan) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, needles, out);
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        out.files_scanned += 1;
        let lines: Vec<&str> = content.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            // Doc and line comments describe behaviour rather than instructing
            // the user; block-comment bodies conventionally start with `*`.
            if trimmed.starts_with("//") || trimmed.starts_with('*') {
                continue;
            }
            // The marker may sit on the line itself or on the one above it:
            // these literals are usually long, and forcing the marker inline
            // makes the exemption harder to read than the thing it exempts.
            let exempt = line.contains(EXEMPTION)
                || idx
                    .checked_sub(1)
                    .and_then(|prev| lines.get(prev))
                    .is_some_and(|prev| prev.contains(EXEMPTION));
            if exempt {
                continue;
            }
            if needles.iter().any(|n| line.contains(n.as_str())) {
                out.hits
                    .push(format!("{}:{} -> {}", path.display(), idx + 1, trimmed));
            }
        }
    }
}

/// Run the scan for the calling crate and fail with an actionable message.
///
/// `src_dir` should be `concat!(env!("CARGO_MANIFEST_DIR"), "/src")`.
///
/// Fails when the scan reads **no files**, which is the failure mode that
/// matters most: a test binary run without its source tree — `cargo nextest
/// archive` executed on another host, a container carrying only `target/` —
/// would otherwise scan nothing and report success.
pub fn assert_no_hardcoded_auth_instructions(src_dir: &str) {
    let scan = scan(std::path::Path::new(src_dir));
    assert!(
        scan.files_scanned > 0,
        "the auth-instruction guard read no files under {src_dir}. It proves nothing in this \
         environment rather than passing: run it where the source tree is present, or the guard \
         is silently disabled."
    );
    assert!(
        scan.hits.is_empty(),
        "auth instructions naming a hardcoded program ({} file(s) scanned):\n{}\n\nUse \
         `program_name_for_instruction()` — not `program_name()` — for anything the user is meant \
         to type: it returns `None` when `argv[0]` gave nothing usable, and the fallback names a \
         *different program that may be installed*. Add `{EXEMPTION}` to the line if it is text \
         this product does not own, such as a fixture for a server-supplied message.",
        scan.files_scanned,
        scan.hits.join("\n")
    );
}

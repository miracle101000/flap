//! Generates a Dart SDK from one or more OpenAPI specs and writes every file
//! to disk. Each spec gets its own subdirectory under the shared output root.
//!
//! Usage:
//!   cargo run --example gen_dart -- --out <out-dir> <spec> [<spec> ...]
//!
//! Examples:
//!   # Single local file (unchanged behaviour)
//!   cargo run --example gen_dart -- \
//!     --out /tmp/sdks \
//!     tests/fixtures/petstore.yaml
//!
//!   # Multiple specs in one run
//!   cargo run --example gen_dart -- \
//!     --out /tmp/sdks \
//!     tests/fixtures/petstore.yaml \
//!     tests/fixtures/secure_petstore.yaml \
//!     https://petstore3.swagger.io/api/v3/openapi.yaml

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    // ── Argument parsing ─────────────────────────────────────────────────────
    // Expected: --out <dir> <spec> [<spec> ...]
    let (out_dir, specs) = match parse_args(&args) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("error: {msg}");
            eprintln!("usage: gen_dart --out <out-dir> <spec> [<spec> ...]");
            return ExitCode::from(2);
        }
    };

    if specs.is_empty() {
        eprintln!("error: at least one spec path or URL is required");
        eprintln!("usage: gen_dart --out <out-dir> <spec> [<spec> ...]");
        return ExitCode::from(2);
    }

    // ── Per-spec generation ──────────────────────────────────────────────────
    let mut any_failed = false;

    for spec in &specs {
        println!("\n── {spec} ──");

        let api = match flap_spec::load_path_or_url(spec) {
            Ok(api) => api,
            Err(e) => {
                eprintln!("  error loading spec: {e:#}");
                any_failed = true;
                continue; // keep going; generate what we can
            }
        };

        // Derive a safe directory name from the spec path or URL stem.
        let subdir_name = spec_to_dir_name(spec);
        
        for (mode, suffix) in [
            (flap_emit_dart::NullSafety::Safe, "null_safe"),
            (flap_emit_dart::NullSafety::Unsafe, "null_unsafe"),
        ] {
            let spec_out = out_dir.join(&subdir_name);

            if let Err(e) = fs::create_dir_all(&spec_out) {
                eprintln!("  error creating {}: {e}", spec_out.display());
                any_failed = true;
                continue;
            }

            // Models
            let models = flap_emit_dart::emit_models(&api, mode);
            let mut filenames: Vec<&String> = models.keys().collect();
            filenames.sort();
            for filename in filenames {
                let path = spec_out.join(filename);
                if let Err(e) = fs::write(&path, &models[filename]) {
                    eprintln!("  error writing {}: {e}", path.display());
                    any_failed = true;
                    continue;
                }
                println!("  wrote {}", path.display());
            }

            // Client
            let (client_filename, client_src) = flap_emit_dart::emit_client(&api, mode);
            let client_path = spec_out.join(&client_filename);
            if let Err(e) = fs::write(&client_path, &client_src) {
                eprintln!("  error writing {}: {e}", client_path.display());
                any_failed = true;
                continue;
            }
            println!("  wrote {}", client_path.display());

            println!(
                "  [{suffix}] {} model file(s) + 1 client → {}",
                models.len(),
                spec_out.display()
            );
        }
    }

    if any_failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parse `--out <dir> <spec> [<spec> ...]` from the raw argument list.
fn parse_args(args: &[String]) -> Result<(PathBuf, Vec<String>), String> {
    let mut out_dir: Option<PathBuf> = None;
    let mut specs: Vec<String> = Vec::new();
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--out" | "-o" => {
                i += 1;
                let dir = args
                    .get(i)
                    .ok_or_else(|| "--out requires a directory argument".to_string())?;
                out_dir = Some(PathBuf::from(dir));
            }
            arg if arg.starts_with("--out=") => {
                out_dir = Some(PathBuf::from(&arg["--out=".len()..]));
            }
            other => {
                specs.push(other.to_string());
            }
        }
        i += 1;
    }

    let out_dir = out_dir.ok_or_else(|| "--out <dir> is required".to_string())?;
    Ok((out_dir, specs))
}

/// Convert a spec path or URL into a safe single-directory-component name.
///
/// Examples:
///   "tests/fixtures/petstore.yaml"               → "petstore"
///   "https://example.com/api/v3/openapi.yaml"    → "openapi"
///   "https://example.com/api/v3/openapi.json"    → "openapi"
fn spec_to_dir_name(spec: &str) -> String {
    // Strip query string and fragment for URLs.
    let without_suffix = spec
        .split('?')
        .next()
        .unwrap_or(spec)
        .split('#')
        .next()
        .unwrap_or(spec);

    // Take just the final path component.
    let basename = without_suffix
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(without_suffix);

    // Strip known extensions.
    let stem = basename
        .strip_suffix(".yaml")
        .or_else(|| basename.strip_suffix(".yml"))
        .or_else(|| basename.strip_suffix(".json"))
        .unwrap_or(basename);

    // Sanitise: replace anything that isn't alphanumeric, `-`, or `_` with `_`.
    let sanitised: String = stem
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if sanitised.is_empty() {
        "spec".to_string()
    } else {
        sanitised
    }
}

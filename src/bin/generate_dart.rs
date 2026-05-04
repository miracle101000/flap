//! Generates a Dart SDK from one or more OpenAPI specs and writes every file
//! to disk. Each spec gets its own subdirectory under the shared output root.
//!
//! Usage:
//!   cargo run --bin generate_dart -- --out <out-dir> [--force] [--client=dio|http] <spec> [<spec> ...]
//!
//! Examples:
//!   # Single local file, Dio client (default)
//!   cargo run --bin generate_dart -- \
//!     --out ./sdks \
//!     tests/fixtures/petstore.yaml
//!
//!   # http package client
//!   cargo run --bin generate_dart -- \
//!     --out ./sdks \
//!     --client=http \
//!     tests/fixtures/petstore.yaml
//!
//!   # Multiple specs, force regeneration
//!   cargo run --bin generate_dart -- \
//!     --out ./sdks \
//!     --force \
//!     tests/fixtures/petstore.yaml \
//!     https://petstore3.swagger.io/api/v3/openapi.yaml

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::UNIX_EPOCH;

use flap_emit_dart::{ClientBackend, MappingConfig, NullSafety, TemplateConfig};

const FLAP_VERSION: &str = env!("CARGO_PKG_VERSION");
const LOCK_FILE: &str = ".flap.lock";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    let (out_dir, specs, force, backend, mappings, templates) = match parse_args(&args) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("error: {msg}");
            eprintln!(
                "usage: gen_dart --out <out-dir> [--force] [--client=dio|http]\n\
                 \t[--type-map=Schema=DartType] [--import-map=DartType=package:...]\n\
                 \t[--template-dir=<dir>] <spec> [<spec> ...]"
            );
            return ExitCode::from(2);
        }
    };

    if specs.is_empty() {
        eprintln!("error: at least one spec path or URL is required");
        return ExitCode::from(2);
    }

    let backend_label = match backend {
        ClientBackend::Dio  => "dio",
        ClientBackend::Http => "http",
    };
    println!("client backend : {backend_label}");
    if !mappings.is_empty() {
        for (k, v) in &mappings.type_map   { println!("  type-map     : {k} → {v}"); }
        for (k, v) in &mappings.import_map { println!("  import-map   : {k} → {v}"); }
    }
    if let Some(dir) = &templates.template_dir {
        println!("template dir   : {}", dir.display());
    }

    let mut any_failed = false;

    for spec in &specs {
        println!("\n── {spec} ──");

        let fingerprint = local_fingerprint(spec, backend, &mappings, &templates);

        let api = match flap_spec::load_path_or_url(spec) {
            Ok(api) => api,
            Err(e) => {
                eprintln!("  error loading spec: {e:#}");
                any_failed = true;
                continue;
            }
        };

        let subdir_name = spec_to_dir_name(spec);

        for (mode, suffix) in [
            (NullSafety::Safe,   "null_safe"),
            (NullSafety::Unsafe, "null_unsafe"),
        ] {
            let spec_out = out_dir.join(&subdir_name);

            if let Err(e) = fs::create_dir_all(&spec_out) {
                eprintln!("  error creating {}: {e}", spec_out.display());
                any_failed = true;
                continue;
            }

            let lock_path = spec_out.join(format!("{LOCK_FILE}.{suffix}.{backend_label}"));
            if !force {
                if let Some(ref fp) = fingerprint {
                    if read_lock(&lock_path).as_deref() == Some(fp.as_str()) {
                        println!("  [{suffix}/{backend_label}] unchanged — skipping");
                        continue;
                    }
                }
            }

            let models = flap_emit_dart::emit_models(&api, mode, &mappings, &templates);
            let mut filenames: Vec<&String> = models.keys().collect();
            filenames.sort();
            let mut write_ok = true;
            for filename in filenames {
                let path = spec_out.join(filename);
                if let Err(e) = fs::write(&path, &models[filename]) {
                    eprintln!("  error writing {}: {e}", path.display());
                    any_failed = true;
                    write_ok = false;
                    continue;
                }
                println!("  wrote {}", path.display());
            }

            let (client_filename, client_src) =
                flap_emit_dart::emit_client(&api, mode, backend, &mappings, &templates);
            let client_path = spec_out.join(&client_filename);
            if let Err(e) = fs::write(&client_path, &client_src) {
                eprintln!("  error writing {}: {e}", client_path.display());
                any_failed = true;
                write_ok = false;
            } else {
                println!("  wrote {}", client_path.display());
            }

            println!(
                "  [{suffix}/{backend_label}] {} model file(s) + 1 client → {}",
                models.len(),
                spec_out.display()
            );

            if write_ok {
                if let Some(ref fp) = fingerprint {
                    write_lock(&lock_path, fp);
                }
            }
        }
    }

    if any_failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse `--out <dir> [--force] [--client=dio|http] <spec> [<spec> ...]`.
fn parse_args(
    args: &[String],
) -> Result<(PathBuf, Vec<String>, bool, ClientBackend, MappingConfig, TemplateConfig), String> {
    let mut out_dir: Option<PathBuf> = None;
    let mut specs: Vec<String> = Vec::new();
    let mut force = false;
    let mut backend = ClientBackend::Dio;
    let mut mappings = MappingConfig::default();
    let mut templates = TemplateConfig::default();
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
            "--force" | "-f" => {
                force = true;
            }
            "--client=dio" => { backend = ClientBackend::Dio; }
            "--client=http" => { backend = ClientBackend::Http; }
            arg if arg.starts_with("--client=") => {
                let val = &arg["--client=".len()..];
                return Err(format!("unknown client backend `{val}` — expected `dio` or `http`"));
            }
            arg if arg.starts_with("--type-map=") => {
                let pair = &arg["--type-map=".len()..];
                let (k, v) = pair.split_once('=').ok_or_else(|| {
                    format!("--type-map requires KEY=VALUE, got `{pair}`")
                })?;
                mappings.type_map.insert(k.to_string(), v.to_string());
            }
            arg if arg.starts_with("--import-map=") => {
                let pair = &arg["--import-map=".len()..];
                let (k, v) = pair.split_once('=').ok_or_else(|| {
                    format!("--import-map requires KEY=VALUE, got `{pair}`")
                })?;
                mappings.import_map.insert(k.to_string(), v.to_string());
            }
            "--template-dir" | "-t" => {
                i += 1;
                let dir = args
                    .get(i)
                    .ok_or_else(|| "--template-dir requires a path argument".to_string())?;
                templates.template_dir = Some(PathBuf::from(dir));
            }
            arg if arg.starts_with("--template-dir=") => {
                templates.template_dir = Some(PathBuf::from(&arg["--template-dir=".len()..]));
            }
            other => { specs.push(other.to_string()); }
        }
        i += 1;
    }

    let out_dir = out_dir.ok_or_else(|| "--out <dir> is required".to_string())?;
    Ok((out_dir, specs, force, backend, mappings, templates))
}

/// Fingerprint a local spec file using mtime + size + flap version + backend.
///
/// Including the backend means switching from `--client=dio` to `--client=http`
/// invalidates the lockfile and forces regeneration even when the spec is
/// unchanged — correct because the client file content differs between backends.
///
/// Returns `None` for remote URLs — those always regenerate.
/// Fingerprint includes template file contents so that editing a template
/// invalidates the lockfile even when the spec is unchanged.
fn local_fingerprint(
    spec: &str,
    backend: ClientBackend,
    mappings: &MappingConfig,
    templates: &TemplateConfig,
) -> Option<String> {
    if spec.starts_with("http://") || spec.starts_with("https://") {
        return None;
    }
    let meta = std::fs::metadata(spec).ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let size = meta.len();

    let backend_tag = match backend {
        ClientBackend::Dio  => "dio",
        ClientBackend::Http => "http",
    };

    let mut type_pairs: Vec<String> =
        mappings.type_map.iter().map(|(k, v)| format!("{k}={v}")).collect();
    type_pairs.sort();
    let mut import_pairs: Vec<String> =
        mappings.import_map.iter().map(|(k, v)| format!("{k}={v}")).collect();
    import_pairs.sort();
    let mappings_tag = format!("t:[{}]i:[{}]", type_pairs.join(","), import_pairs.join(","));

    // Hash every template file's content so edits are detected.
    let template_hash = if let Some(dir) = &templates.template_dir {
        let mut entries: Vec<(String, String)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let (Some(name), Ok(content)) = (
                        path.file_name().and_then(|n| n.to_str()).map(str::to_string),
                        std::fs::read_to_string(&path),
                    ) {
                        entries.push((name, content));
                    }
                }
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        // Simple hash: sum of byte lengths + joined names.
        // Cheap and sufficient — a content change changes the length or text.
        let summary: String = entries
            .iter()
            .map(|(name, content)| format!("{name}:{}", content.len()))
            .collect::<Vec<_>>()
            .join(",");
        format!("tmpl:[{summary}]")
    } else {
        "tmpl:[]".to_string()
    };

    Some(format!(
        "flap:{FLAP_VERSION}|backend:{backend_tag}|{mappings_tag}|{template_hash}|mtime:{mtime}|size:{size}"
    ))
}

fn read_lock(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn write_lock(path: &std::path::Path, fingerprint: &str) {
    let _ = std::fs::write(path, fingerprint);
}

/// Convert a spec path or URL into a safe single-directory-component name.
///
/// Examples:
///   "tests/fixtures/petstore.yaml"            → "petstore"
///   "https://example.com/api/v3/openapi.yaml" → "openapi"
fn spec_to_dir_name(spec: &str) -> String {
    let without_suffix = spec
        .split('?')
        .next()
        .unwrap_or(spec)
        .split('#')
        .next()
        .unwrap_or(spec);

    let basename = without_suffix
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(without_suffix);

    let stem = basename
        .strip_suffix(".yaml")
        .or_else(|| basename.strip_suffix(".yml"))
        .or_else(|| basename.strip_suffix(".json"))
        .unwrap_or(basename);

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

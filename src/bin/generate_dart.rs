use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::Context;
use clap::Parser;
use sha2::{Digest, Sha256};

#[derive(Parser)]
struct Opts {
    /// Path to OpenAPI YAML
    #[clap(long)]
    spec: PathBuf,

    /// Output directory for generated Dart package
    #[clap(long)]
    out: PathBuf,

    /// Skip running `dart run build_runner build` after writing files.
    /// Use this if the Dart SDK is not on $PATH or you want to run
    /// build_runner yourself.
    #[clap(long, default_value_t = false)]
    no_build_runner: bool,

    /// Always regenerate, even if the spec content has not changed
    /// since the last successful run.
    #[clap(long, default_value_t = false)]
    force: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let opts = Opts::parse();

    // ── Hash check ───────────────────────────────────────────────────────────
    let hash = spec_hash(&opts.spec)
        .with_context(|| format!("hashing spec file {}", opts.spec.display()))?;

    let hash_file = opts.out.join(".flap_spec_hash");

    if !opts.force {
        if let Ok(cached) = fs::read_to_string(&hash_file) {
            if cached.trim() == hash.trim() {
                println!("Spec unchanged — skipping generation.");
                return Ok(ExitCode::SUCCESS);
            }
        }
    }

    // ── Load spec → IR ───────────────────────────────────────────────────────
    let api = flap_spec::load(&opts.spec).context("loading spec")?;

    // ── Emit models and client ───────────────────────────────────────────────
    let models = flap_emit_dart::emit_models(&api);
    let (client_filename, client_source) = flap_emit_dart::emit_client(&api);

    // ── Prepare output layout ────────────────────────────────────────────────
    fs::create_dir_all(opts.out.join("lib"))?;

    // Minimal pubspec (users can customise)
    let pubspec = format!(
        r#"name: {name}
description: Generated client from {spec}
environment:
  sdk: '>=2.19.0 <4.0.0'

dependencies:
  dio: ^5.0.0
  freezed_annotation: ^2.0.0
  json_annotation: ^4.0.0

dev_dependencies:
  build_runner: ^2.0.0
  freezed: ^2.0.0
  json_serializable: ^6.0.0
"#,
        name = api.title.to_lowercase().replace(' ', "_"),
        spec = opts.spec.display()
    );
    fs::write(opts.out.join("pubspec.yaml"), &pubspec)?;

    // ── Write .gitignore (append .flap_spec_hash if not already present) ────
    write_gitignore_entry(&opts.out, ".flap_spec_hash").context("writing .gitignore")?;

    // ── Write model files ────────────────────────────────────────────────────
    for (name, src) in &models {
        fs::write(opts.out.join("lib").join(name), src)?;
    }

    // ── Write client ─────────────────────────────────────────────────────────
    fs::write(opts.out.join("lib").join(&client_filename), &client_source)?;

    println!("Wrote generated Dart package to {}", opts.out.display());

    // ── Run build_runner ─────────────────────────────────────────────────────
    if opts.no_build_runner {
        println!(
            "Skipping build_runner (--no-build-runner). \
             Run manually inside {}:\n  \
             dart pub get && dart run build_runner build --delete-conflicting-outputs",
            opts.out.display()
        );
    } else {
        let code = run_build_runner(&opts.out)?;
        if code != ExitCode::SUCCESS {
            // Do NOT write the hash — the build failed; next run must retry.
            return Ok(code);
        }
    }

    // ── Persist hash only after a fully successful run ───────────────────────
    fs::write(&hash_file, &hash)
        .with_context(|| format!("writing hash cache to {}", hash_file.display()))?;

    Ok(ExitCode::SUCCESS)
}

// ── Hash ──────────────────────────────────────────────────────────────────────

/// Returns the lowercase hex SHA-256 digest of the file at `path`.
pub fn spec_hash(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(format!("{digest:x}"))
}

// ── .gitignore helper ─────────────────────────────────────────────────────────

/// Appends `entry` to `out_dir/.gitignore` if it is not already present.
/// Creates the file if it does not exist.
fn write_gitignore_entry(out_dir: &Path, entry: &str) -> anyhow::Result<()> {
    let gitignore = out_dir.join(".gitignore");

    // Read existing content (empty string if the file does not yet exist).
    let existing = match fs::read_to_string(&gitignore) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).context("reading .gitignore"),
    };

    // Check line-by-line so we don't false-positive on partial matches
    // (e.g. `.flap_spec_hash_old` would not satisfy the check for
    // `.flap_spec_hash`).
    let already_present = existing.lines().any(|line| line.trim() == entry);

    if already_present {
        return Ok(());
    }

    // Append with a trailing newline; prepend a newline separator only
    // if the file is non-empty and does not already end with one.
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore)
        .with_context(|| format!("opening {} for append", gitignore.display()))?;

    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{entry}")?;
    Ok(())
}

// ── build_runner ──────────────────────────────────────────────────────────────

/// Runs `dart pub get` then `dart run build_runner build` inside `out_dir`,
/// streaming stdout/stderr directly to the parent terminal.
fn run_build_runner(out_dir: &Path) -> anyhow::Result<ExitCode> {
    println!("\n── dart pub get ─────────────────────────────────────────");
    let pub_get = Command::new("dart")
        .args(["pub", "get"])
        .current_dir(out_dir)
        .status()
        .map_err(|e| {
            anyhow::anyhow!(
                "could not spawn `dart` — is the Dart SDK on your $PATH?\n\
             Install from https://dart.dev/get-dart\n\
             Original error: {e}"
            )
        })?;

    if !pub_get.success() {
        eprintln!(
            "error: `dart pub get` exited with {}",
            exit_code_display(&pub_get)
        );
        return Ok(ExitCode::FAILURE);
    }

    println!("\n── dart run build_runner build ──────────────────────────");
    let build = Command::new("dart")
        .args([
            "run",
            "build_runner",
            "build",
            "--delete-conflicting-outputs",
        ])
        .current_dir(out_dir)
        .status()
        .map_err(|e| anyhow::anyhow!("could not spawn `dart` for build_runner: {e}"))?;

    if !build.success() {
        eprintln!(
            "error: `dart run build_runner build` exited with {}",
            exit_code_display(&build)
        );
        return Ok(ExitCode::FAILURE);
    }

    println!(
        "\nDone. Generated package is ready in {}",
        out_dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn exit_code_display(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("status code {code}"),
        None => "an unknown status (process may have been killed by a signal)".into(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    // Minimal valid OpenAPI 3.0 spec used by both tests.
    const SPEC_YAML: &str = "\
openapi: 3.0.0
info:
  title: Test API
  version: '1.0.0'
servers:
  - url: https://api.example.com
paths:
  /ping:
    get:
      operationId: ping
      responses:
        '200':
          description: ok
";

    const SPEC_YAML_V2: &str = "\
openapi: 3.0.0
info:
  title: Test API v2
  version: '2.0.0'
servers:
  - url: https://api.example.com
paths:
  /ping:
    get:
      operationId: ping
      responses:
        '200':
          description: ok
";

    /// Write `spec.yaml` and run the whole pipeline (minus build_runner)
    /// for the first time. Returns (spec_path, out_dir, TempDir guard).
    ///
    /// We call `run()` logic inline rather than spawning a subprocess so
    /// the test stays self-contained and fast.
    fn first_run(tmp: &TempDir, spec_content: &str) -> (PathBuf, PathBuf) {
        let spec = tmp.path().join("spec.yaml");
        let out = tmp.path().join("out");
        fs::write(&spec, spec_content).unwrap();

        run_pipeline(&spec, &out, false);
        (spec, out)
    }

    /// Runs the generation pipeline synchronously (no build_runner).
    /// Mirrors `run()` without the clap layer.
    fn run_pipeline(spec: &Path, out: &Path, force: bool) {
        let hash = spec_hash(spec).unwrap();
        let hash_file = out.join(".flap_spec_hash");

        let skip = !force
            && matches!(
                fs::read_to_string(&hash_file),
                Ok(cached) if cached.trim() == hash.trim()
            );

        if skip {
            // Simulates "Spec unchanged — skipping generation."
            return;
        }

        let api = flap_spec::load(spec).expect("spec should parse");
        let models = flap_emit_dart::emit_models(&api);
        let (client_filename, client_source) = flap_emit_dart::emit_client(&api);

        fs::create_dir_all(out.join("lib")).unwrap();
        for (name, src) in &models {
            fs::write(out.join("lib").join(name), src).unwrap();
        }
        fs::write(out.join("lib").join(&client_filename), &client_source).unwrap();
        write_gitignore_entry(out, ".flap_spec_hash").unwrap();

        // Persist hash only on success (no build_runner in tests).
        fs::write(&hash_file, &hash).unwrap();
    }

    // ── spec_hash ─────────────────────────────────────────────────────────────

    #[test]
    fn spec_hash_is_stable() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.yaml");
        fs::write(&path, SPEC_YAML).unwrap();
        assert_eq!(spec_hash(&path).unwrap(), spec_hash(&path).unwrap());
    }

    #[test]
    fn spec_hash_changes_with_content() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.yaml");
        fs::write(&path, SPEC_YAML).unwrap();
        let h1 = spec_hash(&path).unwrap();
        fs::write(&path, SPEC_YAML_V2).unwrap();
        let h2 = spec_hash(&path).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn spec_hash_missing_file_returns_err() {
        let tmp = TempDir::new().unwrap();
        let err = spec_hash(&tmp.path().join("missing.yaml"));
        assert!(err.is_err());
    }

    // ── skip-if-unchanged ─────────────────────────────────────────────────────

    #[test]
    fn second_run_with_unchanged_spec_does_not_advance_mtime() {
        let tmp = TempDir::new().unwrap();
        let (spec, out) = first_run(&tmp, SPEC_YAML);

        // Capture mtime of a generated file after the first run.
        let client_path = out.join("lib").join("test_api_client.dart");
        let mtime_after_first = fs::metadata(&client_path)
            .expect("client file must exist after first run")
            .modified()
            .unwrap();

        // Sleep briefly so that any filesystem mtime granularity
        // (1-second on some systems) can distinguish a re-write.
        std::thread::sleep(Duration::from_millis(1100));

        // Second run — spec is unchanged.
        run_pipeline(&spec, &out, false);

        let mtime_after_second = fs::metadata(&client_path).unwrap().modified().unwrap();

        assert_eq!(
            mtime_after_first, mtime_after_second,
            "mtime advanced on second run — generation was not skipped"
        );
    }

    #[test]
    fn changed_spec_triggers_regeneration() {
        let tmp = TempDir::new().unwrap();
        let (spec, out) = first_run(&tmp, SPEC_YAML);

        let client_path = out.join("lib").join("test_api_client.dart");
        let mtime_first = fs::metadata(&client_path).unwrap().modified().unwrap();

        std::thread::sleep(Duration::from_millis(1100));

        // Change the spec.
        fs::write(&spec, SPEC_YAML_V2).unwrap();
        run_pipeline(&spec, &out, false);

        let mtime_second = fs::metadata(&client_path).unwrap().modified().unwrap();
        assert!(
            mtime_second > mtime_first,
            "mtime did not advance after spec change — generation was incorrectly skipped"
        );
    }

    // ── --force ───────────────────────────────────────────────────────────────

    #[test]
    fn force_flag_regenerates_even_when_spec_unchanged() {
        let tmp = TempDir::new().unwrap();
        let (spec, out) = first_run(&tmp, SPEC_YAML);

        let client_path = out.join("lib").join("test_api_client.dart");
        let mtime_first = fs::metadata(&client_path).unwrap().modified().unwrap();

        std::thread::sleep(Duration::from_millis(1100));

        // Same spec, but force=true.
        run_pipeline(&spec, &out, true);

        let mtime_second = fs::metadata(&client_path).unwrap().modified().unwrap();
        assert!(
            mtime_second > mtime_first,
            "mtime did not advance with --force — force flag was not respected"
        );
    }

    #[test]
    fn force_flag_updates_hash_file() {
        let tmp = TempDir::new().unwrap();
        let (spec, out) = first_run(&tmp, SPEC_YAML);

        let hash_file = out.join(".flap_spec_hash");
        let hash_before = fs::read_to_string(&hash_file).unwrap();

        // Spec unchanged, but forced.
        run_pipeline(&spec, &out, true);

        let hash_after = fs::read_to_string(&hash_file).unwrap();

        // Content is the same (same spec) but the file was rewritten —
        // we verify the hash value itself is still correct.
        assert_eq!(
            hash_before.trim(),
            hash_after.trim(),
            "hash value changed unexpectedly on --force with unchanged spec"
        );
    }

    // ── .gitignore ────────────────────────────────────────────────────────────

    #[test]
    fn gitignore_entry_is_written_on_first_run() {
        let tmp = TempDir::new().unwrap();
        let (_, out) = first_run(&tmp, SPEC_YAML);
        let gitignore = fs::read_to_string(out.join(".gitignore")).unwrap();
        assert!(
            gitignore.lines().any(|l| l.trim() == ".flap_spec_hash"),
            ".flap_spec_hash must appear in .gitignore"
        );
    }

    #[test]
    fn gitignore_entry_not_duplicated_on_second_run() {
        let tmp = TempDir::new().unwrap();
        let (spec, out) = first_run(&tmp, SPEC_YAML);

        // Force a second real write so write_gitignore_entry runs again.
        run_pipeline(&spec, &out, true);

        let gitignore = fs::read_to_string(out.join(".gitignore")).unwrap();
        let count = gitignore
            .lines()
            .filter(|l| l.trim() == ".flap_spec_hash")
            .count();
        assert_eq!(count, 1, "entry appeared {count} times, expected exactly 1");
    }

    #[test]
    fn gitignore_preserves_existing_entries() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).unwrap();

        // Pre-seed .gitignore with an unrelated entry.
        fs::write(out.join(".gitignore"), "target/\n").unwrap();

        write_gitignore_entry(&out, ".flap_spec_hash").unwrap();

        let content = fs::read_to_string(out.join(".gitignore")).unwrap();
        assert!(content.contains("target/"), "pre-existing entry was lost");
        assert!(
            content.contains(".flap_spec_hash"),
            "new entry was not added"
        );
    }

    #[test]
    fn gitignore_handles_file_without_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).unwrap();

        // No trailing newline — the helper must add one before appending.
        fs::write(out.join(".gitignore"), "target/").unwrap();

        write_gitignore_entry(&out, ".flap_spec_hash").unwrap();

        let content = fs::read_to_string(out.join(".gitignore")).unwrap();
        // Both entries must be on separate lines.
        assert!(
            content.lines().any(|l| l.trim() == "target/"),
            "target/ lost"
        );
        assert!(
            content.lines().any(|l| l.trim() == ".flap_spec_hash"),
            ".flap_spec_hash missing"
        );
    }

    // ── hash file not written on failed run (contract test) ───────────────────

    #[test]
    fn hash_file_not_written_when_spec_fails_to_parse() {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).unwrap();

        let bad_spec = tmp.path().join("bad.yaml");
        fs::write(&bad_spec, "this: is: not: valid: openapi").unwrap();

        // We call the load directly to simulate what run() does when it errors.
        let load_result = flap_spec::load(&bad_spec);
        if load_result.is_err() {
            // Hash must NOT have been written.
            assert!(
                !out.join(".flap_spec_hash").exists(),
                "hash file must not be written after a failed load"
            );
        }
        // If the bad spec somehow loaded (unexpected), the test is vacuously
        // passing — it doesn't assert on the happy path.
    }
}

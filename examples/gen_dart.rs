//! Generates a Dart SDK from an OpenAPI spec and writes every file to disk.
//!
//! Usage:
//!   cargo run --example gen_dart -- <spec.yaml|https://...> <out-dir>
//!
//! Example:
//!   cargo run --example gen_dart -- \
//!     tests/fixtures/secure_petstore.yaml \
//!     /tmp/secure_petstore_sdk
//!
//!   cargo run --example gen_dart -- \
//!     https://petstore3.swagger.io/api/v3/openapi.yaml \
//!     /tmp/petstore_sdk

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);

    let Some(spec_path) = args.next() else {
        eprintln!("usage: gen_dart <spec.yaml|https://...> <out-dir>");
        return ExitCode::from(2);
    };
    let Some(out_dir) = args.next() else {
        eprintln!("usage: gen_dart <spec.yaml|https://...> <out-dir>");
        return ExitCode::from(2);
    };

    let out_dir = PathBuf::from(out_dir);

    let api = match flap_spec::load_path_or_url(&spec_path) {
        Ok(api) => api,
        Err(e) => {
            eprintln!("error loading spec: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("error creating output dir {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }

    // Models: one file per schema, plus one per synthesised inline enum.
    let models = flap_emit_dart::emit_models(&api, mode);
    let mut filenames: Vec<&String> = models.keys().collect();
    filenames.sort();
    for filename in filenames {
        let path = out_dir.join(filename);
        if let Err(e) = fs::write(&path, &models[filename]) {
            eprintln!("error writing {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        println!("wrote {}", path.display());
    }

    // Client.
    let (client_filename, client_src) = flap_emit_dart::emit_client(&api, mode);
    let client_path = out_dir.join(&client_filename);
    if let Err(e) = fs::write(&client_path, &client_src) {
        eprintln!("error writing {}: {e}", client_path.display());
        return ExitCode::FAILURE;
    }
    println!("wrote {}", client_path.display());

    println!(
        "\nDone. {} model file(s) + 1 client written to {}",
        models.len(),
        out_dir.display()
    );

    ExitCode::SUCCESS
}

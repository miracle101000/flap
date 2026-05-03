use std::path::PathBuf;
use std::fs;
use anyhow::Context;
use clap::Parser;

#[derive(Parser)]
struct Opts {
    /// Path to OpenAPI YAML
    #[clap(long)]
    spec: PathBuf,

    /// Output directory for generated Dart package
    #[clap(long)]
    out: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();

    // Load spec -> IR
    let api = flap_spec::load(&opts.spec).context("loading spec")?;

    // Emit models and client
    let models = flap_emit_dart::emit_models(&api);
    let (client_filename, client_source) = flap_emit_dart::emit_client(&api);

    // Prepare output layout for a Dart package
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
    fs::write(opts.out.join("pubspec.yaml"), pubspec)?;

    // Write model files
    for (name, src) in models {
        fs::write(opts.out.join("lib").join(name), src)?;
    }

    // Write client
    fs::write(opts.out.join("lib").join(client_filename), client_source)?;

    println!("Wrote generated Dart package to {}", opts.out.display());
    Ok(())
}

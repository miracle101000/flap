use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(spec_path) = env::args().nth(1) else {
        eprintln!("usage: flap <spec.yaml>");
        return ExitCode::from(2);
    };

    let api = match flap_spec::load(&spec_path) {
        Ok(api) => api,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    // Emit Dart model files and print each to stdout so we can eyeball them.
    // Sorted by filename for stable, readable output.
    let mut files: Vec<(String, String)> = flap_emit_dart::emit_models(&api).into_iter().collect();

    let (client_file, client_src) = flap_emit_dart::emit_client(&api);
    files.push((client_file, client_src));

    files.sort_by(|a, b| a.0.cmp(&b.0));

    for (filename, source) in &files {
        println!(
            "// ── {filename} {}",
            "─".repeat(60usize.saturating_sub(filename.len()))
        );
        println!("{source}");
    }

    ExitCode::SUCCESS
}

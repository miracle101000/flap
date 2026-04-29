use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(spec_path) = env::args().nth(1) else {
        eprintln!("usage: flap <spec.yaml>");
        return ExitCode::from(2);
    };

    match flap_spec::load(&spec_path) {
        Ok(spec) => {
            println!(
                "Found {} operations and {} schemas.",
                spec.operation_count(),
                spec.schema_count()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            // {:#} prints anyhow's full error chain.
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

use std::env;
use std::process::ExitCode;

use flap_ir::SchemaKind;

fn main() -> ExitCode {
    let Some(spec_path) = env::args().nth(1) else {
        eprintln!("usage: flap <spec.yaml>");
        return ExitCode::from(2);
    };

    match flap_spec::load(&spec_path) {
        Ok(api) => {
            println!("# {}", api.title);
            if let Some(url) = &api.base_url {
                println!("# base: {url}");
            }
            println!();

            println!("Operations ({}):", api.operations.len());
            for op in &api.operations {
                let id = op.operation_id.as_deref().unwrap_or("—");
                println!("  {} {}  ({})", op.method, op.path, id);
            }
            println!();

            println!("Schemas ({}):", api.schemas.len());
            for schema in &api.schemas {
                match &schema.kind {
                    SchemaKind::Object { fields } => {
                        println!("  {} (object)", schema.name);
                        for f in fields {
                            let req = if f.required { "*" } else { "?" };
                            println!("    {req} {}: {}", f.name, f.type_ref);
                        }
                    }
                    SchemaKind::Array { item } => {
                        println!("  {} (array of {})", schema.name, item);
                    }
                }
            }

            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

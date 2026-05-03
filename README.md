# flap

flap is a small Rust-based OpenAPI lowering + code-generation workspace. It provides:

- a language-agnostic intermediate representation (crates/flap-ir)
- a spec lowering crate (crates/flap-spec) that turns OpenAPI into the IR
- a Dart emitter (crates/flap-emit-dart) that can generate Dart client code from the IR

Status: work-in-progress. The core IR and a CLI are present; the project needs examples, CI, and a published emitter workflow to be widely usable.

Features
- Deterministic IR that models operations, schemas, unions, nullability/optional semantics, and security schemes.
- Designed to emit idiomatic Dart (the IR includes notes for Freezed/json_serializable patterns and for preserving absent-vs-null PATCH semantics).
- Workspace layout that makes it straightforward to add more emitters for other languages.

Quickstart (developer)

Prerequisites
- Rust toolchain (rustup + cargo)
- Optional: Dart/Flutter SDK if you plan to build/run the generated Dart clients

Build the workspace

```bash
# from repo root
cargo build --workspace
```

Run the CLI to inspect an OpenAPI spec

```bash
# prints a summary of operations and schemas
cargo run --package flap -- path/to/spec.yaml
```

Example OpenAPI snippet (spec.yaml)

```yaml
openapi: 3.0.0
info:
  title: Example API
  version: "1.0"
servers:
  - url: https://api.example.com
paths:
  /pets:
    get:
      summary: List pets
      operationId: listPets
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                type: array
                items:
                  type: string
```

Running the CLI on the example will produce a human-readable summary like:

```text
# Example API
# base: https://api.example.com

Operations (1):
  GET     /pets                   (listPets)
    →  200      : array<string>

Schemas (0):
```

## Generating Dart clients

The workspace contains `crates/flap-emit-dart` — the Dart emitter that
consumes the flap IR and materialises a complete Dart package. The
`generate_dart` binary ties everything together.

### Prerequisites

- **Rust toolchain** (`rustup` + `cargo`) to build and run flap.
- **Dart SDK ≥ 2.19** — the `dart` binary must be on your `$PATH`.
  Install from <https://dart.dev/get-dart>. If you only want to inspect
  generated source without compiling it, you can skip the Dart SDK and
  pass `--no-build-runner` (see below).

### Running the generator

```bash
cargo run --bin generate_dart -- \
  --spec path/to/your/spec.yaml \
  --out  path/to/output/package
```

The generator performs these steps automatically:

1. Loads and validates your OpenAPI 3.0 spec.
2. Writes a complete Dart package to `--out` (`pubspec.yaml` + all
   source files under `lib/`).
3. Runs `dart pub get` inside the output directory.
4. Runs `dart run build_runner build --delete-conflicting-outputs`
   inside the output directory, producing the Freezed/json_serializable
   implementation files (`.freezed.dart`, `.g.dart`).

After step 4 the package is fully ready to import.

### Skipping build_runner

Pass `--no-build-runner` if the Dart SDK is not available in your
current environment, or if you prefer to run code generation yourself:

```bash
cargo run --bin generate_dart -- \
  --spec path/to/spec.yaml \
  --out  /tmp/my_package \
  --no-build-runner
```

flap will print a reminder with the exact commands to run manually:

```
dart pub get && dart run build_runner build --delete-conflicting-outputs
```

### Typical CI pattern

```yaml
- name: Generate Dart client
  run: |
    cargo run --bin generate_dart -- \
      --spec api/openapi.yaml \
      --out  generated/my_api_client

- name: Analyse generated client
  working-directory: generated/my_api_client
  run: dart analyze
```

Because `--no-build-runner` is **not** set, the generated `.freezed.dart`
and `.g.dart` files are produced in the same step and can be analysed
immediately without a separate build_runner invocation.

## Repository layout

- Cargo.toml           — workspace + top-level package "flap" (CLI)
- src/main.rs          — CLI entry point (summary printer)
- crates/
  - flap-ir/           — intermediate representation types (library)
  - flap-spec/         — lowers OpenAPI -> flap-ir
  - flap-emit-dart/    — Dart emitter (produces Dart client packages)
- examples/            — (placeholder) suggested place for sample specs and generated output

## Contributing

Contributions, issues, and suggestions are welcome. Useful first PRs:
- Add a LICENSE (MIT or Apache-2.0 recommended)
- Add example OpenAPI specs and the generated Dart outputs for those specs
- Add CI (build + basic integration test that runs the emitter and checks generated code compiles)

## Development tips
- Run `cargo build --workspace` to compile everything.
- Use `cargo run --package flap -- path/to/spec.yaml` to inspect a spec.
- To iterate on the Dart emitter, work inside `crates/flap-emit-dart` and add small specs to `examples/`.

## License

This repository does not include a LICENSE file yet. Please add one to make reuse and contribution clear (MIT or Apache-2.0 are common choices).

## Roadmap / Next steps
- Add example specs and generated Dart packages to `examples/` so users can try flap quickly.
- Document the emitter invocation and make the emitter produce a ready-to-publish Dart package (including pubspec.yaml and readme).
- Add a minimal Flutter demo that consumes a generated client to demonstrate integration.
- Add CI to build the workspace and run a smoke test that the generated Dart package analyzes/compiles.

## Contact

If you want help polishing the emitter or creating examples, I can:
- Inspect `crates/flap-emit-dart` and draft a recommended emitter CLI and output layout
- Create a minimal example spec and the generated Dart package as a demo


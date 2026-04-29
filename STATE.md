# Flap — Current State

## What this project is
A Rust CLI tool that reads an OpenAPI spec and generates idiomatic Dart/Flutter
SDKs. No Java required. Output uses Freezed + Dio.

## Where we are
- ✅ Workspace scaffolded (flap, flap-spec, flap-ir, flap-emit-dart crates)
- ✅ PetStore spec downloaded to tests/fixtures/petstore.yaml
- ⬜ Parser: load PetStore YAML and print operations + schemas
- ⬜ IR: minimal types (Api, Operation, Schema, Field)
- ⬜ Dart emitter: produce Freezed models + Dio client for PetStore
- ⬜ CLI: `flap generate --spec X --out Y`

## Next session goal
Make `cargo run -- tests/fixtures/petstore.yaml` print a list of operations
and schemas found in the spec.
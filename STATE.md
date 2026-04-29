# Flap — Current State

## What this project is
A Rust CLI tool that reads an OpenAPI spec and generates idiomatic Dart/Flutter
SDKs. No Java required. Output uses Freezed + Dio.

## Where we are
- ✅ Workspace scaffolded (flap, flap-spec, flap-ir, flap-emit-dart crates)
- ✅ PetStore spec downloaded to tests/fixtures/petstore.yaml
- ✅ Parser: loads PetStore YAML, counts operations + schemas
       → "Found 3 operations and 3 schemas." ✓
- ⬜ IR: minimal types (Api, Operation, Schema, Field) — currently just counts
- ⬜ Dart emitter: produce Freezed models + Dio client for PetStore
- ⬜ CLI: `flap generate --spec X --out Y`

## Next session goal
Replace the count with real data extraction. Build the IR types in
`flap-ir` (Api, Operation, Schema, Field), then have `flap-spec` produce a
fully-populated `Api` value. Print each operation's method + path and each
schema's name + field list. Still no code generation yet.
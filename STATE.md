# Flap — Current State

## What this project is
A Rust CLI tool that reads an OpenAPI spec and generates idiomatic Dart/Flutter
SDKs. No Java required. Output uses Freezed + Dio.

## Where we are
- ✅ Workspace scaffolded (flap, flap-spec, flap-ir, flap-emit-dart crates)
- ✅ PetStore spec downloaded to tests/fixtures/petstore.yaml
- ✅ Parser: loads PetStore YAML
- ✅ IR: Api, Operation, Schema, Field types in flap-ir
- ✅ flap-spec produces fully-populated Api from PetStore
       → operations with method/path/operationId
       → schemas with name/kind/fields/required-flag/format
       → array element types resolved through $ref
- ⬜ Dart emitter: produce Freezed models for each schema
- ⬜ Dart emitter: produce Dio client class with one method per operation
- ⬜ CLI: `flap generate --spec X --out Y` (writes files to disk)

## Next session goal
Start the Dart emitter. In flap-emit-dart, write a function
`emit_models(api: &Api) -> HashMap<String, String>` that returns one
Dart file per schema, each as a Freezed class. Don't write to disk yet —
print the generated source to stdout so we can eyeball it. Just models;
the Dio client comes after.
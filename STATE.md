# Flap — Current State

## What this project is
A Rust CLI tool that reads an OpenAPI spec and generates idiomatic Dart/Flutter
SDKs. No Java required. Output uses Freezed + Dio.

## Where we are
- ✅ Workspace scaffolded
- ✅ PetStore spec parses cleanly
- ✅ IR types: Api, Operation, Schema, Field, TypeRef
- ✅ flap-spec produces fully-populated Api
- ✅ Dart emitter — models:
       → Freezed classes for object schemas
       → typedef for array schemas
       → null-safe required/optional fields
- ✅ D7 name collision handling (Error → ErrorModel)
- ✅ Dart emitter — client stub:
       → class name derived from info.title
       → Dio constructor wired up
       → method per operation with summary doc-comment
       → method bodies are UnimplementedError() placeholders
- ⬜ IR: extend Operation with parameters / request body / responses
- ⬜ Dart emitter: real method bodies (path/query params, request body, response parsing)
- ⬜ CLI: `flap generate --spec X --out Y` (writes files to disk)
- ⬜ End-to-end test: generate, drop into a Flutter project, hit a real API

## Known issues
- Method signatures all return `Future<void>` and take no arguments — by design
  while the IR doesn't model parameters/responses yet.

## Next session goal
Extend the IR to model operation parameters. Add `Parameter { name, location,
type_ref, required }` and `Operation.parameters: Vec<Parameter>`. Update
flap-spec to populate these from `parameters:` in the YAML. Print them
alongside the operation listing. Don't update the emitter yet — get the IR
solid first.
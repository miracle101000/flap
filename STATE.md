# Flap — Current State

## What this project is
A Rust CLI tool that reads an OpenAPI spec and generates idiomatic Dart/Flutter
SDKs. No Java required. Output uses Freezed + Dio.

## Where we are
- ✅ Workspace scaffolded
- ✅ PetStore spec parses cleanly
- ✅ IR types: Api, Operation, Schema, Field, TypeRef, Parameter
- ✅ flap-spec produces fully-populated Api
- ✅ Dart emitter — models (Freezed + typedef)
- ✅ Dart emitter — client stub (class + Dio + UnimplementedError stubs)
- ✅ D7 name collision handling
- ✅ IR: Operation.parameters with location (path/query/header), type, required
- ⬜ IR: Operation.request_body
- ⬜ IR: Operation.responses (status code → schema)
- ⬜ Dart emitter: real method signatures using parameter info
- ⬜ Dart emitter: real method bodies (URL templating, query map, response parsing)
- ⬜ CLI: `flap generate --spec X --out Y` (writes files to disk)
- ⬜ End-to-end test: drop into a Flutter project, hit a real API

## Known issues
- Method signatures still all `Future<void>` with no args — emitter not yet
  using the parameter IR.

## Next session goal
Extend IR with `Operation.request_body: Option<RequestBody>` and populate it
from flap-spec. RequestBody has content_type + schema_ref. PetStore's
`POST /pets` references the `Pet` schema. Print it under the operation in
the listing, similar to how parameters are shown.
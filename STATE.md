# Flap — Current State

## What this project is
A Rust CLI tool that reads an OpenAPI spec and generates idiomatic Dart/Flutter
SDKs. No Java required. Output uses Freezed + Dio.

## Where we are
- ✅ Workspace scaffolded
- ✅ PetStore spec parses cleanly
- ✅ IR types in flap-ir
- ✅ flap-spec produces fully-populated Api
- ✅ Dart emitter: emits Freezed models for object schemas
       → Pet generates correctly with required/optional fields
       → Arrays emit as typedefs (Pets = List<Pet>)
       → Output is idiomatic, modern, null-safe Dart
- ⬜ Name collision handling (Error → ErrorModel etc.) — see DECISIONS D7
- ⬜ Dart emitter: Dio client class with method per operation
- ⬜ CLI: `flap generate --spec X --out Y` (writes files to disk)

## Known issues
- "Error" schema generates `class Error` which collides with dart:core.
  Will be fixed by D7 (suffix-on-collision) before next emitter milestone.

## Next session goal
Implement D7 (name collision suffix), then start the Dio client emitter.
First just emit a stub class `class PetstoreClient { final Dio _dio; ... }`
with placeholder methods. Real method bodies the session after.
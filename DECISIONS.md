# Flap — Decisions Log

## D1: Tool implementation language: Rust
Single binary, no JVM, fast startup. Differentiator vs. existing Java-based tools.

## D2: Output target for v0.1: Dart only (Flutter)
Skip TypeScript and Python despite earlier scope. Flutter is the underserved niche.

## D3: Output uses Freezed + Dio
These are the standard for modern Flutter apps. Existing tools have weak Freezed
support — this is the differentiator.

## D4: Test fixture: PetStore (OpenAPI 3.0 example)
Standard demo spec. Small, covers common features. Real specs (Stripe, etc.) come later.

## D5: OpenAPI version support: 3.0 in v0.1, 3.1 deferred
3.1 adds JSON Schema 2020-12 complexity. Get 3.0 right first.

## D6: oneOf/anyOf require discriminator in v0.1
Hard error otherwise with clear message. Structural unions deferred.

## D7: Dart name collision handling
When a schema name collides with a Dart core type (Error, Type, Object,
Function, Future, Stream, Iterable, List, Map, Set, String, int, double,
bool, num, Symbol, Record, Pattern, RegExp, DateTime, Duration, Uri,
Exception), append "Model" to the generated class name. Document the
list explicitly. Allow override via `x-flap-name` extension.

## D8: Method bodies stubbed before IR is complete
Emit `UnimplementedError(<operationId>)` for v0.1 method bodies until the IR
models parameters, request bodies, and responses. Lets us ship a complete
client *shape* that compiles, while the substantive emitter work happens
incrementally on a stable interface.

## D9: Client class name derived from info.title
`SwaggerPetstoreClient` from "Swagger Petstore". Suffix is always "Client".
Future override via `x-flap-client-name` extension if user feedback warrants.
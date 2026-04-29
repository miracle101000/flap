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
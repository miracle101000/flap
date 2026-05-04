# flap

**flap** is a Rust-based OpenAPI code-generation toolchain that lowers OpenAPI 3.0, OpenAPI 3.1, and Swagger 2.0 specs into idiomatic, production-ready Dart/Flutter client libraries — without requiring Java, a Dart SDK, or any internet connection beyond your spec file.

---

## Table of contents

- [Why flap](#why-flap)
- [Architecture](#architecture)
- [Prerequisites](#prerequisites)
- [Quick start](#quick-start)
- [CLI reference](#cli-reference)
- [Generated Dart output](#generated-dart-output)
  - [Models](#models)
  - [Client — Dio backend](#client--dio-backend)
  - [Client — http backend](#client--http-backend)
  - [Null safety modes](#null-safety-modes)
  - [PATCH tri-state semantics](#patch-tri-state-semantics)
  - [Response headers](#response-headers)
  - [Unions](#unions)
  - [Enums](#enums)
  - [Security schemes](#security-schemes)
  - [Multiple servers](#multiple-servers)
  - [Vendor extensions](#vendor-extensions)
- [Spec validation](#spec-validation)
- [Incremental builds](#incremental-builds)
- [Type and import mappings](#type-and-import-mappings)
- [Custom templates](#custom-templates)
- [Supported spec features](#supported-spec-features)
- [Repository layout](#repository-layout)
- [Crate API](#crate-api)
- [Contributing](#contributing)

---

## Why flap

| Concern | flap | openapi_generator (pub.dev) |
|---|---|---|
| Java required | ❌ | ✅ mandatory |
| Model framework | Freezed (native) | `built_value` — Freezed is an open feature request |
| OpenAPI 3.1 | ✅ | ❌ generates uncompilable Dart |
| PATCH tri-state (`Optional<T?>`) | ✅ | ❌ |
| Typed response headers | ✅ Dart 3 named record | ❌ |
| `CancelToken` per method | ✅ | ❌ |
| Dio constructor passthrough | ✅ `BaseOptions`, interceptors, adapter | ❌ |
| `package:http` backend | ✅ `--client=http` | ❌ |
| Pre-generation validation | ✅ all errors at once | ❌ crashes on first |
| Custom templates | ✅ Jinja2 | ✅ Mustache |
| Type / import mappings | ✅ `--type-map` / `--import-map` | ✅ |
| Incremental builds | ✅ lockfile per mode + backend | ✅ `build_runner` cache |
| Deterministic output | ✅ BTreeMap-sorted | ❌ hash-map order |
| Dependency conflicts | ❌ none | ✅ constant `pubspec.yaml` friction |

---

## Architecture

```
flap (workspace)
├── crates/flap-ir          Language-agnostic intermediate representation
├── crates/flap-spec        Spec loader + lowering pass (OpenAPI → IR)
├── crates/flap-emit-dart   Dart emitter (IR → .dart source files)
└── src/bin/generate_dart   CLI binary that wires everything together
```

The pipeline is strictly one-directional:

```
YAML / JSON spec
      │
      ▼
  flap-spec      ← loads, validates, and lowers to flap-ir types
      │
      ▼
   flap-ir       ← language-agnostic contract (operations, schemas, security)
      │
      ▼
flap-emit-dart   ← produces .dart source files
      │
      ▼
  Output dir     ← ready for build_runner + dart pub get
```

Nothing in the emitter knows about YAML. Nothing in the spec loader knows about Dart. Adding a new target language means writing a new `flap-emit-*` crate that reads from `flap-ir` — the spec loader is reused unchanged.

---

## Prerequisites

- **Rust toolchain** — install via [rustup.rs](https://rustup.rs)
- No Java, no Dart SDK, no internet connection required to run the generator

To compile and analyse the generated Dart output you will additionally need:

- **Dart SDK ≥ 2.19** or the **Flutter SDK** (which includes Dart)
- `flutter pub get` / `dart pub get`
- `dart run build_runner build` (for Freezed + json_serializable codegen)

---

## Quick start

```bash
# Clone and build
git clone https://github.com/your-org/flap
cd flap
cargo build --release

# Generate a Dart client from a local spec
cargo run --bin generate_dart -- \
  --out ./sdks \
  path/to/petstore.yaml

# Generated files appear under ./sdks/petstore/
#   null_safe/    ← Dart ≥ 2.12 output
#   null_unsafe/  ← legacy output
```

After generation, run Freezed inside the output directory:

```bash
cd sdks/petstore/null_safe
dart pub get
dart run build_runner build --delete-conflicting-outputs
```

---

## CLI reference

```
cargo run --bin generate_dart -- [OPTIONS] <spec> [<spec> ...]
```

### Options

| Flag | Default | Description |
|---|---|---|
| `--out <dir>` / `-o <dir>` | required | Root output directory. Each spec gets a subdirectory named after the spec file stem. |
| `--force` / `-f` | false | Regenerate even if the spec and templates are unchanged (bypasses lockfile check). |
| `--client=dio` | default | Use `package:dio` for the generated HTTP client. Full feature set: `CancelToken`, interceptors, `BaseOptions`, `HttpClientAdapter`. |
| `--client=http` | — | Use `package:http` for the generated HTTP client. Simpler; no interceptors or cancel tokens. |
| `--type-map=Schema=DartType` | — | Replace schema `Schema` with the Dart type `DartType` everywhere it appears. May be repeated. |
| `--import-map=DartType=package:path` | — | Add `import 'package:path';` wherever `DartType` is used. May be repeated. |
| `--template-dir <dir>` / `-t <dir>` | — | Directory of Jinja2 template overrides and/or verbatim file replacements. |

### Examples

```bash
# Remote spec, http backend, force regeneration
cargo run --bin generate_dart -- \
  --out ./sdks \
  --client=http \
  --force \
  https://petstore3.swagger.io/api/v3/openapi.yaml

# Replace a spec schema with a hand-written model
cargo run --bin generate_dart -- \
  --out ./sdks \
  --type-map=Pet=MyPet \
  --import-map=MyPet=package:myapp/models/my_pet.dart \
  petstore.yaml

# Multiple specs in one run
cargo run --bin generate_dart -- \
  --out ./sdks \
  petstore.yaml \
  internal_api.yaml \
  https://example.com/api/openapi.json

# Custom Jinja2 templates
cargo run --bin generate_dart -- \
  --out ./sdks \
  --template-dir ./templates \
  petstore.yaml
```

---

## Generated Dart output

Each spec produces two subdirectories under its named folder:

```
sdks/
└── petstore/
    ├── null_safe/          ← Dart ≥ 2.12 sound null safety
    │   ├── .flap.lock.*    ← incremental build lockfiles
    │   ├── flap_utils.dart ← Optional<T?> runtime
    │   ├── pet.dart
    │   ├── order.dart
    │   └── pet_store_client.dart
    └── null_unsafe/        ← legacy Dart < 2.12
        ├── pet.dart
        └── pet_store_client.dart
```

### Models

Every object schema becomes a `@freezed` class:

```dart
import 'package:freezed_annotation/freezed_annotation.dart';

part 'pet.freezed.dart';
part 'pet.g.dart';

@freezed
class Pet with _$Pet {
  const factory Pet({
    required int id,
    required String name,
    @JsonKey(includeIfNull: false) String? tag,
  }) = _Pet;

  factory Pet.fromJson(Map<String, dynamic> json) => _$PetFromJson(json);
}
```

- **Array schemas** → Dart `typedef Name = List<T>;`
- **Map schemas** (`additionalProperties`) → `typedef Name = Map<String, T>;`
- **Alias schemas** (`$ref` at top level) → `typedef Alias = Target;`
- **Core type collisions** (`String`, `Map`, `List`, etc.) → appends `Model` suffix automatically
- **Reserved keyword fields** → appends `Param` suffix automatically

### Client — Dio backend

The default backend generates a Dio client with full production features:

```dart
class PetStoreClient {
  PetStoreClient({
    String baseUrl = 'https://petstore.example.com',
    String? apiKey,              // one param per security scheme
    BaseOptions? options,        // full Dio options passthrough
    List<Interceptor> interceptors = const [],
    HttpClientAdapter? httpClientAdapter,
  }) { ... }

  late final Dio _dio;

  /// List all pets
  // GET /pets
  Future<List<Pet>> listPets({
    int? limit,
    CancelToken? cancelToken,   // every method accepts a cancel token
  }) async { ... }

  // POST /pets
  Future<void> createPets({
    CancelToken? cancelToken,
  }) async { ... }

  // GET /pets/{petId}
  Future<Pet> showPetById({
    required String petId,
    CancelToken? cancelToken,
  }) async { ... }
}
```

**Constructor features:**

- `BaseOptions?` — pass connect/receive timeouts, base headers, response type, etc.
- `List<Interceptor>` — add retry interceptors, logging interceptors, or any custom `InterceptorsWrapper` before auth is injected.
- `HttpClientAdapter?` — swap in a mock adapter for testing or a custom TLS configuration.
- Auth credentials are injected last via `InterceptorsWrapper`, after any user-supplied interceptors.

### Client — http backend

`--client=http` generates a simpler client suitable for environments where Dio is not available:

```dart
class PetStoreClient {
  PetStoreClient({
    String baseUrl = 'https://petstore.example.com',
    String? apiKey,
    http.Client? client,        // injectable for testing
  }) : _baseUrl = ...,
       _client = client ?? http.Client();

  final String _baseUrl;
  final http.Client _client;

  Map<String, String> get _headers => {
    'Content-Type': 'application/json',
    if (_apiKey != null) 'X-API-Key': _apiKey!,
  };

  Future<List<Pet>> listPets({int? limit}) async {
    final uri = Uri.parse('$_baseUrl/pets')
        .replace(queryParameters: ...);
    final _response = await _client.get(uri, headers: _allHeaders);
    if (_response.statusCode < 200 || _response.statusCode >= 300) {
      throw Exception('GET /pets returned ${_response.statusCode}');
    }
    return (jsonDecode(_response.body) as List<dynamic>)
        .map((e) => Pet.fromJson(e as Map<String, dynamic>))
        .toList();
  }
}
```

Multipart operations use `http.MultipartRequest` automatically.

**What the http backend does not generate** (compared to Dio):

- No `CancelToken` per method
- No `List<Interceptor>` constructor param
- No `BaseOptions` / `HttpClientAdapter`

### Null safety modes

Both backends are generated in two flavours:

| Mode | Dart version | Differences |
|---|---|---|
| `null_safe` | Dart ≥ 2.12 | `T?` suffixes, `required` keyword, `Optional<T?>` wrapper, Dart 3 named records for response headers |
| `null_unsafe` | Dart < 2.12 | No `?` suffixes, `@required` annotation from `package:meta`, no `Optional<T?>`, no records |

### PATCH tri-state semantics

HTTP PATCH endpoints require distinguishing three wire states for optional fields:

| `required` | `nullable` | Wire meaning | Generated Dart |
|---|---|---|---|
| true | false | Key MUST be present, value non-null | `required T name` |
| true | true | Key MUST be present, value may be null | `required T? name` |
| false | false | Key MAY be omitted, never null when present | `T? name` + `@JsonKey(includeIfNull: false)` |
| false | true | Key MAY be omitted OR present with null | `Optional<T?> name` |

The bottom-right cell — where a client must be able to send `"key": null` (explicit null) vs omit the key entirely — is handled by the `Optional<T?>` wrapper generated in `flap_utils.dart`:

```dart
// Omit the field entirely:
final patch = MyModel(name: Optional.absent());

// Send "name": null explicitly:
final patch = MyModel(name: Optional.present(null));

// Send "name": "Alice":
final patch = MyModel(name: Optional.present("Alice"));
```

`Optional<T?>` is supported for primitive types (`String`, `int`, `num`, `bool`). Non-primitive fields fall back to `T?` + `includeIfNull: false` with a `// TODO(flap)` comment.

### Response headers

When a spec declares response headers flap generates a Dart 3 named record return type so headers and body are typed together:

```dart
// spec declares X-Rate-Limit-Remaining: integer (required)
// and X-Request-Id: string (optional)

Future<({Pet body, int xRateLimitRemaining, String? xRequestId})>
    showPetById({required String petId, CancelToken? cancelToken}) async {
  final response = await _dio.request<dynamic>(...);
  final xRateLimitRemaining =
      int.parse(response.headers.value('x-rate-limit-remaining'));
  final xRequestIdRaw = response.headers.value('x-request-id');
  final xRequestId = xRequestIdRaw;
  return (
    body: Pet.fromJson(response.data as Map<String, dynamic>),
    xRateLimitRemaining: xRateLimitRemaining,
    xRequestId: xRequestId,
  );
}
```

In `null_unsafe` mode records are not available — the body type is returned directly and headers are dropped.

### Unions

**Discriminated unions** (`oneOf` + `discriminator`) become `@Freezed` sealed classes:

```dart
@Freezed(unionKey: 'type')
sealed class Animal with _$Animal {
  @FreezedUnionValue('cat')
  const factory Animal.cat({required String meow}) = AnimalCat;

  @FreezedUnionValue('dog')
  const factory Animal.dog({required String woof}) = AnimalDog;

  factory Animal.fromJson(Map<String, dynamic> json) =>
      _$AnimalFromJson(json);
}
```

**Untagged unions** (`anyOf` / `oneOf` without discriminator) use try-each deserialization:

```dart
sealed class StringOrPet {
  const StringOrPet._();
  const factory StringOrPet.variant0(String value) = _Variant0;
  const factory StringOrPet.variant1(Pet value) = _Variant1;

  factory StringOrPet.fromJson(dynamic json) {
    if (json is String) return StringOrPet.variant0(json);
    if (json is Map<String, dynamic>) {
      try { return StringOrPet.variant1(Pet.fromJson(json)); } catch (_) {}
    }
    throw ArgumentError('Cannot deserialize into StringOrPet: $json');
  }
}
```

**allOf inheritance** is detected and surfaced via `Schema.extends` in the IR so emitters can generate proper class hierarchies.

**Recursive types** (`Node.children: List<Node>`) are detected by the lowering pass and the `is_recursive` flag is set on `Field` — emitters use this to avoid inline typedef wrapping that would break Freezed's generator.

### Enums

Both string and integer enum values are supported. Every generated enum includes an `unknown` sentinel so clients are resilient to new values added on the server:

```dart
@JsonEnum(unknownValue: PetStatus.unknown)
enum PetStatus {
  @JsonValue('available')
  available,
  @JsonValue('pending')
  pending,
  @JsonValue('sold')
  sold,
  @JsonValue(null)
  unknown;
}
```

Integer enums use `v`-prefixed identifiers so the case never starts with a digit:

```dart
@JsonValue(1)
v1,
@JsonValue(2)
v2,
```

Inline enums (declared directly on a field, parameter, or response) are extracted into named synthetic enum files automatically.

### Security schemes

All five OpenAPI security scheme types are supported. Credentials are constructor parameters and injected per-request:

| Scheme | Generated constructor param | Injection |
|---|---|---|
| `apiKey` (header) | `String? apiKey` | `options.headers['X-API-Key'] = apiKey` |
| `apiKey` (query) | `String? apiKey` | `options.queryParameters['api_key'] = apiKey` |
| `apiKey` (cookie) | `String? apiKey` | Appended to `Cookie` header |
| `http bearer` | `String? authorization` | `Authorization: Bearer <token>` |
| `http basic` | `String? authorization` | `Authorization: Basic <base64>` |
| `oauth2` | `String? schemeName` | `Authorization: Bearer <token>` |
| `openIdConnect` | `String? schemeName` | `Authorization: Bearer <token>` |

Multiple schemes generate multiple constructor parameters. Per-operation `security` overrides in the spec are propagated to the IR but the emitter applies global-level security — per-operation override support is a planned enhancement.

### Multiple servers

When a spec declares more than one server URL, flap emits a constants class:

```dart
abstract final class PetStoreClientUrls {
  static const String server0 = 'https://petstore.example.com';
  static const String server1 = 'https://staging.petstore.example.com';
}

// Usage:
final client = PetStoreClient(baseUrl: PetStoreClientUrls.server1);
```

When exactly one server is declared, the URL is used as the `baseUrl` default directly.

### Vendor extensions

All `x-*` keys are captured from every spec node and surfaced on the IR:

```
Api.extensions          top-level x-* keys
Operation.extensions    per-operation x-* keys
Parameter.extensions    per-parameter x-* keys
RequestBody.extensions  per-requestBody x-* keys
Response.extensions     per-response x-* keys
Schema.extensions       per-schema x-* keys
Field.extensions        per-field schema x-* keys
```

The built-in emitter ignores extensions — but a custom Jinja2 template or a downstream tool consuming the IR can read them directly. This is the primary hook for spec-level code-generation customisation without forking flap.

---

## Spec validation

flap validates the spec **before lowering**, accumulating all errors and reporting them together rather than failing on the first one:

```
error: spec validation failed:
  - Pet: $ref `#/components/schemas/Category` points to undefined schema `Category`
  - createPets parameter[0] `status` (in: body) has no `schema`
  - operationId `listPets` is used by both `/pets` and `/animals`
  - security requirement `ApiKeyAuth` at top-level references an undefined security scheme
```

**What is validated for OpenAPI 3.x:**

- Every `$ref` in schemas, properties, `allOf`/`anyOf`/`oneOf`, `additionalProperties`, `items`, request bodies, and responses resolves to a defined component schema
- `discriminator.mapping` refs resolve
- Every parameter has a `schema` (except body parameters in Swagger 2.0)
- Parameter `in:` values are one of `query`, `path`, `header`, `cookie`
- `operationId` values are unique across the entire spec
- Security requirement names reference defined security schemes

**What is validated for Swagger 2.0:**

- Every `$ref` in `#/definitions/*` resolves
- `operationId` uniqueness
- `$ref` integrity in `allOf`, `additionalProperties`, `items`

---

## Incremental builds

flap writes a lockfile alongside each output directory:

```
sdks/petstore/null_safe/.flap.lock.null_safe.dio
sdks/petstore/null_unsafe/.flap.lock.null_unsafe.dio
```

The fingerprint includes:

- `CARGO_PKG_VERSION` — emitter version bump forces regeneration
- Backend label (`dio` / `http`) — switching backends forces regeneration
- All `--type-map` and `--import-map` values — mapping changes force regeneration
- Template directory contents (file names + byte lengths) — editing a template forces regeneration
- Spec file `mtime` + `size` — spec edits force regeneration

Remote specs (`http://` / `https://`) are always regenerated since flap cannot fingerprint them without fetching.

Use `--force` / `-f` to bypass the lockfile entirely (useful in CI pipelines that always regenerate):

```bash
cargo run --bin generate_dart -- --force --out ./sdks petstore.yaml
```

Commit the lockfiles alongside generated source. That way CI skips regeneration on unchanged specs exactly like a developer's machine does, and a spec edit produces a visible lockfile diff in the PR.

---

## Type and import mappings

Replace any spec-defined schema with a pre-existing Dart type. flap will skip generating a file for the mapped schema and route all usages to the replacement type.

```bash
cargo run --bin generate_dart -- \
  --out ./sdks \
  --type-map=Pet=MyPet \
  --import-map=MyPet=package:myapp/models/my_pet.dart \
  --type-map=User=AppUser \
  --import-map=AppUser=package:myapp/models/app_user.dart \
  petstore.yaml
```

**Requirements for the replacement type:**

- Must implement `factory T.fromJson(Map<String, dynamic> json)` — the same contract Freezed generates, so existing Freezed models drop in with zero modification.
- Must implement `Map<String, dynamic> toJson()` — also generated by Freezed.

`--type-map` and `--import-map` are independent. You can map a type without registering an import (if the type is already in scope) and vice versa.

---

## Custom templates

Pass `--template-dir` to override any generated file. flap resolves output per file in this order:

1. `{template-dir}/{exact-filename}` — verbatim file copy, no rendering
2. `{template-dir}/model.dart.jinja` — Jinja2 template applied to every model file
3. `{template-dir}/client.dart.jinja` — Jinja2 template applied to the client file
4. `{template-dir}/flap_utils.dart` — verbatim override of the Optional runtime
5. Built-in emitter — fallback when nothing in the template dir matches

A render error in a Jinja2 template prints a warning and falls back to the built-in emitter for that file — generation continues rather than failing.

### Template context for `model.dart.jinja`

```
class_name          Dart class name after mapping
schema_name         Original spec schema name
snake_name          snake_case of class_name (for `part` directives)
null_safety         "safe" or "unsafe"
extends             Parent schema name (allOf inheritance), or null
imports             List of sorted, deduped import lines
has_optional_fields true when any field uses Optional<T?>
fields[]
  spec_name         Original spec field name
  dart_name         camelCase Dart identifier
  dart_type         Full resolved Dart type (e.g. "List<String>", "Pet?")
  required          bool
  nullable          bool
  uses_optional_wrapper  bool
  default_expr      "@Default(...)" expression or null
  json_name         Non-null when dart_name ≠ spec_name
```

### Template context for `client.dart.jinja`

```
class_name          Dart client class name
default_base_url    First server URL or empty string
base_urls           All declared server URLs
backend             "dio" or "http"
null_safety         "safe" or "unsafe"
credentials[]
  dart_param_name   camelCase constructor parameter name
  scheme_type       "apiKey"|"httpBasic"|"httpBearer"|"oauth2"|"openIdConnect"
operations[]
  method            "GET", "POST", etc.
  path              "/pets/{petId}"
  method_name       Dart method name
  summary           Optional summary string
  return_type       Full Dart return type
  has_body          bool
  body_type         Dart type of the body, or null
  body_required     bool
  is_multipart      bool
  parameters[]
    spec_name       Original param name
    dart_name       camelCase Dart name
    dart_type       Resolved Dart type
    location        "query"|"path"|"header"|"cookie"
    required        bool
```

### Example: add `copyWithPatch` to every model

`templates/model.dart.jinja`:

```jinja
import 'package:freezed_annotation/freezed_annotation.dart';
{% for line in imports %}{{ line }}
{% endfor %}

part '{{ snake_name }}.freezed.dart';
part '{{ snake_name }}.g.dart';

@freezed
class {{ class_name }} with _${{ class_name }} {
  const {{ class_name }}._();

  const factory {{ class_name }}({
{% for f in fields %}    {% if f.required %}required {{ f.dart_type }} {{ f.dart_name }}{% else %}{{ f.dart_type }} {{ f.dart_name }}{% endif %},
{% endfor %}  }) = _{{ class_name }};

  factory {{ class_name }}.fromJson(Map<String, dynamic> json) =>
      _${{ class_name }}FromJson(json);

  /// Applies a partial patch map — keys in [patch] override this instance.
  {{ class_name }} copyWithPatch(Map<String, dynamic> patch) =>
      {{ class_name }}.fromJson({...toJson(), ...patch});
}
```

---

## Supported spec features

### OpenAPI 3.0 / 3.1

| Feature | Support |
|---|---|
| `paths` — GET, POST, PUT, DELETE, PATCH, OPTIONS, HEAD, TRACE | ✅ |
| Path parameters | ✅ |
| Query parameters | ✅ |
| Header parameters | ✅ |
| Cookie parameters | ✅ |
| `requestBody` — `application/json` | ✅ |
| `requestBody` — `multipart/form-data` | ✅ |
| Response body — `application/json` | ✅ |
| Response headers (scalar and array-of-scalar) | ✅ |
| `components/schemas` — object | ✅ |
| `components/schemas` — array | ✅ |
| `components/schemas` — map (`additionalProperties`) | ✅ |
| `components/schemas` — `$ref` alias | ✅ |
| `allOf` (field merging + inheritance detection) | ✅ |
| `anyOf` (untagged union) | ✅ |
| `oneOf` + `discriminator` (tagged union) | ✅ |
| `oneOf` without `discriminator` (untagged union) | ✅ |
| `nullable: true` (OpenAPI 3.0) | ✅ |
| `type: [T, "null"]` (OpenAPI 3.1) | ✅ |
| String enums | ✅ |
| Integer enums | ✅ |
| `default:` field values | ✅ |
| `format: date-time` → `DateTime` | ✅ |
| `format: float` / `double` → `double` | ✅ |
| `format: int32` / `int64` | ✅ |
| Recursive schemas | ✅ |
| `servers:` — single and multiple URLs | ✅ |
| `security` — top-level and per-operation | ✅ |
| `securitySchemes` — apiKey, http bearer, http basic, oauth2, openIdConnect | ✅ |
| Vendor extensions (`x-*`) | ✅ captured on all nodes |
| Response headers — complex object types | ❌ v0.1 limitation |
| `$ref` to external files | ❌ only `#/components/schemas/*` |
| `parameters` at path-item level (OpenAPI 3.x) | ❌ |

### Swagger 2.0

| Feature | Support |
|---|---|
| `paths` — GET, POST, PUT, DELETE, PATCH, OPTIONS, HEAD | ✅ |
| Path, query, header, cookie parameters | ✅ |
| `formData` parameters (treated as query) | ✅ |
| Body parameters | ✅ |
| `definitions` — object, array, map, allOf | ✅ |
| `securityDefinitions` — apiKey, basic, oauth2 | ✅ |
| `host` + `basePath` → base URL | ✅ |
| `nullable` | ❌ (Swagger 2.0 has no nullable concept) |
| `oneOf` / `anyOf` | ❌ (not in Swagger 2.0) |

---

## Repository layout

```
flap/
├── Cargo.toml                   Workspace manifest + top-level "flap" package
├── Cargo.lock
├── README.md
├── src/
│   ├── main.rs                  CLI spec inspector (prints operation/schema summary)
│   └── bin/
│       └── generate_dart.rs     Dart generator CLI binary
├── crates/
│   ├── flap-ir/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs           IR types: Api, Operation, Schema, Field, TypeRef, …
│   ├── flap-spec/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs           OpenAPI 3.x + 3.1 loader and lowering pass
│   │       └── swagger.rs       Swagger 2.0 serde types
│   └── flap-emit-dart/
│       ├── Cargo.toml
│       └── src/lib.rs           Dart emitter: models, client (Dio + http), templates
└── examples/                    (add sample specs and generated output here)
```

### Key types in `flap-ir`

```rust
pub struct Api {
    pub title: String,
    pub base_urls: Vec<String>,
    pub operations: Vec<Operation>,
    pub schemas: Vec<Schema>,
    pub security_schemes: Vec<SecurityScheme>,
    pub security: Vec<String>,
    pub extensions: Extensions,
}

pub struct Operation {
    pub method: HttpMethod,
    pub path: String,
    pub operation_id: Option<String>,
    pub summary: Option<String>,
    pub parameters: Vec<Parameter>,
    pub request_body: Option<RequestBody>,
    pub responses: Vec<Response>,
    pub security: Option<Vec<String>>,
    pub extensions: Extensions,
}

pub struct Field {
    pub name: String,
    pub type_ref: TypeRef,
    pub required: bool,
    pub nullable: bool,
    pub is_recursive: bool,
    pub default_value: Option<DefaultValue>,
    pub extensions: Extensions,
}

pub enum SchemaKind {
    Object { fields: Vec<Field> },
    Array { item: TypeRef },
    Map { value: TypeRef },
    Union { variants, discriminator, variant_tags },
    UntaggedUnion { variants: Vec<TypeRef> },
    Alias { target: String },
}
```

---

## Crate API

### `flap-spec`

```rust
// Load a local file (auto-detects OpenAPI 3.x vs Swagger 2.0)
let api = flap_spec::load("path/to/spec.yaml")?;

// Load a remote spec
let api = flap_spec::load_url("https://example.com/openapi.yaml")?;

// Accept either a path or URL
let api = flap_spec::load_path_or_url(spec_arg)?;

// Parse from an in-memory string (useful in tests)
let api = flap_spec::load_str(yaml_text)?;
let api = flap_spec::load_swagger_str(yaml_text)?;
```

Validation runs automatically inside `load*` — a `Result::Err` contains all accumulated errors.

### `flap-emit-dart`

```rust
use flap_emit_dart::{ClientBackend, MappingConfig, NullSafety, TemplateConfig};

// Generate model files → HashMap<filename, dart_source>
let files = flap_emit_dart::emit_models(&api, NullSafety::Safe, &mappings, &templates);

// Generate client file → (filename, dart_source)
let (filename, source) = flap_emit_dart::emit_client(
    &api,
    NullSafety::Safe,
    ClientBackend::Dio,
    &mappings,
    &templates,
);
```

---

## Contributing

### Build and test

```bash
cargo build --workspace
cargo test --workspace
```

### Useful first contributions

- **Add example specs** — put small OpenAPI files in `examples/` with the expected generated Dart alongside them. These serve as both documentation and regression tests.
- **CI** — a GitHub Actions workflow that runs `cargo test`, then generates a Dart client from a fixture spec and runs `dart analyze` on the output.
- **`parameters` at path-item level** — OpenAPI 3.x allows parameters on the path item that are inherited by all operations. The Swagger 2.0 lowering already handles this; the OpenAPI 3.x lowering does not yet.
- **Per-operation security override** — the IR carries `Operation.security` but the emitter does not yet act on it to generate per-method auth logic.
- **LICENSE** — the repository needs a license file (MIT or Apache-2.0 recommended).

### Development tips

```bash
# Inspect a spec without generating output
cargo run --package flap -- path/to/spec.yaml

# Iterate on the emitter — change src, regenerate, check diff
cargo run --bin generate_dart -- --force --out /tmp/test-out my-spec.yaml

# Run only emitter tests
cargo test --package flap-emit-dart

# Run only spec loader tests
cargo test --package flap-spec
```

---

## License

MIT — see [LICENSE](LICENSE) for the full text.
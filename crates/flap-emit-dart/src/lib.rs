//! Dart / Flutter code emitter (Phase 4 — production output).
//!
//! Public API:
//! - [`emit_models`] → one Dart source per top-level schema, plus one per
//!   synthesised inline enum (`Pet.status` → `pet_status.dart`).
//! - [`emit_client`] → a single Dio client file with one method per operation,
//!   with real signatures (return types from the success response, named
//!   arguments per parameter and body) and real bodies (path templating,
//!   `queryParameters` map, request `data`, response deserialisation).
//!
//! ## Output conventions (DECISIONS D3 — Freezed + Dio)
//! - Object schemas → `@freezed` class with `fromJson` factory.
//! - Array schemas → `typedef Name = List<ItemType>;`
//! - Inline `enum: [...]` → a separate Dart `enum` file annotated with
//!   `@JsonValue('<original>')` on each case so the on-the-wire string
//!   round-trips through json_serializable.
//! - Operations → real Dio methods on a generated client class.
//!
//! ## Synthetic enum names
//! `TypeRef::Enum` is a structural value with no name of its own, but Dart
//! enums need names. Every inline enum is given a synthesised PascalCase name
//! based on its containing context:
//!
//! | Where it appears                  | Synth name                          |
//! |-----------------------------------|-------------------------------------|
//! | `Pet.status` field                | `PetStatus`                         |
//! | `listPets` op's `status` query    | `ListPetsStatus`                    |
//!
//! The name is consulted everywhere the enum's Dart type is needed —
//! Freezed field declarations, parameter types, deserialisation expressions,
//! and the `import 'pet_status.dart';` directives at the top of dependent
//! files. Conflicts (two contexts producing the same name) are not expected
//! in real specs, but if they occur the registry is last-write-wins with
//! the `enums` BTreeMap keyed by synth name.
//!
//! ## Name collisions
//!
//! - **Class names that clash with Dart core identifiers** (DECISIONS D7):
//!   `Error`, `Type`, etc. get a `Model` suffix. Applied at every usage —
//!   class declaration, file name, named-ref types in fields/responses,
//!   `*.fromJson(...)` deserialisation calls.
//! - **Field names that aren't valid Dart camelCase**: the OpenAPI spec
//!   often uses `snake_case` (e.g. `created_at`). Dart property names get
//!   the camelCase form (`createdAt`) and the original key is preserved
//!   on the wire via `@JsonKey(name: '<original>')`. Names already in
//!   camelCase (the entire PetStore set: `id`, `name`, `tag`) emit no
//!   annotation.
//! - **Parameter names that collide with Dart reserved words** (DECISIONS
//!   D10): a `Param` suffix is appended in the Dart signature. The original
//!   name is preserved on the wire — path template uses the renamed
//!   identifier in `${...}` interpolation, but query/header maps and the
//!   `{...}` placeholders in the URL still use the original spec name.
//! - **Two parameters in the same operation sharing a name across
//!   locations** (DECISIONS D10): hard error during generation. We panic
//!   rather than silently shadow.
//!
//! ## Phase 5 — authentication
//!
//! When `Api.security_schemes` is non-empty, the emitted client constructor
//! gains one optional `String?` parameter per scheme and installs a Dio
//! `InterceptorsWrapper` that injects the supplied credentials into every
//! outgoing request. Specifically:
//!
//! - `HttpBearer` → `options.headers['Authorization'] = 'Bearer $token'`
//! - `ApiKey` (header) → `options.headers['<name>'] = key`
//! - `ApiKey` (query)  → `options.queryParameters['<name>'] = key`
//! - `ApiKey` (cookie) → appends `<name>=$key` to any existing `Cookie`
//!   header so caller-set cookies survive.
//!
//! Each injection is wrapped in `if (cred != null) { ... }` so omitted
//! credentials produce no header — endpoints that opt out of auth via
//! `security: []` continue to work without surprises. Per-operation
//! overrides (`Operation.security`) live in the IR but are not yet routed
//! through; the interceptor is global. Refining that — e.g. to skip
//! credential injection on operations that explicitly declare `security: []`
//! — is a follow-up for a future phase.

use std::collections::{BTreeMap, HashMap};

use flap_ir::{
    Api, ApiKeyLocation, Field, Operation, ParameterLocation, RequestBody, Response, Schema,
    SchemaKind, SecurityScheme, TypeRef,
};

// ── Identifier policy ────────────────────────────────────────────────────────

/// Schema names that collide with Dart core identifiers (DECISIONS D7).
/// When a schema name is in this list, the emitted class gets a "Model" suffix.
const DART_CORE_COLLISIONS: &[&str] = &[
    "bool",
    "DateTime",
    "double",
    "Duration",
    "Error",
    "Exception",
    "Function",
    "Future",
    "int",
    "Iterable",
    "List",
    "Map",
    "num",
    "Object",
    "Pattern",
    "Record",
    "RegExp",
    "Set",
    "Stream",
    "String",
    "Symbol",
    "Type",
    "Uri",
];

/// Dart reserved keywords that may not be used as identifiers (DECISIONS D10).
/// Limited to entries that could plausibly appear as an OpenAPI parameter
/// or field name. Built-in types like `int` are handled separately by D7.
const DART_RESERVED_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "assert",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "covariant",
    "default",
    "deferred",
    "do",
    "dynamic",
    "else",
    "enum",
    "export",
    "extends",
    "extension",
    "external",
    "factory",
    "false",
    "final",
    "finally",
    "for",
    "get",
    "hide",
    "if",
    "implements",
    "import",
    "in",
    "interface",
    "is",
    "late",
    "library",
    "mixin",
    "new",
    "null",
    "of",
    "on",
    "operator",
    "part",
    "required",
    "rethrow",
    "return",
    "set",
    "show",
    "static",
    "super",
    "switch",
    "sync",
    "this",
    "throw",
    "true",
    "try",
    "typedef",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// Dart class name for an OpenAPI schema. Appends "Model" on collision (D7).
fn dart_class_name(schema_name: &str) -> String {
    if DART_CORE_COLLISIONS.contains(&schema_name) {
        format!("{schema_name}Model")
    } else {
        schema_name.to_string()
    }
}

/// Dart identifier safe to use as an argument or local variable name.
/// Reserved words get a `Param` suffix; the wire-side name is unchanged.
fn escape_dart_keyword(name: &str) -> String {
    if DART_RESERVED_KEYWORDS.contains(&name) {
        format!("{name}Param")
    } else {
        name.to_string()
    }
}

// ── Synthetic enum registry ──────────────────────────────────────────────────

/// One inline `enum: [...]` discovered while walking the API.
#[derive(Debug, Clone)]
struct SynthEnum {
    /// PascalCase synthesised type name (e.g. "PetStatus").
    name: String,
    /// String values in spec order. Preserved verbatim — the on-the-wire
    /// representation lives on `@JsonValue` annotations, while the Dart
    /// enum case names are derived separately.
    values: Vec<String>,
}

/// Lookup tables for enum types and their location-derived names.
#[derive(Debug, Default)]
struct EnumRegistry {
    /// (schema_name, field_name) → synth enum name.
    field_enums: HashMap<(String, String), String>,
    /// (operation_id, param_name) → synth enum name. Operations without
    /// `operationId` are skipped — we have no stable way to name the enum.
    param_enums: HashMap<(String, String), String>,
    /// All synth enums, keyed by name. BTreeMap for deterministic iteration
    /// (file emission order matters for golden-output testing).
    enums: BTreeMap<String, SynthEnum>,
}

impl EnumRegistry {
    fn build(api: &Api) -> Self {
        let mut reg = Self::default();

        // Schemas: object fields with TypeRef::Enum.
        for schema in &api.schemas {
            if let SchemaKind::Object { fields } = &schema.kind {
                for field in fields {
                    if let TypeRef::Enum(values) = &field.type_ref {
                        let synth = format!("{}{}", schema.name, to_pascal_case(&field.name));
                        reg.field_enums
                            .insert((schema.name.clone(), field.name.clone()), synth.clone());
                        reg.enums.insert(
                            synth.clone(),
                            SynthEnum {
                                name: synth,
                                values: values.clone(),
                            },
                        );
                    }
                }
            }
            // Top-level array schemas with an enum item are exotic; skip
            // until a real spec exhibits the pattern.
        }

        // Operations: parameter schemas with TypeRef::Enum.
        for op in &api.operations {
            let Some(op_id) = &op.operation_id else {
                continue;
            };
            let op_pascal = to_pascal_case(op_id);
            for param in &op.parameters {
                if let TypeRef::Enum(values) = &param.type_ref {
                    let synth = format!("{}{}", op_pascal, to_pascal_case(&param.name));
                    reg.param_enums
                        .insert((op_id.clone(), param.name.clone()), synth.clone());
                    reg.enums.insert(
                        synth.clone(),
                        SynthEnum {
                            name: synth,
                            values: values.clone(),
                        },
                    );
                }
            }
        }

        reg
    }

    fn lookup_field(&self, schema: &str, field: &str) -> Option<&str> {
        self.field_enums
            .get(&(schema.to_string(), field.to_string()))
            .map(String::as_str)
    }

    fn lookup_param(&self, op_id: &str, param: &str) -> Option<&str> {
        self.param_enums
            .get(&(op_id.to_string(), param.to_string()))
            .map(String::as_str)
    }
}

// ── Public entry point: models ────────────────────────────────────────────────

/// Returns a map of `filename → Dart source`. One file per schema, plus one
/// file per synthesised inline enum.
pub fn emit_models(api: &Api) -> HashMap<String, String> {
    let registry = EnumRegistry::build(api);
    let mut files = HashMap::new();

    for schema in &api.schemas {
        let class_name = dart_class_name(&schema.name);
        let filename = format!("{}.dart", to_snake_case(&class_name));
        let source = emit_schema(schema, &class_name, &registry, &api.schemas);
        files.insert(filename, source);
    }

    for synth in registry.enums.values() {
        let filename = format!("{}.dart", to_snake_case(&synth.name));
        let source = emit_synth_enum(synth);
        files.insert(filename, source);
    }

    files
}

// ── Schema-shape dispatch ─────────────────────────────────────────────────────

fn emit_schema(
    schema: &Schema,
    class_name: &str,
    registry: &EnumRegistry,
    schemas: &[Schema],
) -> String {
    match &schema.kind {
        SchemaKind::Object { fields } => {
            emit_freezed_class(class_name, &schema.name, fields, registry)
        }
        SchemaKind::Array { item } => emit_array_typedef(class_name, item),
        SchemaKind::Map { value } => emit_map_typedef(class_name, value),
        SchemaKind::Union {
            variants,
            discriminator,
            variant_tags,
        } => emit_freezed_union(
            class_name,
            &schema.name,
            variants,
            discriminator,
            variant_tags,
            schemas,
            registry,
        ),
    }
}

// ── Union schemas → @Freezed union ───────────────────────────────────────────

/// Emits a Freezed sealed-class union for a `SchemaKind::Union`.
///
/// Freezed unions look like:
///
///   @Freezed(unionKey: 'petType')
///   sealed class Pet with _$Pet {
///     const factory Pet.dog({ required String name, required int age }) = Dog;
///     const factory Pet.cat({ required String name, required bool indoor }) = Cat;
///
///     factory Pet.fromJson(Map<String, dynamic> json) => _$PetFromJson(json);
///   }
///
/// Each variant's fields are inlined into its factory parameters. We
/// reuse `emit_field` so JsonKey rewrites, optionality, and synth-enum
/// lookups behave identically to standalone object emission. Imports are
/// gathered across all variants' field types — recursion through Map and
/// Array is handled by `collect_field_imports`.
///
/// The right-hand class name (`= Dog;`) uses the variant schema's Dart
/// class name verbatim; D7 collisions are still applied. If the same
/// schema is also emitted as a standalone `@freezed class`, the user
/// will get a duplicate-definition error from Dart — that interaction
/// is documented as a known v0.1 limitation.
fn emit_freezed_union(
    class_name: &str,
    _schema_name: &str,
    variants: &[TypeRef],
    discriminator: &str,
    variant_tags: &[String],
    schemas: &[Schema],
    registry: &EnumRegistry,
) -> String {
    let snake = to_snake_case(class_name);
    let mut out = String::new();

    out.push_str("import 'package:freezed_annotation/freezed_annotation.dart';\n");

    // Collect imports needed by any variant's fields. We do NOT import the
    // variant classes themselves — Freezed defines them via the `=` on each
    // factory. We DO import every named ref / synth enum referenced inside
    // those variant fields.
    let mut imports: Vec<String> = Vec::new();
    for variant in variants {
        let TypeRef::Named(variant_name) = variant else {
            continue; // lowering rejects non-Named variants; defensive.
        };
        if let Some(SchemaKind::Object { fields }) = named_schema_kind(variant_name, schemas) {
            for field in fields {
                collect_field_imports(
                    &field.type_ref,
                    &field.name,
                    variant_name, // registry keys are scoped to the variant schema
                    class_name,
                    registry,
                    &mut imports,
                );
            }
        }
    }
    imports.sort();
    imports.dedup();
    for line in &imports {
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');

    out.push_str(&format!("part '{snake}.freezed.dart';\n"));
    out.push_str(&format!("part '{snake}.g.dart';\n"));
    out.push('\n');

    out.push_str(&format!("@Freezed(unionKey: '{discriminator}')\n"));
    out.push_str(&format!(
        "sealed class {class_name} with _${class_name} {{\n"
    ));

    for (variant, wire_tag) in variants.iter().zip(variant_tags.iter()) {
        let TypeRef::Named(variant_name) = variant else {
            continue;
        };
        let variant_class = format!("{}{}", class_name, to_pascal_case(variant_name));
        let factory_name = to_camel_case(variant_name);

        let fields: &[Field] = match named_schema_kind(variant_name, schemas) {
            Some(SchemaKind::Object { fields }) => fields.as_slice(),
            // Non-object variants (array/map/union-of-union) are exotic
            // enough that v0.1 just emits an empty factory rather than
            // failing the whole emission. A future phase can bail here
            // once we've seen what real specs do with this.
            _ => &[],
        };

        // Override Freezed's default tag-from-factory-name behaviour only
        // when the wire tag genuinely differs. Common cases (camelCase
        // wire tags matching schema names) emit no annotation, which
        // keeps the output clean for the 90% spec.
        if factory_name != *wire_tag {
            out.push_str(&format!("  @FreezedUnionValue('{wire_tag}')\n"));
        }

        out.push_str(&format!("  const factory {class_name}.{factory_name}({{\n"));
        for field in fields {
            // Pass the variant's schema name so JsonKey + synth-enum lookups
            // resolve against the registry entries created during build().
            out.push_str(&emit_field(field, variant_name, registry));
        }
        out.push_str(&format!("  }}) = {variant_class};\n\n"));
    }

    out.push_str(&format!(
        "  factory {class_name}.fromJson(Map<String, dynamic> json) =>\n"
    ));
    out.push_str(&format!("      _${class_name}FromJson(json);\n"));
    out.push_str("}\n");

    out
}

// ── Top-level array / map → typedef ──────────────────────────────────────────

fn emit_array_typedef(name: &str, item: &TypeRef) -> String {
    let dart_item = to_dart_type(item, None);
    format!(
        "// Generated from OpenAPI array schema `{name}`.\n\
         typedef {name} = List<{dart_item}>;\n"
    )
}

fn emit_map_typedef(name: &str, value: &TypeRef) -> String {
    let dart_value = to_dart_type(value, None);
    format!(
        "// Generated from OpenAPI map schema `{name}`\n\
         // (object with `additionalProperties` and no fixed properties).\n\
         typedef {name} = Map<String, {dart_value}>;\n"
    )
}

// ── Object schemas → @freezed class ──────────────────────────────────────────

fn emit_freezed_class(
    class_name: &str,
    schema_name: &str,
    fields: &[Field],
    registry: &EnumRegistry,
) -> String {
    let snake = to_snake_case(class_name);
    let mut out = String::new();

    // Freezed import.
    out.push_str("import 'package:freezed_annotation/freezed_annotation.dart';\n");

    // Cross-file imports: any synth enum referenced by a field, plus any
    // named refs that resolve to schemas (so the tooling sees them, even
    // if they're already part of the same generated package). We recurse
    // through `TypeRef::Map` so `Map<String, Pet>` still imports `pet.dart`.
    let mut imports: Vec<String> = Vec::new();
    for field in fields {
        collect_field_imports(
            &field.type_ref,
            &field.name,
            schema_name,
            class_name,
            registry,
            &mut imports,
        );
    }
    imports.sort();
    imports.dedup();
    for line in &imports {
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');

    // Freezed `part` directives.
    out.push_str(&format!("part '{snake}.freezed.dart';\n"));
    out.push_str(&format!("part '{snake}.g.dart';\n"));
    out.push('\n');

    out.push_str("@freezed\n");
    out.push_str(&format!("class {class_name} with _${class_name} {{\n"));
    out.push_str(&format!("  const factory {class_name}({{\n"));
    for field in fields {
        out.push_str(&emit_field(field, schema_name, registry));
    }
    out.push_str(&format!("  }}) = _{class_name};\n"));
    out.push('\n');
    out.push_str(&format!(
        "  factory {class_name}.fromJson(Map<String, dynamic> json) =>\n"
    ));
    out.push_str(&format!("      _${class_name}FromJson(json);\n"));
    out.push_str("}\n");

    out
}

fn emit_field(field: &Field, schema_name: &str, registry: &EnumRegistry) -> String {
    let synth = registry.lookup_field(schema_name, &field.name);
    let dart_type = to_dart_type(&field.type_ref, synth);
    let dart_name = to_camel_case(&field.name);

    let mut line = String::from("    ");
    if dart_name != field.name {
        // snake_case wire name preserved via @JsonKey, camelCase property.
        line.push_str(&format!("@JsonKey(name: '{}') ", field.name));
    }
    if field.required {
        line.push_str(&format!("required {dart_type} {dart_name},\n"));
    } else {
        line.push_str(&format!("{dart_type}? {dart_name},\n"));
    }
    line
}

/// Walks a field's `TypeRef` and pushes any `import '...dart';` lines the
/// generated source will need. Recurses through `Map` so nested named refs
/// pull in their files too. Self-references (`class_name == cls`) are
/// skipped — Freezed's `part` directives handle those without an import.
fn collect_field_imports(
    type_ref: &TypeRef,
    field_name: &str,
    schema_name: &str,
    class_name: &str,
    registry: &EnumRegistry,
    imports: &mut Vec<String>,
) {
    match type_ref {
        TypeRef::Enum(_) => {
            if let Some(synth) = registry.lookup_field(schema_name, field_name) {
                imports.push(format!("import '{}.dart';", to_snake_case(synth)));
            }
        }
        TypeRef::Named(name) => {
            let cls = dart_class_name(name);
            if cls != class_name {
                imports.push(format!("import '{}.dart';", to_snake_case(&cls)));
            }
        }
        TypeRef::Map(inner) => {
            collect_field_imports(
                inner,
                field_name,
                schema_name,
                class_name,
                registry,
                imports,
            );
        }
        TypeRef::Array(inner) => {
            collect_field_imports(
                inner,
                field_name,
                schema_name,
                class_name,
                registry,
                imports,
            );
        }
        TypeRef::String
        | TypeRef::Integer { .. }
        | TypeRef::Number { .. }
        | TypeRef::Boolean
        | TypeRef::DateTime => {}
    }
}

// ── Synthesised enum file ─────────────────────────────────────────────────────

fn emit_synth_enum(synth: &SynthEnum) -> String {
    let mut out = String::new();
    out.push_str("import 'package:freezed_annotation/freezed_annotation.dart';\n");
    out.push('\n');
    out.push_str(&format!("enum {} {{\n", synth.name));
    for (i, value) in synth.values.iter().enumerate() {
        let case = to_dart_enum_case(value);
        out.push_str(&format!("  @JsonValue('{value}')\n"));
        let trailing = if i == synth.values.len() - 1 {
            ";"
        } else {
            ","
        };
        out.push_str(&format!("  {case}{trailing}\n"));
    }
    out.push_str("}\n");
    out
}

// ── Public entry point: client ────────────────────────────────────────────────

/// Returns `(filename, Dart source)` for the Dio client file.
pub fn emit_client(api: &Api) -> (String, String) {
    let registry = EnumRegistry::build(api);
    let class_name = api_client_name(&api.title);
    let filename = format!("{}.dart", to_snake_case(&class_name));
    let source = emit_client_source(api, &class_name, &registry);
    (filename, source)
}

/// "Swagger Petstore" → "SwaggerPetstoreClient".
fn api_client_name(title: &str) -> String {
    // Each whitespace-separated word becomes a Pascal-cased segment:
    // first char uppercased, remainder lowercased so all-caps words like
    // "API" round-trip as "Api" rather than staying SHOUTED. This mirrors
    // the policy used by `to_camel_case` for separator-bearing inputs.
    let pascal: String = title
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
            }
        })
        .collect();
    format!("{pascal}Client")
}

fn emit_client_source(api: &Api, class_name: &str, registry: &EnumRegistry) -> String {
    let mut out = String::new();

    out.push_str("import 'package:dio/dio.dart';\n");
    out.push('\n');

    // Import every model file plus every synth enum file. The client
    // references most of them; importing the whole set keeps the file
    // stable as operations evolve.
    let mut imports: Vec<String> = Vec::new();
    for schema in &api.schemas {
        let cls = dart_class_name(&schema.name);
        imports.push(format!("import '{}.dart';", to_snake_case(&cls)));
    }
    for synth in registry.enums.values() {
        imports.push(format!("import '{}.dart';", to_snake_case(&synth.name)));
    }
    imports.sort();
    imports.dedup();
    for line in &imports {
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');

    // Phase 5 (auth): each declared security scheme becomes an optional
    // constructor parameter. The Dart identifier is derived from the
    // scheme's registry key (e.g. `bearerAuth` → `bearerAuth`,
    // `X-API-Key` → `xApiKey`), with reserved-word collisions handled
    // by the same `escape_dart_keyword` policy used for path/query
    // parameters. Each entry pairs the original scheme with its chosen
    // Dart identifier so signature, interceptor, and any future
    // per-scheme branching all use the same name.
    let credentials: Vec<DartCredential> = api
        .security_schemes
        .iter()
        .map(DartCredential::from_scheme)
        .collect();

    out.push_str(&format!("class {class_name} {{\n"));
    out.push_str(&emit_constructor(class_name, &credentials));
    out.push('\n');
    out.push_str("  final Dio _dio;\n");

    for op in &api.operations {
        out.push('\n');
        out.push_str(&emit_method(op, &api.schemas, registry));
    }

    out.push_str("}\n");
    out
}

// ── Phase 5 (auth): credential plumbing ──────────────────────────────────────

/// One per `Api.security_schemes` entry. Pre-computes the Dart-side identity
/// of each scheme so the constructor signature, the interceptor body, and
/// any future per-scheme touch point share a single source of truth.
struct DartCredential<'a> {
    scheme: &'a SecurityScheme,
    /// Dart identifier for the constructor parameter, e.g. `bearerAuth`,
    /// `apiKeyAuth`, or `xApiKey` for a scheme literally named `X-API-Key`.
    /// Reserved words are escaped with the same `Param` suffix used for
    /// route parameters (DECISIONS D10).
    dart_param_name: String,
}

impl<'a> DartCredential<'a> {
    fn from_scheme(scheme: &'a SecurityScheme) -> Self {
        let dart_param_name = escape_dart_keyword(&to_camel_case(scheme.scheme_name()));
        Self {
            scheme,
            dart_param_name,
        }
    }
}

/// Emits the constructor — including the auth interceptor wiring when one
/// or more security schemes are present.
///
/// Without auth, the body matches the pre-Phase-5 shape:
/// ```dart
///   FooClient({required String baseUrl})
///       : _dio = Dio(BaseOptions(baseUrl: baseUrl));
/// ```
///
/// With auth, the constructor accepts each credential as an optional named
/// argument and installs an `InterceptorsWrapper` that injects the configured
/// credentials into every outgoing request:
/// ```dart
///   FooClient({
///     required String baseUrl,
///     String? bearerAuth,
///     String? apiKeyAuth,
///   }) : _dio = Dio(BaseOptions(baseUrl: baseUrl)) {
///     _dio.interceptors.add(
///       InterceptorsWrapper(
///         onRequest: (options, handler) {
///           if (bearerAuth != null) {
///             options.headers['Authorization'] = 'Bearer $bearerAuth';
///           }
///           if (apiKeyAuth != null) {
///             options.headers['X-API-Key'] = apiKeyAuth;
///           }
///           handler.next(options);
///         },
///       ),
///     );
///   }
/// ```
///
/// We choose the assertion form `if (cred != null)` even though Dart's
/// null-promotion makes `options.headers[k] = cred!` redundant — the
/// nullable form keeps the interceptor honest about *not* sending headers
/// when no credential is configured, which is the behaviour callers expect
/// for endpoints flagged `security: []`.
fn emit_constructor(class_name: &str, credentials: &[DartCredential]) -> String {
    let mut out = String::new();

    if credentials.is_empty() {
        // Same shape the emitter produced before Phase 5 — keeps the
        // generated output stable for specs that never declare auth.
        out.push_str(&format!("  {class_name}({{required String baseUrl}})\n"));
        out.push_str("      : _dio = Dio(BaseOptions(baseUrl: baseUrl));\n");
        return out;
    }

    // Multi-line constructor signature.
    out.push_str(&format!("  {class_name}({{\n"));
    out.push_str("    required String baseUrl,\n");
    for cred in credentials {
        out.push_str(&format!("    String? {},\n", cred.dart_param_name));
    }
    out.push_str("  }) : _dio = Dio(BaseOptions(baseUrl: baseUrl)) {\n");

    // Interceptor body.
    out.push_str("    _dio.interceptors.add(\n");
    out.push_str("      InterceptorsWrapper(\n");
    out.push_str("        onRequest: (options, handler) {\n");
    for cred in credentials {
        out.push_str(&emit_credential_injection(cred));
    }
    out.push_str("          handler.next(options);\n");
    out.push_str("        },\n");
    out.push_str("      ),\n");
    out.push_str("    );\n");
    out.push_str("  }\n");

    out
}

/// One credential's `if (...) ...` block inside the interceptor.
///
/// - `HttpBearer` → `options.headers['Authorization'] = 'Bearer $cred';`
/// - `ApiKey` in header → `options.headers['<param>'] = cred;`
/// - `ApiKey` in query  → `options.queryParameters['<param>'] = cred;`
/// - `ApiKey` in cookie → append `<param>=$cred` to the `Cookie` header,
///   preserving any existing cookies a caller may have set themselves.
fn emit_credential_injection(cred: &DartCredential) -> String {
    let dart = &cred.dart_param_name;
    match cred.scheme {
        SecurityScheme::HttpBearer { .. } => format!(
            "          if ({dart} != null) {{\n            \
             options.headers['Authorization'] = 'Bearer ${dart}';\n          }}\n"
        ),
        SecurityScheme::ApiKey {
            parameter_name,
            location,
            ..
        } => match location {
            ApiKeyLocation::Header => format!(
                "          if ({dart} != null) {{\n            \
                 options.headers['{parameter_name}'] = {dart};\n          }}\n"
            ),
            ApiKeyLocation::Query => format!(
                "          if ({dart} != null) {{\n            \
                 options.queryParameters['{parameter_name}'] = {dart};\n          }}\n"
            ),
            ApiKeyLocation::Cookie => format!(
                "          if ({dart} != null) {{\n            \
                 final existing = options.headers['Cookie'];\n            \
                 final cookie = '{parameter_name}=${dart}';\n            \
                 options.headers['Cookie'] = existing == null\n                \
                 ? cookie\n                : '$existing; $cookie';\n          }}\n"
            ),
        },
    }
}

// ── Method emission ───────────────────────────────────────────────────────────

/// Bundle of per-parameter naming decisions, computed once and reused by
/// signature, path templating, query / header construction, and any
/// downstream call site.
struct DartParam<'a> {
    /// Original OpenAPI name (used as wire-side key in path / query / header).
    spec_name: &'a str,
    /// Dart identifier used in the signature and method body. May differ
    /// from `spec_name` if the spec name was snake_case (camelCased) or
    /// collided with a Dart reserved keyword (suffixed with `Param`).
    dart_name: String,
    location: ParameterLocation,
    /// The non-nullable Dart type — e.g. `String`, `int`, `PetStatus`.
    /// The signature emitter adds a `?` for optional params and uses the
    /// bare form for required ones.
    non_null_type: String,
    required: bool,
}

fn emit_method(op: &Operation, schemas: &[Schema], registry: &EnumRegistry) -> String {
    let mut out = String::new();

    if let Some(summary) = &op.summary {
        out.push_str(&format!("  /// {summary}\n"));
    }
    out.push_str(&format!("  // {} {}\n", op.method, op.path));

    let method_name = op_method_name(op);
    let dart_params = build_dart_params(op, registry);
    let return_type = success_return_type(&op.responses);

    // Signature.
    if dart_params.is_empty() && op.request_body.is_none() {
        out.push_str(&format!(
            "  Future<{return_type}> {method_name}() async {{\n"
        ));
    } else {
        out.push_str(&format!("  Future<{return_type}> {method_name}({{\n"));

        // Required params first, then body, then optional. Names sorted
        // within each group for deterministic, low-diff output.
        let mut required: Vec<&DartParam> = dart_params.iter().filter(|p| p.required).collect();
        let mut optional: Vec<&DartParam> = dart_params.iter().filter(|p| !p.required).collect();
        required.sort_by(|a, b| a.dart_name.cmp(&b.dart_name));
        optional.sort_by(|a, b| a.dart_name.cmp(&b.dart_name));

        for p in &required {
            out.push_str(&format!(
                "    required {} {},\n",
                p.non_null_type, p.dart_name
            ));
        }
        if let Some(body) = &op.request_body {
            let body_type = to_dart_type(&body.schema_ref, None);
            if body.required {
                out.push_str(&format!("    required {body_type} body,\n"));
            } else {
                out.push_str(&format!("    {body_type}? body,\n"));
            }
        }
        for p in &optional {
            out.push_str(&format!("    {}? {},\n", p.non_null_type, p.dart_name));
        }
        out.push_str("  }) async {\n");
    }

    out.push_str(&emit_method_body(op, &dart_params, schemas, registry));
    out.push_str("  }\n");
    out
}

/// Builds the Dart-facing parameter list and enforces D10's cross-location
/// uniqueness rule. Panics with a clear message if two parameters share a
/// name across path/query/header — generation must fail loudly rather than
/// emit a method that compiles but loses one of the values.
fn build_dart_params<'a>(op: &'a Operation, registry: &EnumRegistry) -> Vec<DartParam<'a>> {
    let op_id = op.operation_id.as_deref().unwrap_or("");
    let mut out = Vec::with_capacity(op.parameters.len());
    let mut seen: HashMap<&str, ParameterLocation> = HashMap::new();

    for param in &op.parameters {
        if let Some(prev) = seen.get(param.name.as_str()) {
            panic!(
                "DECISIONS D10: parameter `{}` of operation `{}` appears in both \
                 `{}` and `{}` locations — cannot emit a Dart method without \
                 losing one of them",
                param.name, op_id, prev, param.location
            );
        }
        seen.insert(&param.name, param.location);

        let synth = registry.lookup_param(op_id, &param.name);
        let non_null_type = to_dart_type(&param.type_ref, synth);
        let dart_name = escape_dart_keyword(&to_camel_case(&param.name));

        out.push(DartParam {
            spec_name: &param.name,
            dart_name,
            location: param.location,
            non_null_type,
            required: param.required,
        });
    }

    out
}

fn emit_method_body(
    op: &Operation,
    dart_params: &[DartParam],
    schemas: &[Schema],
    registry: &EnumRegistry,
) -> String {
    let mut body = String::new();

    // Path templating: replace each {spec_name} with ${dart_name}. Spec
    // names that don't match a declared path parameter pass through
    // untouched so a malformed input is still visibly malformed in the
    // output rather than silently mangled.
    let mut templated_path = op.path.clone();
    for p in dart_params {
        if p.location == ParameterLocation::Path {
            let needle = format!("{{{}}}", p.spec_name);
            let repl = format!("${{{}}}", p.dart_name);
            templated_path = templated_path.replace(&needle, &repl);
        }
    }
    let dart_path_literal = format!("'{templated_path}'");

    // Query parameters: collect into a Map<String, dynamic>, with `if (…)`
    // collection-if guarded entries for optional values so null query
    // params are dropped at the Dio call site.
    let query_params: Vec<&DartParam> = dart_params
        .iter()
        .filter(|p| p.location == ParameterLocation::Query)
        .collect();
    if !query_params.is_empty() {
        body.push_str("    final queryParameters = <String, dynamic>{\n");
        for p in &query_params {
            if p.required {
                body.push_str(&format!("      '{}': {},\n", p.spec_name, p.dart_name));
            } else {
                body.push_str(&format!(
                    "      if ({} != null) '{}': {},\n",
                    p.dart_name, p.spec_name, p.dart_name
                ));
            }
        }
        body.push_str("    };\n");
    }

    // Header parameters: same shape as queries, attached via Options(headers:).
    let header_params: Vec<&DartParam> = dart_params
        .iter()
        .filter(|p| p.location == ParameterLocation::Header)
        .collect();
    if !header_params.is_empty() {
        body.push_str("    final headers = <String, dynamic>{\n");
        for p in &header_params {
            if p.required {
                body.push_str(&format!("      '{}': {},\n", p.spec_name, p.dart_name));
            } else {
                body.push_str(&format!(
                    "      if ({} != null) '{}': {},\n",
                    p.dart_name, p.spec_name, p.dart_name
                ));
            }
        }
        body.push_str("    };\n");
    }

    // Body data expression.
    let data_expr = op.request_body.as_ref().map(body_data_expression);

    // Return-type analysis decides whether to await-and-discard or
    // capture-and-deserialise.
    let success_schema = success_response_schema(&op.responses);
    let needs_response_var = success_schema.is_some();

    // Compose the Dio call.
    let response_assign = if needs_response_var {
        "    final response = "
    } else {
        "    "
    };
    body.push_str(response_assign);
    body.push_str("await _dio.request<dynamic>(\n");
    body.push_str(&format!("      {dart_path_literal},\n"));

    // Options: method, plus headers if any. Build inline to keep the
    // method body compact.
    let method_str = op.method.to_string();
    if header_params.is_empty() {
        body.push_str(&format!(
            "      options: Options(method: '{method_str}'),\n"
        ));
    } else {
        body.push_str(&format!(
            "      options: Options(method: '{method_str}', headers: headers),\n"
        ));
    }

    if !query_params.is_empty() {
        body.push_str("      queryParameters: queryParameters,\n");
    }
    if let Some(expr) = &data_expr {
        body.push_str(&format!("      data: {expr},\n"));
    }
    body.push_str("    );\n");

    // Deserialise the response body if there is a schema to honour.
    if let Some(schema) = success_schema {
        body.push_str("    final data = response.data;\n");
        let expr = deserialize_expr(schema, schemas, registry, "data");
        body.push_str(&format!("    return {expr};\n"));
    }

    body
}

/// Builds the right-hand side of `data: ...` for the Dio call.
///
/// - JSON object body → `body.toJson()`. Freezed classes implement `toJson`,
///   so this round-trips cleanly through json_serializable.
/// - JSON array body → `body` directly; Dart's `jsonEncode` walks lists and
///   calls `.toJson()` on each element via its default `toEncodable`.
/// - Multipart body → wrap in `FormData.fromMap` so Dio sends a multipart
///   request. The IR's `is_multipart` flag is the single source of truth
///   for this branch.
/// - Primitive body (string, int, etc.) → pass through. Real specs rarely
///   use a non-object request body, so this branch is mostly defensive.
fn body_data_expression(body: &RequestBody) -> String {
    if body.is_multipart {
        return "FormData.fromMap(body.toJson())".into();
    }
    match &body.schema_ref {
        TypeRef::Named(_) => "body.toJson()".into(),
        _ => "body".into(),
    }
}

/// Returns the chosen success response schema (preferring the lowest 2xx
/// status code). `None` means "no body" or "no 2xx response".
fn success_response_schema(responses: &[Response]) -> Option<&TypeRef> {
    success_response(responses).and_then(|r| r.schema_ref.as_ref())
}

fn success_response(responses: &[Response]) -> Option<&Response> {
    responses
        .iter()
        .find(|r| matches!(r.status_code.parse::<u16>(), Ok(c) if (200..300).contains(&c)))
}

/// Returns the Dart return type for an operation, derived from its
/// chosen success response. Falls back to `void` when there is no 2xx
/// response or the response declares no body.
///
/// Top-level enum responses are uncommon and we don't synthesise a name
/// for them, so they degrade to `String` via `to_dart_type`'s fallback.
/// If they become important, a separate per-operation-and-status registry
/// can be added without disturbing the rest of the emitter.
fn success_return_type(responses: &[Response]) -> String {
    match success_response_schema(responses) {
        Some(t) => to_dart_type(t, None),
        None => "void".into(),
    }
}

fn deserialize_expr(
    type_ref: &TypeRef,
    schemas: &[Schema],
    registry: &EnumRegistry,
    data_var: &str,
) -> String {
    match type_ref {
        TypeRef::String => format!("{data_var} as String"),
        TypeRef::Integer { .. } => format!("({data_var} as num).toInt()"),
        TypeRef::Number { format } => match format.as_deref() {
            Some("float" | "double") => format!("({data_var} as num).toDouble()"),
            _ => format!("{data_var} as num"),
        },
        TypeRef::Boolean => format!("{data_var} as bool"),
        TypeRef::DateTime => format!("DateTime.parse({data_var} as String)"),
        TypeRef::Map(inner) => {
            let value_ty = to_dart_type(inner, None);
            let inner_expr = deserialize_expr(inner, schemas, registry, "v");
            format!(
                "({data_var} as Map<String, dynamic>).map(\n      \
                 (k, v) => MapEntry(k, {inner_expr}),\n    ).cast<String, {value_ty}>()"
            )
        }
        TypeRef::Array(inner) => {
            let inner_expr = deserialize_expr(inner, schemas, registry, "e");
            format!(
                "({data_var} as List<dynamic>)\n        .map((e) => {inner_expr})\n        .toList()"
            )
        }
        TypeRef::Enum(_) => {
            // Top-level enum response — uncommon. The wire form is a string;
            // downstream code can construct the synthesised enum case
            // explicitly if a future caller needs it.
            format!("{data_var} as String")
        }
        TypeRef::Named(name) => match named_schema_kind(name, schemas) {
            Some(SchemaKind::Object { .. }) | Some(SchemaKind::Union { .. }) => {
                let cls = dart_class_name(name);
                format!("{cls}.fromJson({data_var} as Map<String, dynamic>)")
            }
            Some(SchemaKind::Array { item }) => {
                let item_expr = deserialize_expr(item, schemas, registry, "e");
                format!(
                    "({data_var} as List<dynamic>)\n        .map((e) => {item_expr})\n        .toList()"
                )
            }
            // A $ref to a top-level pure-map schema. The Dart side is a
            // `typedef <Name> = Map<String, V>;` so the runtime shape is
            // identical to an inline map — defer to the same casting pipe.
            Some(SchemaKind::Map { value }) => {
                let value_ty = to_dart_type(value, None);
                let inner_expr = deserialize_expr(value, schemas, registry, "v");
                format!(
                    "({data_var} as Map<String, dynamic>).map(\n      \
                     (k, v) => MapEntry(k, {inner_expr}),\n    ).cast<String, {value_ty}>()"
                )
            }
            None => {
                // Reference into a missing schema; lowering would have rejected
                // this, so it's an internal-consistency failure if hit.
                let cls = dart_class_name(name);
                format!("{cls}.fromJson({data_var} as Map<String, dynamic>)")
            }
        },
    }
}

fn named_schema_kind<'a>(name: &str, schemas: &'a [Schema]) -> Option<&'a SchemaKind> {
    schemas.iter().find(|s| s.name == name).map(|s| &s.kind)
}

/// Operation method name. Prefers `operationId`; falls back to a slug
/// derived from method + path. Real specs always set `operationId` for
/// generated SDKs, so the fallback is mostly safety net.
fn op_method_name(op: &Operation) -> String {
    if let Some(id) = &op.operation_id {
        return id.clone();
    }
    let path_slug: String = op
        .path
        .split('/')
        .filter(|s| !s.is_empty() && !s.starts_with('{'))
        .flat_map(|s| {
            let mut chars = s.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        })
        .collect();
    let method = format!("{}", op.method).to_lowercase();
    format!("{method}{path_slug}")
}

// ── Type mapping ──────────────────────────────────────────────────────────────

/// Maps an IR `TypeRef` to a Dart type expression.
///
/// `enum_synth_name` is the synthesised PascalCase name produced by the
/// [`EnumRegistry`]. For inline enums in a context the registry knows about
/// (object fields, operation parameters), the caller threads the name in;
/// the function returns it directly. For nested or anonymous enums where
/// no synth name is available, we fall back to `String` — the on-the-wire
/// representation for v0.1's string-only enum support.
fn to_dart_type(type_ref: &TypeRef, enum_synth_name: Option<&str>) -> String {
    match type_ref {
        TypeRef::String => "String".into(),
        TypeRef::Integer { .. } => "int".into(),
        TypeRef::Number { format } => match format.as_deref() {
            Some("float" | "double") => "double".into(),
            _ => "num".into(),
        },
        TypeRef::Boolean => "bool".into(),
        TypeRef::DateTime => "DateTime".into(),
        TypeRef::Enum(_) => enum_synth_name
            .map(str::to_string)
            .unwrap_or_else(|| "String".into()),
        TypeRef::Map(inner) => format!("Map<String, {}>", to_dart_type(inner, None)),
        TypeRef::Array(inner) => format!("List<{}>", to_dart_type(inner, None)),
        TypeRef::Named(name) => dart_class_name(name),
    }
}

// ── Naming utilities ──────────────────────────────────────────────────────────

/// PascalCase or camelCase → snake_case.
/// "Pet" → "pet", "ErrorModel" → "error_model", "listPets" → "list_pets",
/// "PetStatus" → "pet_status".
fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(ch.to_lowercase());
    }
    out
}

/// snake_case / kebab-case / already-camelCase → camelCase.
///
/// Splits on `_` and `-` so OpenAPI's three common naming styles all
/// converge: `created_at` → `createdAt`, `X-Auth` → `xAuth`,
/// `content-type` → `contentType`, `petId` → `petId` (unchanged),
/// `API_KEY` → `apiKey`, `X-API-Key` → `xApiKey`.
///
/// When the input contains no separators we pass it through unchanged —
/// already-camelCase names like `petId` must round-trip without being
/// flattened. With separators present, each split segment is lowercased
/// so that fully-uppercase initialisms (`API`, `URL`) are preserved as
/// camelCase fragments rather than left in their SHOUTED form.
fn to_camel_case(s: &str) -> String {
    if !s.contains('_') && !s.contains('-') {
        return s.to_string();
    }
    let parts: Vec<&str> = s
        .split(|c| c == '_' || c == '-')
        .filter(|p| !p.is_empty())
        .collect();
    let mut out = String::with_capacity(s.len());
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            out.push_str(&part.to_lowercase());
        } else {
            let mut chars = part.chars();
            if let Some(c) = chars.next() {
                out.extend(c.to_uppercase());
            }
            out.push_str(&chars.as_str().to_lowercase());
        }
    }
    out
}

/// snake_case or camelCase → PascalCase.
/// "status" → "Status", "created_at" → "CreatedAt", "listPets" → "ListPets".
fn to_pascal_case(s: &str) -> String {
    let camel = to_camel_case(s);
    let mut chars = camel.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Wire enum value → Dart enum case name.
/// "available" → "available", "IN_STOCK" → "inStock", "in-stock" → "inStock",
/// "123abc" → "v123abc". The `@JsonValue` annotation carries the original
/// string, so the case name only needs to be a valid Dart identifier.
fn to_dart_enum_case(value: &str) -> String {
    // Replace any non-alphanumeric run with an underscore so we can split
    // cleanly on "_".
    let cleaned: String = value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let parts: Vec<&str> = cleaned.split('_').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return "value".into();
    }
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            out.push_str(&part.to_lowercase());
        } else {
            let mut chars = part.chars();
            if let Some(c) = chars.next() {
                out.extend(c.to_uppercase());
            }
            out.push_str(&chars.as_str().to_lowercase());
        }
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out = format!("v{out}");
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use flap_ir::{
        Api, Field, HttpMethod, Operation, Parameter, ParameterLocation, RequestBody, Response,
        Schema, SchemaKind, TypeRef,
    };

    // ── Fixture: PetStore (matches what flap_spec produces) ──────────────────

    fn pet_schema() -> Schema {
        Schema {
            name: "Pet".into(),
            kind: SchemaKind::Object {
                fields: vec![
                    Field {
                        name: "id".into(),
                        type_ref: TypeRef::Integer {
                            format: Some("int64".into()),
                        },
                        required: true,
                    },
                    Field {
                        name: "name".into(),
                        type_ref: TypeRef::String,
                        required: true,
                    },
                    Field {
                        name: "tag".into(),
                        type_ref: TypeRef::String,
                        required: false,
                    },
                ],
            },
        }
    }

    fn pets_schema() -> Schema {
        Schema {
            name: "Pets".into(),
            kind: SchemaKind::Array {
                item: TypeRef::Named("Pet".into()),
            },
        }
    }

    fn error_schema() -> Schema {
        Schema {
            name: "Error".into(),
            kind: SchemaKind::Object {
                fields: vec![
                    Field {
                        name: "code".into(),
                        type_ref: TypeRef::Integer {
                            format: Some("int32".into()),
                        },
                        required: true,
                    },
                    Field {
                        name: "message".into(),
                        type_ref: TypeRef::String,
                        required: true,
                    },
                ],
            },
        }
    }

    fn petstore_api() -> Api {
        Api {
            title: "Swagger Petstore".into(),
            base_url: Some("http://petstore.swagger.io/v1".into()),
            operations: vec![
                Operation {
                    method: HttpMethod::Get,
                    path: "/pets".into(),
                    operation_id: Some("listPets".into()),
                    summary: Some("List all pets".into()),
                    parameters: vec![Parameter {
                        name: "limit".into(),
                        location: ParameterLocation::Query,
                        type_ref: TypeRef::Integer {
                            format: Some("int32".into()),
                        },
                        required: false,
                    }],
                    request_body: None,
                    responses: vec![
                        Response {
                            status_code: "200".into(),
                            schema_ref: Some(TypeRef::Named("Pets".into())),
                        },
                        Response {
                            status_code: "default".into(),
                            schema_ref: Some(TypeRef::Named("Error".into())),
                        },
                    ],
                    security: None,
                },
                Operation {
                    method: HttpMethod::Post,
                    path: "/pets".into(),
                    operation_id: Some("createPets".into()),
                    summary: Some("Create a pet".into()),
                    parameters: vec![],
                    request_body: Some(RequestBody {
                        content_type: "application/json".into(),
                        schema_ref: TypeRef::Named("Pet".into()),
                        required: true,
                        is_multipart: false,
                    }),
                    responses: vec![
                        Response {
                            status_code: "201".into(),
                            schema_ref: None,
                        },
                        Response {
                            status_code: "default".into(),
                            schema_ref: Some(TypeRef::Named("Error".into())),
                        },
                    ],
                    security: None,
                },
                Operation {
                    method: HttpMethod::Get,
                    path: "/pets/{petId}".into(),
                    operation_id: Some("showPetById".into()),
                    summary: Some("Info for a specific pet".into()),
                    parameters: vec![Parameter {
                        name: "petId".into(),
                        location: ParameterLocation::Path,
                        type_ref: TypeRef::String,
                        required: true,
                    }],
                    request_body: None,
                    responses: vec![
                        Response {
                            status_code: "200".into(),
                            schema_ref: Some(TypeRef::Named("Pet".into())),
                        },
                        Response {
                            status_code: "default".into(),
                            schema_ref: Some(TypeRef::Named("Error".into())),
                        },
                    ],
                    security: None,
                },
            ],
            schemas: vec![error_schema(), pet_schema(), pets_schema()],
            security_schemes: vec![],
            security: vec![],
        }
    }

    // ── Naming utilities ─────────────────────────────────────────────────────

    #[test]
    fn snake_case_conversion() {
        assert_eq!(to_snake_case("Pet"), "pet");
        assert_eq!(to_snake_case("PetStore"), "pet_store");
        assert_eq!(to_snake_case("listPets"), "list_pets");
        assert_eq!(to_snake_case("ErrorModel"), "error_model");
        assert_eq!(to_snake_case("PetStatus"), "pet_status");
    }

    #[test]
    fn camel_case_conversion() {
        assert_eq!(to_camel_case("id"), "id");
        assert_eq!(to_camel_case("name"), "name");
        assert_eq!(to_camel_case("created_at"), "createdAt");
        assert_eq!(to_camel_case("first_name"), "firstName");
        assert_eq!(to_camel_case("petId"), "petId");
        assert_eq!(to_camel_case("API_KEY"), "apiKey");
    }

    #[test]
    fn pascal_case_conversion() {
        assert_eq!(to_pascal_case("status"), "Status");
        assert_eq!(to_pascal_case("created_at"), "CreatedAt");
        assert_eq!(to_pascal_case("listPets"), "ListPets");
        assert_eq!(to_pascal_case("petId"), "PetId");
    }

    #[test]
    fn dart_enum_case_conversion() {
        assert_eq!(to_dart_enum_case("available"), "available");
        assert_eq!(to_dart_enum_case("IN_STOCK"), "inStock");
        assert_eq!(to_dart_enum_case("in-stock"), "inStock");
        assert_eq!(to_dart_enum_case("123abc"), "v123abc");
    }

    #[test]
    fn keyword_escape() {
        assert_eq!(escape_dart_keyword("limit"), "limit");
        assert_eq!(escape_dart_keyword("in"), "inParam");
        assert_eq!(escape_dart_keyword("class"), "classParam");
        assert_eq!(escape_dart_keyword("required"), "requiredParam");
    }

    // ── D7 collisions ────────────────────────────────────────────────────────

    #[test]
    fn d7_error_renamed() {
        assert_eq!(dart_class_name("Error"), "ErrorModel");
    }

    #[test]
    fn d7_non_colliding_unchanged() {
        assert_eq!(dart_class_name("Pet"), "Pet");
        assert_eq!(dart_class_name("Pets"), "Pets");
        assert_eq!(dart_class_name("Order"), "Order");
    }

    #[test]
    fn d7_all_collisions_renamed() {
        for name in DART_CORE_COLLISIONS {
            let result = dart_class_name(name);
            assert!(
                result.ends_with("Model"),
                "{name} should map to {name}Model, got {result}"
            );
        }
    }

    #[test]
    fn d7_named_ref_uses_renamed_type() {
        let dart_type = to_dart_type(&TypeRef::Named("Error".into()), None);
        assert_eq!(dart_type, "ErrorModel");
    }

    // ── Models: Pet (object) ─────────────────────────────────────────────────

    #[test]
    fn pet_emits_freezed_class() {
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Pet", "Pet", &fields_of(&pet_schema()), &registry);
        assert!(src.contains("@freezed"), "missing @freezed");
        assert!(
            src.contains("class Pet with _$Pet"),
            "missing class declaration"
        );
        assert!(src.contains("required int id"), "missing required int id");
        assert!(
            src.contains("required String name"),
            "missing required String name"
        );
        assert!(src.contains("String? tag"), "missing optional String? tag");
        assert!(src.contains(") = _Pet"), "missing = _Pet");
        assert!(src.contains("_$PetFromJson(json)"), "missing fromJson");
        assert!(
            src.contains("part 'pet.freezed.dart'"),
            "missing freezed part"
        );
        assert!(src.contains("part 'pet.g.dart'"), "missing g part");
    }

    #[test]
    fn pet_no_jsonkey_for_camelcase_fields() {
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Pet", "Pet", &fields_of(&pet_schema()), &registry);
        assert!(
            !src.contains("@JsonKey"),
            "PetStore single-word fields should not get @JsonKey, got:\n{src}"
        );
    }

    #[test]
    fn snake_case_fields_get_jsonkey_with_original_name() {
        let schema = Schema {
            name: "User".into(),
            kind: SchemaKind::Object {
                fields: vec![
                    Field {
                        name: "first_name".into(),
                        type_ref: TypeRef::String,
                        required: true,
                    },
                    Field {
                        name: "id".into(),
                        type_ref: TypeRef::String,
                        required: true,
                    },
                ],
            },
        };
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("User", "User", &fields_of(&schema), &registry);
        assert!(
            src.contains("@JsonKey(name: 'first_name') required String firstName"),
            "missing camelCased property + JsonKey, got:\n{src}"
        );
        assert!(
            src.contains("required String id,\n"),
            "single-word field shouldn't have JsonKey, got:\n{src}"
        );
    }

    // ── Models: arrays ───────────────────────────────────────────────────────

    #[test]
    fn pets_emits_typedef() {
        let src = emit_array_typedef("Pets", &TypeRef::Named("Pet".into()));
        assert!(src.contains("typedef Pets = List<Pet>"), "missing typedef");
        assert!(!src.contains("@freezed"), "array must not emit @freezed");
    }

    // ── Models: D7 + file naming ─────────────────────────────────────────────

    #[test]
    fn d7_error_schema_emits_error_model_dart() {
        let api = Api {
            title: "Test".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![error_schema()],
            security_schemes: vec![],
            security: vec![],
        };
        let files = emit_models(&api);
        assert!(files.contains_key("error_model.dart"));
        assert!(!files.contains_key("error.dart"));
        let src = &files["error_model.dart"];
        assert!(src.contains("class ErrorModel"));
        assert!(src.contains("with _$ErrorModel"));
        assert!(src.contains(") = _ErrorModel"));
        assert!(src.contains("_$ErrorModelFromJson"));
        assert!(src.contains("part 'error_model.freezed.dart'"));
        assert!(src.contains("part 'error_model.g.dart'"));
    }

    #[test]
    fn emit_models_keys_include_renamed_and_originals() {
        let api = Api {
            title: "Test".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![error_schema(), pet_schema(), pets_schema()],
            security_schemes: vec![],
            security: vec![],
        };
        let files = emit_models(&api);
        assert!(files.contains_key("error_model.dart"));
        assert!(files.contains_key("pet.dart"));
        assert!(files.contains_key("pets.dart"));
    }

    // ── Models: enums (Phase 4) ──────────────────────────────────────────────

    #[test]
    fn enum_field_synthesises_dart_enum_file() {
        let pet_with_status = Schema {
            name: "Pet".into(),
            kind: SchemaKind::Object {
                fields: vec![Field {
                    name: "status".into(),
                    type_ref: TypeRef::Enum(vec![
                        "available".into(),
                        "pending".into(),
                        "sold".into(),
                    ]),
                    required: false,
                }],
            },
        };
        let api = Api {
            title: "Test".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![pet_with_status],
            security_schemes: vec![],
            security: vec![],
        };
        let files = emit_models(&api);

        // A separate file for the synth enum.
        assert!(
            files.contains_key("pet_status.dart"),
            "missing pet_status.dart, got keys: {:?}",
            files.keys().collect::<Vec<_>>()
        );
        let enum_src = &files["pet_status.dart"];
        assert!(enum_src.contains("enum PetStatus {"), "missing enum decl");
        assert!(
            enum_src.contains("@JsonValue('available')"),
            "missing JsonValue for available"
        );
        assert!(enum_src.contains("  available,"));
        assert!(enum_src.contains("@JsonValue('sold')"));
        assert!(enum_src.contains("  sold;"), "last case must end with `;`");

        // Pet model imports the enum and uses it as the field type.
        let pet_src = &files["pet.dart"];
        assert!(
            pet_src.contains("import 'pet_status.dart';"),
            "missing enum import, got:\n{pet_src}"
        );
        assert!(
            pet_src.contains("PetStatus? status"),
            "missing PetStatus type, got:\n{pet_src}"
        );
    }

    // ── Models: maps and dates (Phase 4) ─────────────────────────────────────

    #[test]
    fn map_field_lowers_to_dart_map() {
        let schema = Schema {
            name: "Item".into(),
            kind: SchemaKind::Object {
                fields: vec![Field {
                    name: "labels".into(),
                    type_ref: TypeRef::Map(Box::new(TypeRef::String)),
                    required: false,
                }],
            },
        };
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Item", "Item", &fields_of(&schema), &registry);
        assert!(
            src.contains("Map<String, String>? labels"),
            "missing Map type, got:\n{src}"
        );
    }

    #[test]
    fn map_of_named_lowers_correctly() {
        let dart_type = to_dart_type(&TypeRef::Map(Box::new(TypeRef::Named("Pet".into()))), None);
        assert_eq!(dart_type, "Map<String, Pet>");
    }

    #[test]
    fn datetime_field_lowers_to_dart_datetime() {
        let schema = Schema {
            name: "Event".into(),
            kind: SchemaKind::Object {
                fields: vec![Field {
                    name: "createdAt".into(),
                    type_ref: TypeRef::DateTime,
                    required: true,
                }],
            },
        };
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Event", "Event", &fields_of(&schema), &registry);
        assert!(
            src.contains("required DateTime createdAt"),
            "missing DateTime, got:\n{src}"
        );
    }

    // ── Client: signatures (Phase 4) ─────────────────────────────────────────

    #[test]
    fn api_client_name_from_title() {
        assert_eq!(api_client_name("Swagger Petstore"), "SwaggerPetstoreClient");
        assert_eq!(api_client_name("My API"), "MyApiClient");
    }

    #[test]
    fn client_class_and_constructor() {
        let (filename, src) = emit_client(&petstore_api());
        assert_eq!(filename, "swagger_petstore_client.dart");
        assert!(src.contains("class SwaggerPetstoreClient"));
        assert!(src.contains("final Dio _dio"));
        assert!(src.contains("required String baseUrl"));
        assert!(src.contains("BaseOptions(baseUrl: baseUrl)"));
    }

    #[test]
    fn client_imports_models() {
        let (_, src) = emit_client(&petstore_api());
        assert!(src.contains("import 'package:dio/dio.dart';"));
        // Error renamed → file is error_model.dart
        assert!(src.contains("import 'error_model.dart';"));
        assert!(src.contains("import 'pet.dart';"));
        assert!(src.contains("import 'pets.dart';"));
    }

    #[test]
    fn list_pets_signature() {
        let (_, src) = emit_client(&petstore_api());
        // Future<Pets> return, optional limit query param.
        assert!(
            src.contains("Future<Pets> listPets({"),
            "missing return type / signature, got:\n{src}"
        );
        assert!(
            src.contains("int? limit,"),
            "missing optional limit, got:\n{src}"
        );
    }

    #[test]
    fn show_pet_by_id_signature() {
        let (_, src) = emit_client(&petstore_api());
        assert!(
            src.contains("Future<Pet> showPetById({"),
            "missing signature, got:\n{src}"
        );
        assert!(
            src.contains("required String petId,"),
            "missing required path param, got:\n{src}"
        );
    }

    #[test]
    fn create_pets_signature_has_required_body() {
        let (_, src) = emit_client(&petstore_api());
        assert!(
            src.contains("Future<void> createPets({"),
            "POST /pets has no 2xx schema → return Future<void>, got:\n{src}"
        );
        assert!(
            src.contains("required Pet body,"),
            "missing required body argument, got:\n{src}"
        );
    }

    #[test]
    fn reserved_keyword_param_renamed_in_signature() {
        // Spec uses `in` as a query parameter name. Dart can't accept it
        // verbatim — we rename to `inParam` in the signature but preserve
        // `'in'` as the wire-side query map key.
        let api = Api {
            title: "T".into(),
            base_url: None,
            operations: vec![Operation {
                method: HttpMethod::Get,
                path: "/things".into(),
                operation_id: Some("listThings".into()),
                summary: None,
                parameters: vec![Parameter {
                    name: "in".into(),
                    location: ParameterLocation::Query,
                    type_ref: TypeRef::String,
                    required: false,
                }],
                request_body: None,
                responses: vec![],
                security: None,
            }],
            schemas: vec![],
            security_schemes: vec![],
            security: vec![],
        };
        let (_, src) = emit_client(&api);
        assert!(
            src.contains("String? inParam,"),
            "param `in` should be escaped to inParam, got:\n{src}"
        );
        assert!(
            src.contains("if (inParam != null) 'in': inParam"),
            "wire key should remain 'in', got:\n{src}"
        );
    }

    #[test]
    #[should_panic(expected = "DECISIONS D10")]
    fn cross_location_name_collision_is_hard_error() {
        // Same name `id` in path and query — D10 says hard error.
        let api = Api {
            title: "T".into(),
            base_url: None,
            operations: vec![Operation {
                method: HttpMethod::Get,
                path: "/things/{id}".into(),
                operation_id: Some("getThing".into()),
                summary: None,
                parameters: vec![
                    Parameter {
                        name: "id".into(),
                        location: ParameterLocation::Path,
                        type_ref: TypeRef::String,
                        required: true,
                    },
                    Parameter {
                        name: "id".into(),
                        location: ParameterLocation::Query,
                        type_ref: TypeRef::String,
                        required: false,
                    },
                ],
                request_body: None,
                responses: vec![],
                security: None,
            }],
            schemas: vec![],
            security_schemes: vec![],
            security: vec![],
        };
        let _ = emit_client(&api);
    }

    // ── Client: bodies (Phase 4) ─────────────────────────────────────────────

    #[test]
    fn list_pets_body_has_query_map_and_list_deserialisation() {
        let (_, src) = emit_client(&petstore_api());
        // Query map.
        assert!(
            src.contains("final queryParameters = <String, dynamic>{"),
            "missing query map, got:\n{src}"
        );
        assert!(
            src.contains("if (limit != null) 'limit': limit,"),
            "missing collection-if entry, got:\n{src}"
        );
        // Dio call.
        assert!(src.contains("await _dio.request<dynamic>("));
        assert!(src.contains("'/pets',"), "missing path literal");
        assert!(src.contains("options: Options(method: 'GET'),"));
        assert!(src.contains("queryParameters: queryParameters,"));
        // List deserialisation.
        assert!(
            src.contains("(data as List<dynamic>)"),
            "missing list cast, got:\n{src}"
        );
        assert!(
            src.contains(".map((e) => Pet.fromJson(e as Map<String, dynamic>))"),
            "missing element deserialisation, got:\n{src}"
        );
    }

    #[test]
    fn show_pet_body_has_path_templating_and_object_deserialisation() {
        let (_, src) = emit_client(&petstore_api());
        assert!(
            src.contains("'/pets/${petId}',"),
            "missing path templating, got:\n{src}"
        );
        assert!(
            src.contains("return Pet.fromJson(data as Map<String, dynamic>);"),
            "missing object deserialisation, got:\n{src}"
        );
    }

    #[test]
    fn create_pets_body_passes_tojson_data_and_no_return() {
        let (_, src) = emit_client(&petstore_api());
        // We expect:  await _dio.request<dynamic>(... data: body.toJson() ...)
        // No `final response` because there's no schema to deserialise.
        assert!(
            src.contains("await _dio.request<dynamic>("),
            "missing dio call, got:\n{src}"
        );
        assert!(
            src.contains("data: body.toJson(),"),
            "missing body data, got:\n{src}"
        );
        // No `final response` for void returns.
        let create_section = src
            .split("createPets")
            .nth(1)
            .expect("createPets should appear in output")
            .split("Future<")
            .next()
            .unwrap_or("");
        assert!(
            !create_section.contains("final response"),
            "void return should not capture response, got:\n{create_section}"
        );
    }

    #[test]
    fn multipart_body_wraps_in_formdata() {
        let api = Api {
            title: "Upload".into(),
            base_url: None,
            operations: vec![Operation {
                method: HttpMethod::Post,
                path: "/upload".into(),
                operation_id: Some("upload".into()),
                summary: None,
                parameters: vec![],
                request_body: Some(RequestBody {
                    content_type: "multipart/form-data".into(),
                    schema_ref: TypeRef::Named("Upload".into()),
                    required: true,
                    is_multipart: true,
                }),
                responses: vec![Response {
                    status_code: "204".into(),
                    schema_ref: None,
                }],
                security: None,
            }],
            schemas: vec![Schema {
                name: "Upload".into(),
                kind: SchemaKind::Object { fields: vec![] },
            }],
            security_schemes: vec![],
            security: vec![],
        };
        let (_, src) = emit_client(&api);
        assert!(
            src.contains("data: FormData.fromMap(body.toJson()),"),
            "multipart body must wrap in FormData.fromMap, got:\n{src}"
        );
    }

    #[test]
    fn header_param_attached_via_options() {
        let api = Api {
            title: "T".into(),
            base_url: None,
            operations: vec![Operation {
                method: HttpMethod::Get,
                path: "/x".into(),
                operation_id: Some("getX".into()),
                summary: None,
                parameters: vec![Parameter {
                    name: "X-Auth".into(),
                    location: ParameterLocation::Header,
                    type_ref: TypeRef::String,
                    required: true,
                }],
                request_body: None,
                responses: vec![],
                security: None,
            }],
            schemas: vec![],
            security_schemes: vec![],
            security: vec![],
        };
        let (_, src) = emit_client(&api);
        assert!(
            src.contains("'X-Auth': "),
            "header should use original spec name as map key, got:\n{src}"
        );
        assert!(
            src.contains("options: Options(method: 'GET', headers: headers),"),
            "options should attach headers, got:\n{src}"
        );
    }

    #[test]
    fn no_2xx_response_returns_void() {
        // Operation with only a `default` response → Future<void>.
        let api = Api {
            title: "T".into(),
            base_url: None,
            operations: vec![Operation {
                method: HttpMethod::Delete,
                path: "/x".into(),
                operation_id: Some("deleteX".into()),
                summary: None,
                parameters: vec![],
                request_body: None,
                responses: vec![Response {
                    status_code: "default".into(),
                    schema_ref: Some(TypeRef::Named("Error".into())),
                }],
                security: None,
            }],
            schemas: vec![error_schema()],
            security_schemes: vec![],
            security: vec![],
        };
        let (_, src) = emit_client(&api);
        assert!(
            src.contains("Future<void> deleteX("),
            "default-only response → void, got:\n{src}"
        );
    }

    // ── Helper for tests ─────────────────────────────────────────────────────

    fn fields_of(schema: &Schema) -> Vec<Field> {
        match &schema.kind {
            SchemaKind::Object { fields } => fields
                .iter()
                .map(|f| Field {
                    name: f.name.clone(),
                    type_ref: f.type_ref.clone(),
                    required: f.required,
                })
                .collect(),
            _ => panic!("expected object schema"),
        }
    }
}

// ── Phase 5 (auth) tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod auth_tests {
    //! Phase 5 (auth) tests. These are deliberately self-contained — they
    //! build minimal `Api` fixtures rather than reusing the petstore helper
    //! so they remain readable in isolation and are easy to extend as new
    //! scheme variants arrive.

    use super::*;
    use flap_ir::{ApiKeyLocation, SecurityScheme};

    fn empty_api(title: &str, schemes: Vec<SecurityScheme>) -> Api {
        Api {
            title: title.into(),
            base_url: None,
            operations: vec![],
            schemas: vec![],
            security_schemes: schemes,
            security: vec![],
        }
    }

    // ── Constructor shape ────────────────────────────────────────────────────

    #[test]
    fn no_security_keeps_pre_phase5_constructor() {
        // Specs without auth must produce the exact same constructor shape
        // we emitted before Phase 5 — single-line initializer, no body block,
        // no interceptor wiring. Anything else is a regression.
        let api = empty_api("Plain", vec![]);
        let (_, src) = emit_client(&api);
        assert!(
            src.contains("PlainClient({required String baseUrl})\n"),
            "expected single-line constructor signature, got:\n{src}"
        );
        assert!(
            src.contains(": _dio = Dio(BaseOptions(baseUrl: baseUrl));\n"),
            "expected initializer-only constructor, got:\n{src}"
        );
        assert!(
            !src.contains("InterceptorsWrapper"),
            "no auth → no interceptor, got:\n{src}"
        );
    }

    #[test]
    fn http_bearer_emits_bearer_token_param_and_header_injection() {
        let api = empty_api(
            "Secure",
            vec![SecurityScheme::HttpBearer {
                scheme_name: "bearerAuth".into(),
                bearer_format: Some("JWT".into()),
            }],
        );
        let (_, src) = emit_client(&api);

        // Constructor signature.
        assert!(
            src.contains(
                "SecureClient({\n    required String baseUrl,\n    String? bearerAuth,\n  })"
            ),
            "constructor should accept optional bearerAuth, got:\n{src}"
        );
        // Initializer + body block.
        assert!(
            src.contains(": _dio = Dio(BaseOptions(baseUrl: baseUrl)) {"),
            "constructor should open a body block when auth is present, got:\n{src}"
        );
        // Interceptor wiring.
        assert!(
            src.contains("_dio.interceptors.add("),
            "missing interceptor registration, got:\n{src}"
        );
        assert!(
            src.contains("InterceptorsWrapper("),
            "missing InterceptorsWrapper, got:\n{src}"
        );
        assert!(
            src.contains("onRequest: (options, handler) {"),
            "missing onRequest handler, got:\n{src}"
        );
        // Header injection — guarded by a null check.
        assert!(
            src.contains("if (bearerAuth != null) {"),
            "missing null guard, got:\n{src}"
        );
        assert!(
            src.contains("options.headers['Authorization'] = 'Bearer $bearerAuth';"),
            "missing Authorization header, got:\n{src}"
        );
        assert!(
            src.contains("handler.next(options);"),
            "interceptor must call handler.next, got:\n{src}"
        );
    }

    #[test]
    fn api_key_in_header_injects_custom_header() {
        let api = empty_api(
            "Secure",
            vec![SecurityScheme::ApiKey {
                scheme_name: "apiKeyAuth".into(),
                parameter_name: "X-API-Key".into(),
                location: ApiKeyLocation::Header,
            }],
        );
        let (_, src) = emit_client(&api);

        assert!(
            src.contains("String? apiKeyAuth,"),
            "missing apiKeyAuth param, got:\n{src}"
        );
        assert!(
            src.contains("if (apiKeyAuth != null) {"),
            "missing null guard, got:\n{src}"
        );
        assert!(
            src.contains("options.headers['X-API-Key'] = apiKeyAuth;"),
            "header should use the spec's parameter name, not the scheme name, got:\n{src}"
        );
    }

    #[test]
    fn api_key_in_query_injects_query_parameter() {
        let api = empty_api(
            "Secure",
            vec![SecurityScheme::ApiKey {
                scheme_name: "apiKeyAuth".into(),
                parameter_name: "api_key".into(),
                location: ApiKeyLocation::Query,
            }],
        );
        let (_, src) = emit_client(&api);

        assert!(
            src.contains("options.queryParameters['api_key'] = apiKeyAuth;"),
            "query-located key must go on queryParameters, got:\n{src}"
        );
        assert!(
            !src.contains("options.headers['api_key']"),
            "query key must not also land in headers, got:\n{src}"
        );
    }

    #[test]
    fn api_key_in_cookie_appends_to_existing_cookie_header() {
        // Caller may have set their own cookies on the request before the
        // interceptor runs (e.g. a session cookie). The injection must
        // preserve those, not clobber them.
        let api = empty_api(
            "Secure",
            vec![SecurityScheme::ApiKey {
                scheme_name: "session".into(),
                parameter_name: "session_id".into(),
                location: ApiKeyLocation::Cookie,
            }],
        );
        let (_, src) = emit_client(&api);

        assert!(
            src.contains("final existing = options.headers['Cookie'];"),
            "cookie injection must read existing header, got:\n{src}"
        );
        assert!(
            src.contains("final cookie = 'session_id=$session';"),
            "cookie value must use spec parameter name, got:\n{src}"
        );
        assert!(
            src.contains("? cookie\n                : '$existing; $cookie';"),
            "must append (with `; ` separator) when caller already set Cookie, got:\n{src}"
        );
    }

    #[test]
    fn multiple_schemes_each_get_their_own_injection_block() {
        // Bearer + ApiKey/header on the same client — both arguments accepted,
        // both injected, both null-guarded independently.
        let api = empty_api(
            "Secure",
            vec![
                SecurityScheme::ApiKey {
                    scheme_name: "apiKeyAuth".into(),
                    parameter_name: "X-API-Key".into(),
                    location: ApiKeyLocation::Header,
                },
                SecurityScheme::HttpBearer {
                    scheme_name: "bearerAuth".into(),
                    bearer_format: None,
                },
            ],
        );
        let (_, src) = emit_client(&api);

        assert!(src.contains("String? apiKeyAuth,"));
        assert!(src.contains("String? bearerAuth,"));
        assert!(src.contains("options.headers['X-API-Key'] = apiKeyAuth;"));
        assert!(src.contains("options.headers['Authorization'] = 'Bearer $bearerAuth';"));

        // Each block is independently guarded — supplying one credential
        // must not affect whether the other gets sent.
        let api_key_guards = src.matches("if (apiKeyAuth != null)").count();
        let bearer_guards = src.matches("if (bearerAuth != null)").count();
        assert_eq!(
            api_key_guards, 1,
            "expected exactly one guard per scheme for apiKeyAuth, got {api_key_guards}"
        );
        assert_eq!(
            bearer_guards, 1,
            "expected exactly one guard per scheme for bearerAuth, got {bearer_guards}"
        );

        // Single shared `handler.next(options);` at the end of the closure.
        assert_eq!(
            src.matches("handler.next(options);").count(),
            1,
            "handler.next must be called exactly once per request"
        );
    }

    #[test]
    fn reserved_word_scheme_name_gets_param_suffix() {
        // A scheme literally named `default` would produce a Dart param
        // called `default`, which is a reserved word. D10's escape policy
        // applies — the param becomes `defaultParam`. The wire-side spec
        // (the header name from `parameter_name`) is unaffected.
        let api = empty_api(
            "Secure",
            vec![SecurityScheme::ApiKey {
                scheme_name: "default".into(),
                parameter_name: "X-API-Key".into(),
                location: ApiKeyLocation::Header,
            }],
        );
        let (_, src) = emit_client(&api);

        assert!(
            src.contains("String? defaultParam,"),
            "reserved word must be escaped in the constructor, got:\n{src}"
        );
        assert!(
            src.contains("if (defaultParam != null) {"),
            "guard must use the escaped name, got:\n{src}"
        );
        assert!(
            src.contains("options.headers['X-API-Key'] = defaultParam;"),
            "header value must use the escaped Dart name, got:\n{src}"
        );
        // The wire-side header key remains the spec's parameter_name —
        // `defaultParam` must not leak there.
        assert!(
            !src.contains("options.headers['default']"),
            "wire header key must not use the (escaped) Dart name, got:\n{src}"
        );
    }

    #[test]
    fn dashed_scheme_name_produces_valid_dart_identifier() {
        // `X-API-Key` as a registry key cannot appear verbatim in Dart —
        // dashes are illegal in identifiers. The exact camelCase shape is
        // up to `to_camel_case` (and is exercised by its own dedicated
        // tests); here we just assert auth-relevant invariants:
        //  1. The constructor parameter is a valid Dart identifier.
        //  2. The wire-side header key is the spec's parameter_name verbatim.
        let api = empty_api(
            "Secure",
            vec![SecurityScheme::ApiKey {
                scheme_name: "X-API-Key".into(),
                parameter_name: "X-API-Key".into(),
                location: ApiKeyLocation::Header,
            }],
        );
        let (_, src) = emit_client(&api);

        let ident_line = src
            .lines()
            .find(|l| l.trim_start().starts_with("String? "))
            .expect("expected a `String? <ident>,` line");
        let ident = ident_line
            .trim()
            .trim_start_matches("String? ")
            .trim_end_matches(',');
        assert!(
            !ident.contains('-'),
            "dart identifier `{ident}` must not contain dashes"
        );
        assert!(
            ident != "X-API-Key",
            "dart identifier must not be the bare spec name"
        );
        assert!(
            src.contains(&format!("options.headers['X-API-Key'] = {ident};")),
            "header injection must use the spec's parameter_name as the key \
             and the dart identifier as the value, got:\n{src}"
        );
    }
}

// ── Phase 6 (top-level maps & inline arrays) tests ───────────────────────────

#[cfg(test)]
mod phase6_tests {
    //! Tests for the post-Phase-5 expansion: top-level pure-map schemas
    //! become Dart typedefs, and inline `type: array` produces
    //! `TypeRef::Array` which renders as `List<T>` everywhere a TypeRef is
    //! used (fields, parameters, responses, request bodies).

    use super::*;
    use flap_ir::{
        Api, Field, HttpMethod, Operation, Parameter, ParameterLocation, Response, Schema,
        SchemaKind, TypeRef,
    };

    // ── Top-level map → typedef ──────────────────────────────────────────────

    #[test]
    fn top_level_map_emits_typedef() {
        let api = Api {
            title: "T".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![Schema {
                name: "UnitsMap".into(),
                kind: SchemaKind::Map {
                    value: TypeRef::String,
                },
            }],
            security_schemes: vec![],
            security: vec![],
        };
        let files = emit_models(&api);
        assert!(
            files.contains_key("units_map.dart"),
            "missing units_map.dart, got: {:?}",
            files.keys().collect::<Vec<_>>()
        );
        let src = &files["units_map.dart"];
        assert!(
            src.contains("typedef UnitsMap = Map<String, String>;"),
            "expected typedef, got:\n{src}"
        );
        assert!(
            !src.contains("@freezed"),
            "map typedef must not be a Freezed class, got:\n{src}"
        );
    }

    #[test]
    fn top_level_map_of_named_ref_in_typedef() {
        let api = Api {
            title: "T".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![
                Schema {
                    name: "Pet".into(),
                    kind: SchemaKind::Object { fields: vec![] },
                },
                Schema {
                    name: "PetCatalog".into(),
                    kind: SchemaKind::Map {
                        value: TypeRef::Named("Pet".into()),
                    },
                },
            ],
            security_schemes: vec![],
            security: vec![],
        };
        let files = emit_models(&api);
        let src = &files["pet_catalog.dart"];
        assert!(
            src.contains("typedef PetCatalog = Map<String, Pet>;"),
            "expected typedef of named ref, got:\n{src}"
        );
    }

    // ── Inline array TypeRef ─────────────────────────────────────────────────

    #[test]
    fn inline_array_field_renders_as_list() {
        let schema = Schema {
            name: "Pet".into(),
            kind: SchemaKind::Object {
                fields: vec![Field {
                    name: "tags".into(),
                    type_ref: TypeRef::Array(Box::new(TypeRef::String)),
                    required: true,
                }],
            },
        };
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Pet", "Pet", &fields_of(&schema), &registry);
        assert!(
            src.contains("required List<String> tags"),
            "expected List<String>, got:\n{src}"
        );
    }

    #[test]
    fn inline_array_of_named_renders_with_import() {
        let schema = Schema {
            name: "Litter".into(),
            kind: SchemaKind::Object {
                fields: vec![Field {
                    name: "pups".into(),
                    type_ref: TypeRef::Array(Box::new(TypeRef::Named("Pet".into()))),
                    required: false,
                }],
            },
        };
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Litter", "Litter", &fields_of(&schema), &registry);
        assert!(
            src.contains("List<Pet>? pups"),
            "expected optional List<Pet>, got:\n{src}"
        );
        assert!(
            src.contains("import 'pet.dart';"),
            "List of Named must import the element's file, got:\n{src}"
        );
    }

    #[test]
    fn inline_array_query_parameter_in_signature() {
        let api = Api {
            title: "T".into(),
            base_url: None,
            operations: vec![Operation {
                method: HttpMethod::Get,
                path: "/forecast".into(),
                operation_id: Some("getForecast".into()),
                summary: None,
                parameters: vec![Parameter {
                    name: "hourly".into(),
                    location: ParameterLocation::Query,
                    type_ref: TypeRef::Array(Box::new(TypeRef::String)),
                    required: false,
                }],
                request_body: None,
                responses: vec![],
                security: None,
            }],
            schemas: vec![],
            security_schemes: vec![],
            security: vec![],
        };
        let (_, src) = emit_client(&api);
        assert!(
            src.contains("List<String>? hourly,"),
            "expected List<String> param, got:\n{src}"
        );
        // Wire-side serialisation: the existing query map shape passes the
        // List value through — Dio handles the repeated-key encoding.
        assert!(
            src.contains("if (hourly != null) 'hourly': hourly,"),
            "expected pass-through into query map, got:\n{src}"
        );
    }

    #[test]
    fn inline_array_response_deserialises_via_list_map() {
        let api = Api {
            title: "T".into(),
            base_url: None,
            operations: vec![Operation {
                method: HttpMethod::Get,
                path: "/things".into(),
                operation_id: Some("listThings".into()),
                summary: None,
                parameters: vec![],
                request_body: None,
                responses: vec![Response {
                    status_code: "200".into(),
                    schema_ref: Some(TypeRef::Array(Box::new(TypeRef::String))),
                }],
                security: None,
            }],
            schemas: vec![],
            security_schemes: vec![],
            security: vec![],
        };
        let (_, src) = emit_client(&api);
        assert!(
            src.contains("Future<List<String>> listThings("),
            "expected Future<List<String>> return, got:\n{src}"
        );
        assert!(
            src.contains("(data as List<dynamic>)"),
            "expected list cast, got:\n{src}"
        );
        assert!(
            src.contains(".map((e) => e as String)"),
            "expected element extraction, got:\n{src}"
        );
    }

    // ── Helper ───────────────────────────────────────────────────────────────

    fn fields_of(schema: &Schema) -> Vec<Field> {
        match &schema.kind {
            SchemaKind::Object { fields } => fields
                .iter()
                .map(|f| Field {
                    name: f.name.clone(),
                    type_ref: f.type_ref.clone(),
                    required: f.required,
                })
                .collect(),
            _ => panic!("expected object schema"),
        }
    }
}

#[test]
fn union_emits_freezed_union_class() {
    let api = Api {
        title: "T".into(),
        base_url: None,
        operations: vec![],
        schemas: vec![
            Schema {
                name: "Dog".into(),
                kind: SchemaKind::Object {
                    fields: vec![
                        Field {
                            name: "petType".into(),
                            type_ref: TypeRef::String,
                            required: true,
                        },
                        Field {
                            name: "name".into(),
                            type_ref: TypeRef::String,
                            required: true,
                        },
                        Field {
                            name: "breed".into(),
                            type_ref: TypeRef::String,
                            required: false,
                        },
                    ],
                },
            },
            Schema {
                name: "Cat".into(),
                kind: SchemaKind::Object {
                    fields: vec![
                        Field {
                            name: "petType".into(),
                            type_ref: TypeRef::String,
                            required: true,
                        },
                        Field {
                            name: "name".into(),
                            type_ref: TypeRef::String,
                            required: true,
                        },
                        Field {
                            name: "indoor".into(),
                            type_ref: TypeRef::Boolean,
                            required: false,
                        },
                    ],
                },
            },
            Schema {
                name: "Pet".into(),
                kind: SchemaKind::Union {
                    variants: vec![TypeRef::Named("Dog".into()), TypeRef::Named("Cat".into())],
                    discriminator: "petType".into(),
                    variant_tags: vec!["dog".into(), "cat".into()],
                },
            },
        ],
        security_schemes: vec![],
        security: vec![],
    };
    let files = emit_models(&api);
    let src = &files["pet.dart"];
    assert!(src.contains("@Freezed(unionKey: 'petType')"), "got:\n{src}");
    assert!(src.contains("sealed class Pet with _$Pet"), "got:\n{src}");
    assert!(src.contains("const factory Pet.dog({"), "got:\n{src}");
    assert!(src.contains("}) = PetDog;"), "got:\n{src}");
    assert!(src.contains("const factory Pet.cat({"), "got:\n{src}");
    assert!(src.contains("}) = PetCat;"), "got:\n{src}");
    assert!(
        src.contains("required String name"),
        "field inlining: {src}"
    );
    assert!(src.contains("bool? indoor"), "optional bool: {src}");
    assert!(src.contains("factory Pet.fromJson"), "got:\n{src}");
    assert!(src.contains("_$PetFromJson(json)"), "got:\n{src}");
    assert!(
        !src.contains("@FreezedUnionValue"),
        "no annotation should fire when tag matches camelCase factory: {src}"
    );
    assert!(src.contains("part 'pet.freezed.dart'"));
    assert!(src.contains("part 'pet.g.dart'"));
}

#[test]
fn explicit_variant_tags_emit_union_value_annotation() {
    let api = Api {
        title: "T".into(),
        base_url: None,
        operations: vec![],
        schemas: vec![
            Schema {
                name: "Dog".into(),
                kind: SchemaKind::Object {
                    fields: vec![Field {
                        name: "name".into(),
                        type_ref: TypeRef::String,
                        required: true,
                    }],
                },
            },
            Schema {
                name: "Cat".into(),
                kind: SchemaKind::Object {
                    fields: vec![Field {
                        name: "name".into(),
                        type_ref: TypeRef::String,
                        required: true,
                    }],
                },
            },
            Schema {
                name: "Pet".into(),
                kind: SchemaKind::Union {
                    variants: vec![TypeRef::Named("Dog".into()), TypeRef::Named("Cat".into())],
                    discriminator: "petType".into(),
                    variant_tags: vec!["v1.dog".into(), "v1.cat".into()],
                },
            },
        ],
        security_schemes: vec![],
        security: vec![],
    };
    let files = emit_models(&api);
    let src = &files["pet.dart"];
    assert!(
        src.contains("@FreezedUnionValue('v1.dog')"),
        "missing annotation for Dog, got:\n{src}"
    );
    assert!(
        src.contains("@FreezedUnionValue('v1.cat')"),
        "missing annotation for Cat, got:\n{src}"
    );
}

#[test]
fn one_of_without_mapping_uses_schema_name_default() {
    // OpenAPI default: wire tag is the schema name verbatim ("Dog").
    // Factory name is camelCased ("dog"). They differ, so we expect
    // an annotation — this pins down the "honest OpenAPI" behaviour
    // even though most specs in practice use lowercase tags.
    let api = Api {
        title: "T".into(),
        base_url: None,
        operations: vec![],
        schemas: vec![
            Schema {
                name: "Dog".into(),
                kind: SchemaKind::Object {
                    fields: vec![Field {
                        name: "name".into(),
                        type_ref: TypeRef::String,
                        required: true,
                    }],
                },
            },
            Schema {
                name: "Pet".into(),
                kind: SchemaKind::Union {
                    variants: vec![TypeRef::Named("Dog".into())],
                    discriminator: "petType".into(),
                    variant_tags: vec!["Dog".into()], // OpenAPI default
                },
            },
        ],
        security_schemes: vec![],
        security: vec![],
    };
    let files = emit_models(&api);
    let src = &files["pet.dart"];
    assert!(src.contains("@FreezedUnionValue('Dog')"), "got:\n{src}");
}

#[test]
fn one_of_with_camelcase_matching_tags_omits_annotation() {
    // The clean path: spec uses lowercase wire tags that match the
    // camelCased factory name. No annotation needed.
    let api = Api {
        title: "T".into(),
        base_url: None,
        operations: vec![],
        schemas: vec![
            Schema {
                name: "Dog".into(),
                kind: SchemaKind::Object {
                    fields: vec![Field {
                        name: "name".into(),
                        type_ref: TypeRef::String,
                        required: true,
                    }],
                },
            },
            Schema {
                name: "Pet".into(),
                kind: SchemaKind::Union {
                    variants: vec![TypeRef::Named("Dog".into())],
                    discriminator: "petType".into(),
                    variant_tags: vec!["dog".into()], // matches factory name
                },
            },
        ],
        security_schemes: vec![],
        security: vec![],
    };
    let files = emit_models(&api);
    let src = &files["pet.dart"];
    assert!(
        !src.contains("@FreezedUnionValue"),
        "no annotation expected when tag matches factory name, got:\n{src}"
    );
}

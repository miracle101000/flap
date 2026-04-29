//! Dart / Flutter code emitter.
//!
//! Public API:
//! - `emit_models` → one Dart source file per schema (Freezed classes / typedefs).
//! - `emit_client` → a single Dio client stub file with one method per operation.
//!
//! Output conventions (DECISIONS D3 — Freezed + Dio):
//! - Object schemas  → `@freezed` class with `fromJson` factory.
//! - Array schemas   → `typedef Name = List<ItemType>;`
//! - Operations      → stub methods on a Dio client class (bodies next session).
//!
//! Name collision (DECISIONS D7):
//! - Schema names that clash with Dart core identifiers get a "Model" suffix.
//! - Applies to both the generated class name and any `TypeRef::Named` reference
//!   to that schema, so callers always see a consistent type name.
//!
//! TODO (post-PetStore): snake_case OpenAPI field names need to_camel_case()
//! + `@JsonKey(name: '...')` annotation. PetStore fields are single words.

use std::collections::HashMap;

use flap_ir::{Api, Field, Operation, Schema, SchemaKind, TypeRef};

// ── D7: Dart core name collision list ────────────────────────────────────────

/// Schema names that collide with Dart core identifiers.
/// When a schema name is in this list, the emitted class gets a "Model" suffix.
/// Source: DECISIONS D7.
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

/// Returns the Dart class name for an OpenAPI schema name.
/// Appends "Model" when the name collides with a Dart core identifier (D7).
fn dart_class_name(schema_name: &str) -> String {
    if DART_CORE_COLLISIONS.contains(&schema_name) {
        format!("{schema_name}Model")
    } else {
        schema_name.to_string()
    }
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Returns a map of `filename → Dart source` — one entry per schema.
/// Filenames are derived from the (potentially collision-renamed) Dart class name.
pub fn emit_models(api: &Api) -> HashMap<String, String> {
    api.schemas
        .iter()
        .map(|schema| {
            let class_name = dart_class_name(&schema.name);
            let filename = format!("{}.dart", to_snake_case(&class_name));
            let source = emit_schema(schema, &class_name);
            (filename, source)
        })
        .collect()
}

/// Returns `(filename, Dart source)` for the Dio client stub.
pub fn emit_client(api: &Api) -> (String, String) {
    let class_name = api_client_name(&api.title);
    let filename = format!("{}.dart", to_snake_case(&class_name));
    let source = emit_client_source(api, &class_name);
    (filename, source)
}

// ── Per-schema dispatch ───────────────────────────────────────────────────────

fn emit_schema(schema: &Schema, class_name: &str) -> String {
    match &schema.kind {
        SchemaKind::Object { fields } => emit_freezed_class(class_name, fields),
        SchemaKind::Array { item } => emit_array_typedef(class_name, item),
    }
}

// ── Object schemas → Freezed class ───────────────────────────────────────────

fn emit_freezed_class(name: &str, fields: &[Field]) -> String {
    let snake = to_snake_case(name);
    let mut out = String::new();

    out.push_str("import 'package:freezed_annotation/freezed_annotation.dart';\n");
    out.push('\n');
    out.push_str(&format!("part '{snake}.freezed.dart';\n"));
    out.push_str(&format!("part '{snake}.g.dart';\n"));
    out.push('\n');

    out.push_str("@freezed\n");
    out.push_str(&format!("class {name} with _${name} {{\n"));

    out.push_str(&format!("  const factory {name}({{\n"));
    for field in fields {
        out.push_str(&emit_field(field));
    }
    out.push_str(&format!("  }}) = _{name};\n"));
    out.push('\n');

    out.push_str(&format!(
        "  factory {name}.fromJson(Map<String, dynamic> json) =>\n"
    ));
    out.push_str(&format!("      _${name}FromJson(json);\n"));

    out.push_str("}\n");
    out
}

fn emit_field(field: &Field) -> String {
    let dart_type = to_dart_type(&field.type_ref);
    // OpenAPI field names in PetStore are single lowercase words — already
    // valid Dart camelCase. Multi-word snake_case names need to_camel_case()
    // + @JsonKey(name: '...') — TODO post-PetStore.
    let dart_name = &field.name;
    if field.required {
        format!("    required {dart_type} {dart_name},\n")
    } else {
        format!("    {dart_type}? {dart_name},\n")
    }
}

// ── Array schemas → typedef ───────────────────────────────────────────────────

fn emit_array_typedef(name: &str, item: &TypeRef) -> String {
    let dart_item = to_dart_type(item);
    format!(
        "// Generated from OpenAPI array schema `{name}`.\n\
         typedef {name} = List<{dart_item}>;\n"
    )
}

// ── Dio client stub ───────────────────────────────────────────────────────────

/// Converts an API title to a PascalCase Dart client class name.
/// "Swagger Petstore" → "SwaggerPetstoreClient"
fn api_client_name(title: &str) -> String {
    let pascal: String = title
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect();
    format!("{pascal}Client")
}

fn emit_client_source(api: &Api, class_name: &str) -> String {
    let mut out = String::new();

    // Package import.
    out.push_str("import 'package:dio/dio.dart';\n");
    out.push('\n');

    // Import each generated model file (sorted for determinism).
    let mut model_imports: Vec<String> = api
        .schemas
        .iter()
        .map(|s| {
            let dart_name = dart_class_name(&s.name);
            format!("import '{}.dart';", to_snake_case(&dart_name))
        })
        .collect();
    model_imports.sort();
    for import in &model_imports {
        out.push_str(import);
        out.push('\n');
    }
    out.push('\n');

    // Class declaration + constructor.
    out.push_str(&format!("class {class_name} {{\n"));
    out.push_str(&format!("  {class_name}({{required String baseUrl}})\n"));
    out.push_str("      : _dio = Dio(BaseOptions(baseUrl: baseUrl));\n");
    out.push('\n');
    out.push_str("  final Dio _dio;\n");

    // One stub method per operation.
    for op in &api.operations {
        out.push('\n');
        if let Some(summary) = &op.summary {
            out.push_str(&format!("  /// {summary}\n"));
        }
        out.push_str(&format!("  // {} {}\n", op.method, op.path));
        let method_name = op_method_name(op);
        out.push_str(&format!("  Future<void> {method_name}() async {{\n"));
        out.push_str(&format!("    throw UnimplementedError('{method_name}');\n"));
        out.push_str("  }\n");
    }

    out.push_str("}\n");
    out
}

/// Returns the Dart method name for an operation.
/// Prefers `operationId`; falls back to a slug derived from method + path.
fn op_method_name(op: &Operation) -> String {
    if let Some(id) = &op.operation_id {
        return id.clone();
    }
    // Fallback: "GET /pets/{petId}" → "getPetsPetId"
    // Post-PetStore concern; all PetStore ops have operationIds.
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

fn to_dart_type(type_ref: &TypeRef) -> String {
    match type_ref {
        TypeRef::String => "String".into(),
        // Dart has one int regardless of int32/int64.
        TypeRef::Integer { .. } => "int".into(),
        // Prefer double for float/double formats; num for unspecified.
        TypeRef::Number { format } => match format.as_deref() {
            Some("float" | "double") => "double".into(),
            _ => "num".into(),
        },
        TypeRef::Boolean => "bool".into(),
        // Apply D7 renaming when resolving named schema references too.
        TypeRef::Named(name) => dart_class_name(name),
    }
}

// ── Name utilities ────────────────────────────────────────────────────────────

/// Converts PascalCase or camelCase to snake_case.
/// "Pet" → "pet", "ErrorModel" → "error_model", "listPets" → "list_pets".
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use flap_ir::{Api, Field, HttpMethod, Operation, Schema, SchemaKind, TypeRef};

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
                    parameters: vec![],
                },
                Operation {
                    method: HttpMethod::Post,
                    path: "/pets".into(),
                    operation_id: Some("createPets".into()),
                    summary: Some("Create a pet".into()),
                    parameters: vec![],
                },
                Operation {
                    method: HttpMethod::Get,
                    path: "/pets/{petId}".into(),
                    operation_id: Some("showPetById".into()),
                    summary: Some("Info for a specific pet".into()),
                    parameters: vec![],
                },
            ],
            schemas: vec![error_schema(), pet_schema(), pets_schema()],
        }
    }

    // ── D7 collision tests ────────────────────────────────────────────────────

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
    fn d7_error_schema_emits_error_model_dart() {
        let api = Api {
            title: "Test".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![error_schema()],
        };
        let files = emit_models(&api);

        // File key uses the renamed class name.
        assert!(
            files.contains_key("error_model.dart"),
            "key should be error_model.dart"
        );
        assert!(
            !files.contains_key("error.dart"),
            "should not emit error.dart"
        );

        let src = &files["error_model.dart"];
        assert!(src.contains("class ErrorModel"), "missing class ErrorModel");
        assert!(
            src.contains("with _$ErrorModel"),
            "missing mixin _$ErrorModel"
        );
        assert!(src.contains("} = _ErrorModel"), "missing = _ErrorModel");
        assert!(
            src.contains("_$ErrorModelFromJson"),
            "missing renamed fromJson"
        );
        assert!(
            src.contains("part 'error_model.freezed.dart'"),
            "missing part directive"
        );
        assert!(
            src.contains("part 'error_model.g.dart'"),
            "missing part directive"
        );
        assert!(
            !src.contains("class Error "),
            "must not emit the colliding name"
        );
    }

    #[test]
    fn d7_named_ref_to_error_uses_renamed_type() {
        // When a field has TypeRef::Named("Error"), to_dart_type should
        // return "ErrorModel", not "Error".
        let dart_type = to_dart_type(&TypeRef::Named("Error".into()));
        assert_eq!(dart_type, "ErrorModel");
    }

    // ── Existing model tests (unchanged behaviour) ────────────────────────────

    #[test]
    fn snake_case_conversion() {
        assert_eq!(to_snake_case("Pet"), "pet");
        assert_eq!(to_snake_case("PetStore"), "pet_store");
        assert_eq!(to_snake_case("listPets"), "list_pets");
        assert_eq!(to_snake_case("ErrorModel"), "error_model");
    }

    #[test]
    fn pet_emits_freezed_class() {
        let src = emit_schema(&pet_schema(), "Pet");
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
        assert!(src.contains("} = _Pet"), "missing = _Pet");
        assert!(src.contains("_$PetFromJson(json)"), "missing fromJson");
        assert!(
            src.contains("part 'pet.freezed.dart'"),
            "missing freezed part"
        );
        assert!(src.contains("part 'pet.g.dart'"), "missing g part");
    }

    #[test]
    fn pets_emits_typedef() {
        let src = emit_schema(&pets_schema(), "Pets");
        assert!(src.contains("typedef Pets = List<Pet>"), "missing typedef");
        assert!(!src.contains("@freezed"), "array must not emit @freezed");
    }

    #[test]
    fn emit_models_keys() {
        let api = Api {
            title: "Test".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![error_schema(), pet_schema(), pets_schema()],
        };
        let files = emit_models(&api);
        assert!(
            files.contains_key("error_model.dart"),
            "missing error_model.dart"
        );
        assert!(files.contains_key("pet.dart"), "missing pet.dart");
        assert!(files.contains_key("pets.dart"), "missing pets.dart");
    }

    // ── Client stub tests ────────────────────────────────────────────────────

    #[test]
    fn api_client_name_from_title() {
        assert_eq!(api_client_name("Swagger Petstore"), "SwaggerPetstoreClient");
        assert_eq!(api_client_name("My API"), "MyApiClient");
    }

    #[test]
    fn client_stub_class_and_constructor() {
        let (filename, src) = emit_client(&petstore_api());
        assert_eq!(filename, "swagger_petstore_client.dart");
        assert!(src.contains("class SwaggerPetstoreClient"), "missing class");
        assert!(src.contains("final Dio _dio"), "missing _dio field");
        assert!(
            src.contains("required String baseUrl"),
            "missing baseUrl param"
        );
        assert!(
            src.contains("BaseOptions(baseUrl: baseUrl)"),
            "missing BaseOptions"
        );
    }

    #[test]
    fn client_stub_methods() {
        let (_, src) = emit_client(&petstore_api());
        assert!(src.contains("Future<void> listPets()"), "missing listPets");
        assert!(
            src.contains("Future<void> createPets()"),
            "missing createPets"
        );
        assert!(
            src.contains("Future<void> showPetById()"),
            "missing showPetById"
        );
        assert!(
            src.contains("UnimplementedError('listPets')"),
            "missing placeholder body"
        );
    }

    #[test]
    fn client_stub_imports_renamed_model() {
        let (_, src) = emit_client(&petstore_api());
        assert!(
            src.contains("import 'package:dio/dio.dart'"),
            "missing dio import"
        );
        // Error was renamed to ErrorModel → file is error_model.dart
        assert!(
            src.contains("import 'error_model.dart'"),
            "missing error_model import"
        );
        assert!(src.contains("import 'pet.dart'"), "missing pet import");
        assert!(src.contains("import 'pets.dart'"), "missing pets import");
    }
}

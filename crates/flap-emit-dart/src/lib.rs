//! Dart / Flutter code emitter.
//!
//! Public API: `emit_models`, which produces one Dart source file per schema.
//! The Dio client emitter comes in a later session.
//!
//! Output conventions (matching DECISIONS D3 — Freezed + Dio):
//! - Object schemas  → `@freezed` class with `fromJson` factory.
//! - Array schemas   → `typedef Name = List<ItemType>;`
//!   Freezed has no concept of a bare list class; a typedef keeps the name
//!   usable in callers without generating dead code.
//!
//! TODO (post-PetStore): when a field name is snake_case in the OpenAPI spec
//! but camelCase in Dart (e.g. `first_name` → `firstName`), emit
//! `@JsonKey(name: 'first_name')` above the parameter. PetStore fields are
//! all single words so this is not needed yet.

use std::collections::HashMap;

use flap_ir::{Api, Field, Schema, SchemaKind, TypeRef};

// ── Public entry point ────────────────────────────────────────────────────────

/// Returns a map of `filename → Dart source` — one entry per schema.
/// The caller decides what to do with the strings (print, write to disk, etc.).
pub fn emit_models(api: &Api) -> HashMap<String, String> {
    api.schemas
        .iter()
        .map(|schema| {
            let filename = format!("{}.dart", to_snake_case(&schema.name));
            let source = emit_schema(schema);
            (filename, source)
        })
        .collect()
}

// ── Per-schema dispatch ───────────────────────────────────────────────────────

fn emit_schema(schema: &Schema) -> String {
    match &schema.kind {
        SchemaKind::Object { fields } => emit_freezed_class(&schema.name, fields),
        SchemaKind::Array { item } => emit_array_typedef(&schema.name, item),
    }
}

// ── Object schemas → Freezed class ───────────────────────────────────────────

fn emit_freezed_class(name: &str, fields: &[Field]) -> String {
    let snake = to_snake_case(name);
    let mut out = String::new();

    // Imports and part directives.
    out.push_str("import 'package:freezed_annotation/freezed_annotation.dart';\n");
    out.push('\n');
    out.push_str(&format!("part '{snake}.freezed.dart';\n"));
    out.push_str(&format!("part '{snake}.g.dart';\n"));
    out.push('\n');

    // Class declaration.
    out.push_str("@freezed\n");
    out.push_str(&format!("class {name} with _${name} {{\n"));

    // Primary constructor.
    out.push_str(&format!("  const factory {name}({{\n"));
    for field in fields {
        out.push_str(&emit_field(field));
    }
    out.push_str(&format!("  }}) = _{name};\n"));

    out.push('\n');

    // fromJson factory.
    out.push_str(&format!(
        "  factory {name}.fromJson(Map<String, dynamic> json) =>\n"
    ));
    out.push_str(&format!("      _${name}FromJson(json);\n"));

    out.push_str("}\n");

    out
}

/// Emits one parameter line inside the `const factory` constructor.
fn emit_field(field: &Field) -> String {
    let dart_type = to_dart_type(&field.type_ref);
    // OpenAPI field names in PetStore are single lowercase words, so they're
    // already valid Dart identifiers with no casing conversion needed.
    // Multi-word names (e.g. snake_case) will need to_camel_case() here — TODO.
    let dart_name = &field.name;

    if field.required {
        format!("    required {dart_type} {dart_name},\n")
    } else {
        // Nullable type + no `required` keyword = optional named parameter.
        format!("    {dart_type}? {dart_name},\n")
    }
}

// ── Array schemas → typedef ───────────────────────────────────────────────────

fn emit_array_typedef(name: &str, item: &TypeRef) -> String {
    let dart_item = to_dart_type(item);
    // A Dart typedef keeps the schema name usable without generating a
    // Freezed class (which would be meaningless for a bare list).
    format!(
        "// Generated from OpenAPI array schema `{name}`.\n\
         typedef {name} = List<{dart_item}>;\n"
    )
}

// ── Type mapping ──────────────────────────────────────────────────────────────

fn to_dart_type(type_ref: &TypeRef) -> String {
    match type_ref {
        TypeRef::String => "String".into(),
        // Dart has a single `int` type regardless of int32/int64.
        TypeRef::Integer { .. } => "int".into(),
        // Prefer `double` for float/double formats; fall back to `num` for
        // unformatted number (which could be int or float at runtime).
        TypeRef::Number { format } => match format.as_deref() {
            Some("float" | "double") => "double".into(),
            _ => "num".into(),
        },
        TypeRef::Boolean => "bool".into(),
        TypeRef::Named(name) => name.clone(),
    }
}

// ── Name utilities ────────────────────────────────────────────────────────────

/// Converts a PascalCase or camelCase identifier to snake_case.
/// `Pet` → `pet`, `PetStore` → `pet_store`, `listPets` → `list_pets`.
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
    use flap_ir::{Field, HttpMethod, Operation, Schema, SchemaKind, TypeRef};

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

    #[test]
    fn snake_case_conversion() {
        assert_eq!(to_snake_case("Pet"), "pet");
        assert_eq!(to_snake_case("PetStore"), "pet_store");
        assert_eq!(to_snake_case("listPets"), "list_pets");
        assert_eq!(to_snake_case("Error"), "error");
    }

    #[test]
    fn pet_emits_freezed_class() {
        let src = emit_schema(&pet_schema());
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
        let src = emit_schema(&pets_schema());
        assert!(src.contains("typedef Pets = List<Pet>"), "missing typedef");
        // Should NOT contain Freezed boilerplate.
        assert!(
            !src.contains("@freezed"),
            "array schema should not emit @freezed"
        );
    }

    #[test]
    fn emit_models_keys() {
        let api = Api {
            title: "Test".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![pet_schema(), pets_schema()],
        };
        let files = emit_models(&api);
        assert!(files.contains_key("pet.dart"), "missing pet.dart");
        assert!(files.contains_key("pets.dart"), "missing pets.dart");
    }
}

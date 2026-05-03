//! Dart / Flutter code emitter (Phase 4 — production output).
//!
//! Public API:
//! - [`emit_models`] — one Dart source per top-level schema, plus one per
//!   synthesised inline enum, plus the shared `flap_utils.dart` runtime.
//! - [`emit_client`] — a single Dio client file with one method per operation.
//!
//! ## Phase 8 — strict nullability vs. omission
//!
//! HTTP PATCH endpoints distinguish three states for a field on the wire:
//! "client did not send this key", "client sent the key with the literal
//! value null", and "client sent the key with a real value". OpenAPI 3.0
//! encodes this with two orthogonal flags:
//!
//! | `required` | `nullable` | wire semantics                              |
//! |------------|------------|---------------------------------------------|
//! | true       | false      | key MUST be present, value non-null         |
//! | true       | true       | key MUST be present, value MAY be null      |
//! | false      | false      | key MAY be omitted, never null when present |
//! | false      | true       | key MAY be omitted, value MAY be null       |
//!
//! The Dart shape per cell:
//! - (true,  false) — `required T name`
//! - (true,  true ) — `required T? name`     (sends `null` literally)
//! - (false, false) — `T? name` + `@JsonKey(includeIfNull: false)`
//! - (false, true ) — `Optional<T?> name`    (the cell that motivates all of this)
//!
//! Only the bottom-right cell needs the `Optional<T?>` wrapper. The
//! wrapper has two states — `Optional.absent()` and `Optional.present(value)` —
//! corresponding directly to "key omitted" and "key present (possibly with null)".
//!
//! A `JsonConverter<Optional<T?>, Object?>` cannot, by itself, both drop
//! a key and emit a literal `null`: `@JsonKey(includeIfNull: false)`
//! checks the converter's output against `null`, so it can't tell those
//! two apart. The working design is: the converter emits a sentinel
//! object for `Optional.absent()`, and the class-level `toJson` override
//! calls `stripOptionalAbsent` to remove sentinel-valued entries from
//! the map before it leaves the model. `includeIfNull: false` still
//! does its proper job for the (false, false) row above.
//!
//! For non-primitive `T` (DateTime, Named, Map, Array, Enum), the
//! generic `OptionalConverter<T>`'s `as T?` cast doesn't survive — the
//! JSON-side runtime types differ. Those fall back to the (false,
//! false) shape with a visible `// TODO(flap)` so the silent loss of
//! the absent/present-null distinction is at least loud. A future phase
//! will emit per-field `fromJson`/`toJson` lambdas to fix that.

use std::collections::{BTreeMap, HashMap};

use flap_ir::{
    Api, ApiKeyLocation, Field, Operation, ParameterLocation, RequestBody, Response, Schema,
    SchemaKind, SecurityScheme, TypeRef,
};

// ── Identifier policy ────────────────────────────────────────────────────────

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

fn dart_class_name(schema_name: &str) -> String {
    if DART_CORE_COLLISIONS.contains(&schema_name) {
        format!("{schema_name}Model")
    } else {
        schema_name.to_string()
    }
}

fn escape_dart_keyword(name: &str) -> String {
    if DART_RESERVED_KEYWORDS.contains(&name) {
        format!("{name}Param")
    } else {
        name.to_string()
    }
}

// ── Synthetic enum registry ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SynthEnum {
    name: String,
    values: Vec<String>,
}

#[derive(Debug, Default)]
struct EnumRegistry {
    field_enums: HashMap<(String, String), String>,
    param_enums: HashMap<(String, String), String>,
    enums: BTreeMap<String, SynthEnum>,
}

impl EnumRegistry {
    fn build(api: &Api) -> Self {
        let mut reg = Self::default();

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
        }

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

/// Returns a map of `filename → Dart source`.
///
/// Every emission includes a `flap_utils.dart` file containing the
/// `Optional<T>` wrapper, the absence sentinel, and the
/// `OptionalConverter<T>` JsonConverter. We emit it unconditionally
/// (rather than only when at least one schema has a nullable+optional
/// field) to keep the file set stable across spec edits — adding a
/// nullable field to a previously-strict schema doesn't change which
/// files exist, only their contents.
pub fn emit_models(api: &Api) -> HashMap<String, String> {
    let registry = EnumRegistry::build(api);
    let mut files = HashMap::new();

    files.insert("flap_utils.dart".to_string(), emit_flap_utils());

    for schema in &api.schemas {
        if schema.internal {
            continue;
        }
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

// ── Phase 8: shared Optional<T> runtime ──────────────────────────────────────

/// The shared `Optional<T>` + `OptionalConverter` runtime.
///
/// Generated as a single file imported by every model that has an
/// `Optional`-wrapped field. The contents are static — the function
/// returns a string literal rather than building from IR — because
/// nothing in here depends on the spec.
fn emit_flap_utils() -> String {
    r#"// GENERATED — do not edit by hand.
// Shared runtime for fields whose absence and explicit-null forms must
// be distinguished on the wire (notably HTTP PATCH bodies).

import 'package:freezed_annotation/freezed_annotation.dart';

/// Tri-state wrapper. `Optional.absent()` means "the key was omitted
/// from the payload"; `Optional.present(value)` means "the key was
/// supplied with this value", where `value` itself may be `null`.
sealed class Optional<T> {
  const Optional();
  const factory Optional.present(T value) = _Present<T>;
  const factory Optional.absent() = _Absent<T>;

  bool get isPresent => this is _Present<T>;
  bool get isAbsent => this is _Absent<T>;

  /// Throws if `isAbsent`. Use `valueOrNull` for a fallback.
  T get value => switch (this) {
        _Present<T>(:final value) => value,
        _Absent<T>() =>
          throw StateError('Optional.value called on Optional.absent()'),
      };

  T? get valueOrNull => switch (this) {
        _Present<T>(:final value) => value,
        _Absent<T>() => null,
      };
}

final class _Present<T> extends Optional<T> {
  final T value;
  const _Present(this.value);

  @override
  bool operator ==(Object other) =>
      identical(this, other) ||
      (other is _Present<T> && other.value == value);

  @override
  int get hashCode => Object.hash(_Present, value);
}

final class _Absent<T> extends Optional<T> {
  const _Absent();

  @override
  bool operator ==(Object other) => other is _Absent<T>;

  @override
  int get hashCode => (_Absent).hashCode;
}

/// Sentinel emitted by [OptionalConverter.toJson] for the absent case.
/// `stripOptionalAbsent` removes any map entry whose value is identical
/// to this object before the map ever reaches `jsonEncode`.
const Object kOptionalAbsentSentinel = _OptionalAbsentSentinel();

class _OptionalAbsentSentinel {
  const _OptionalAbsentSentinel();
}

/// Removes any entry whose value is the absence sentinel. Generated
/// `toJson` overrides on models with `Optional` fields call this on the
/// `_$ClassNameToJson` output before returning.
Map<String, dynamic> stripOptionalAbsent(Map<String, dynamic> m) {
  m.removeWhere((_, v) => identical(v, kOptionalAbsentSentinel));
  return m;
}

/// Converter for `Optional<T?>` where `T` has a direct JSON shape
/// (`String`, `int`, `double`, `num`, `bool`). For non-primitive `T`
/// (DateTime, custom classes, lists, maps), generated code emits
/// per-field `@JsonKey(fromJson: ..., toJson: ...)` lambdas instead,
/// because `as T?` won't survive the JSON-side runtime types.
///
/// Round-trip semantics:
/// - `fromJson(null)` → `Optional.present(null)` (key was present with null)
/// - `fromJson(value)` → `Optional.present(value)`
/// - **the absent case is encoded by NOT calling fromJson at all**, which
///   relies on `@Default(Optional<T?>.absent())` on the field.
/// - `toJson(Optional.absent())` → sentinel (stripped at the boundary)
/// - `toJson(Optional.present(null))` → `null` (preserved as `"key": null`)
/// - `toJson(Optional.present(value))` → `value`
class OptionalConverter<T> implements JsonConverter<Optional<T?>, Object?> {
  const OptionalConverter();

  @override
  Optional<T?> fromJson(Object? json) => Optional<T?>.present(json as T?);

  @override
  Object? toJson(Optional<T?> opt) => switch (opt) {
        _Absent<T?>() => kOptionalAbsentSentinel,
        _Present<T?>(:final value) => value,
      };
}
"#
    .to_string()
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
            emit_freezed_class(class_name, &schema.name, fields, schemas, registry)
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
        SchemaKind::UntaggedUnion { variants } => {
            emit_untagged_union(class_name, &schema.name, variants, schemas, registry)
        }
    }
}

// ── Union schemas → @Freezed union ───────────────────────────────────────────

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

    let mut imports: Vec<String> = Vec::new();
    let mut needs_flap_utils = false;
    for variant in variants {
        let TypeRef::Named(variant_name) = variant else {
            continue;
        };
        if let Some(SchemaKind::Object { fields }) = named_schema_kind(variant_name, schemas) {
            for field in fields {
                collect_field_imports(
                    &field.type_ref,
                    &field.name,
                    variant_name,
                    class_name,
                    registry,
                    &mut imports,
                );
                if field_uses_optional_wrapper(field) {
                    needs_flap_utils = true;
                }
            }
        }
    }
    if needs_flap_utils {
        imports.push("import 'flap_utils.dart';".to_string());
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
            _ => &[],
        };

        if factory_name != *wire_tag {
            out.push_str(&format!("  @FreezedUnionValue('{wire_tag}')\n"));
        }

        out.push_str(&format!("  const factory {class_name}.{factory_name}({{\n"));
        for field in fields {
            out.push_str(&emit_field(field, variant_name, schemas, registry));
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

fn emit_untagged_union(
    class_name: &str,
    _schema_name: &str,
    variants: &[TypeRef],
    schemas: &[Schema],
    registry: &EnumRegistry,
) -> String {
    let snake = to_snake_case(class_name);
    let mut out = String::new();

    // ── Imports ─────────────────────────────────────────────────────
    out.push_str("import 'dart:convert';\n"); // for jsonEncode in toJson
    out.push_str("import 'package:flutter/foundation.dart';\n"); // for listEquals, mapEquals – if needed later; but for equality we'll implement manually. Actually we can just use `identical` and manual checks.
    // We'll keep it simple: no extra imports for equality.

    // Collect imports for named variant classes used in constructors.
    let mut imports: Vec<String> = Vec::new();
    for variant in variants {
        if let TypeRef::Named(variant_name) = variant
            && !is_internal_wrapper(schemas, variant_name)
        {
            let cls = dart_class_name(variant_name);
            imports.push(format!("import '{}.dart';", to_snake_case(&cls)));
        }
    }
    imports.sort();
    imports.dedup();
    for line in &imports {
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');

    // ── Sealed class definition ─────────────────────────────────────
    out.push_str(&format!("sealed class {class_name} {{\n"));
    out.push_str(&format!("  const {class_name}._();\n\n"));

    // Generate constructors for each variant.
    for (i, variant) in variants.iter().enumerate() {
        let (variant_dart_type, variant_param_name, is_primitive) =
            resolve_untagged_variant_info(variant, schemas, registry);

        let factory_name = format!("variant{}", i); // deterministic, not from spec name

        let formal = if is_primitive {
            format!("{variant_dart_type} value")
        } else {
            format!("{variant_dart_type} value")
        };

        out.push_str(&format!(
            "  const factory {class_name}.{factory_name}({formal}) = _Variant{i};\n"
        ));
    }

    out.push_str(&format!(
        "\n  factory {class_name}.fromJson(dynamic json) {{\n"
    ));

    // Try each variant in order.
    for (i, variant) in variants.iter().enumerate() {
        let (variant_dart_type, _, is_primitive) =
            resolve_untagged_variant_info(variant, schemas, registry);
        let factory_name = format!("variant{}", i);

        if is_primitive {
            // Match on JSON type.
            out.push_str(&format!(
                "    if (json is {variant_dart_type}) return {class_name}.{factory_name}(json);\n"
            ));
        } else {
            // Named object: try to call its fromJson.
            out.push_str(&format!("    if (json is Map<String, dynamic>) {{\n"));
            out.push_str(&format!("      try {{\n"));
            out.push_str(&format!(
                "        return {class_name}.{factory_name}({variant_dart_type}.fromJson(json));\n"
            ));
            out.push_str(&format!("      }} catch (_) {{}}\n"));
            out.push_str(&format!("    }}\n"));
        }
    }

    out.push_str(&format!(
        "    throw ArgumentError('Cannot deserialize into {class_name}: $json');\n"
    ));
    out.push_str(&format!("  }}\n\n"));

    // toJson method – dispatch to concrete subclass.
    out.push_str(&format!("  Object? toJson();\n"));
    out.push_str(&format!("}}\n\n"));

    // ── Emit private subclasses ─────────────────────────────────────
    for (i, variant) in variants.iter().enumerate() {
        let (variant_dart_type, _, _) = resolve_untagged_variant_info(variant, schemas, registry);

        out.push_str(&format!("class _Variant{i} extends {class_name} {{\n"));
        out.push_str(&format!("  final {variant_dart_type} value;\n"));
        out.push_str(&format!("  const _Variant{i}(this.value) : super._();\n\n"));

        // toJson for primitive: return the value itself; for object: return value.toJson()
        if is_variant_primitive(variant, schemas) {
            out.push_str(&format!("  @override\n"));
            out.push_str(&format!("  Object? toJson() => value;\n"));
        } else {
            out.push_str(&format!("  @override\n"));
            out.push_str("  Object? toJson() => value.toJson();\n");
        }

        // Equality
        out.push_str(&format!("  @override\n"));
        out.push_str(&"  bool operator ==(Object other) =>\n".to_string());
        out.push_str(&format!(
            "      other is _Variant{i} && other.value == value;\n"
        ));

        out.push_str(&format!("  @override\n"));
        out.push_str(&format!(
            "  int get hashCode => Object.hash(_Variant{i}, value);\n"
        ));
        out.push_str(&format!("}}\n\n"));
    }

    // ── Emit a converter for use with @JsonKey on fields ────────────
    // This allows the field to seamlessly serialize/deserialize the union.
    let converter_name = format!("_{class_name}Converter");
    out.push_str(&format!(
        "class {converter_name} implements JsonConverter<{class_name}, Object?> {{\n"
    ));
    out.push_str(&format!("  const {converter_name}();\n\n"));
    out.push_str(&format!(
        "  @override\n  {class_name} fromJson(Object? json) =>\n      {class_name}.fromJson(json);\n\n"
    ));
    out.push_str(&format!(
        "  @override\n  Object? toJson({class_name} object) => object.toJson();\n"
    ));
    out.push_str(&format!("}}\n"));

    out
}

/// Returns (dart_type_string, parameter_name, is_primitive).
fn resolve_untagged_variant_info(
    type_ref: &TypeRef,
    schemas: &[Schema],
    registry: &EnumRegistry,
) -> (String, String, bool) {
    match type_ref {
        TypeRef::Named(name) => {
            // Check if this is an internal wrapper schema (primitive wrapper)
            if let Some(wrapper_schema) = schemas.iter().find(|s| s.name == *name)
                && wrapper_schema.internal
            {
                // This is a wrapper for a => primitive; extract the inner type.
                if let SchemaKind::Object { fields } = &wrapper_schema.kind
                    && fields.len() == 1
                    && fields[0].name == "value"
                {
                    let inner_type = &fields[0].type_ref;
                    let dart_inner = to_dart_type(inner_type, None);
                    return (dart_inner, "value".to_string(), true);
                }
                // Fallback (shouldn't happen)
                panic!("internal wrapper schema without a single 'value' field");
            }
            // Regular named schema
            let cls = dart_class_name(name);
            (cls, "value".to_string(), false)
        }
        // For any other TypeRef (shouldn't happen for untagged union variants)
        _ => panic!("unexpected TypeRef in untagged union variant"),
    }
}

fn is_internal_wrapper(schemas: &[Schema], variant_name: &str) -> bool {
    schemas.iter().any(|s| s.name == variant_name && s.internal)
}

// Helper to check if a variant is a primitive (i.e., its named schema is an internal wrapper)
fn is_variant_primitive(variant_type_ref: &TypeRef, schemas: &[Schema]) -> bool {
    match variant_type_ref {
        TypeRef::Named(name) => is_internal_wrapper(schemas, name),
        _ => false,
    }
}

fn is_anyof_wrapper(schemas: &[Schema], variant_name: &str) -> bool {
    schemas.iter().any(|s| s.name == variant_name && matches!(s.kind, SchemaKind::Object { ref fields } if fields.len() == 1 && fields[0].name == "value"))
}

fn wrapper_inner_type<'a>(schemas: &'a [Schema], variant_name: &str) -> Option<&'a TypeRef> {
    schemas
        .iter()
        .find(|s| s.name == variant_name)
        .and_then(|s| match &s.kind {
            SchemaKind::Object { fields } if fields.len() == 1 && fields[0].name == "value" => {
                Some(&fields[0].type_ref)
            }
            _ => None,
        })
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

/// True when this field will be emitted with the `Optional<T?>` wrapper
/// in `emit_field`. This is the predicate that decides whether the
/// enclosing class needs the `flap_utils.dart` import, the private
/// constructor, and the `toJson` override that strips absence sentinels.
///
/// Mirrors the (false, true, primitive) cell of the (required, nullable,
/// supports-wrapper) decision matrix; if `emit_field` ever changes which
/// cells produce the wrapper, this predicate must change in lockstep or
/// the class-level plumbing falls out of sync with the field-level shape.
fn field_uses_optional_wrapper(field: &Field) -> bool {
    !field.required && field.nullable && type_ref_supports_optional_wrapper(&field.type_ref)
}

/// True if any of the supplied fields will be emitted with the
/// `Optional<T?>` wrapper. Drives both the `flap_utils.dart` import and
/// the `toJson` override on the enclosing class.
fn class_has_optional_wrapper_field(fields: &[Field]) -> bool {
    fields.iter().any(field_uses_optional_wrapper)
}

fn emit_freezed_class(
    class_name: &str,
    schema_name: &str,
    fields: &[Field],
    schemas: &[Schema],
    registry: &EnumRegistry,
) -> String {
    let snake = to_snake_case(class_name);
    let has_optional = class_has_optional_wrapper_field(fields);
    let mut out = String::new();

    out.push_str("import 'package:freezed_annotation/freezed_annotation.dart';\n");
    if has_optional {
        out.push_str("import 'flap_utils.dart';\n");
    }

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

    out.push_str(&format!("part '{snake}.freezed.dart';\n"));
    out.push_str(&format!("part '{snake}.g.dart';\n"));
    out.push('\n');

    out.push_str("@freezed\n");
    out.push_str(&format!("class {class_name} with _${class_name} {{\n"));

    // Freezed requires a private constructor `const ClassName._();` to
    // attach custom methods like our `toJson` override. We only emit it
    // when we actually need the override — keeps the output unchanged
    // for specs that never touch nullability.
    if has_optional {
        out.push_str(&format!("  const {class_name}._();\n\n"));
    }

    out.push_str(&format!("  const factory {class_name}({{\n"));
    for field in fields {
        out.push_str(&emit_field(field, schema_name, schemas, registry));
    }
    out.push_str(&format!("  }}) = _{class_name};\n"));
    out.push('\n');
    out.push_str(&format!(
        "  factory {class_name}.fromJson(Map<String, dynamic> json) =>\n"
    ));
    out.push_str(&format!("      _${class_name}FromJson(json);\n"));

    if has_optional {
        // The cast `this as _ClassName` is needed because
        // `_$ClassNameToJson` is generated against the concrete factory
        // target (the `= _ClassName` on the factory), not the public
        // sealed class itself.
        out.push('\n');
        out.push_str("  @override\n");
        out.push_str("  Map<String, dynamic> toJson() =>\n");
        out.push_str(&format!(
            "      stripOptionalAbsent(_${class_name}ToJson(this as _{class_name}));\n"
        ));
    }

    out.push_str("}\n");
    out
}

/// Whether the canonical `OptionalConverter<T>` works for this `T`.
///
/// True for primitives whose JSON shape is `T` itself; false for types
/// that need transformation (DateTime → String, Named → Map, Array →
/// List, etc.). The `false` branch in `emit_field` falls back to `T?`
/// semantics with a TODO so the silent loss of the absent/present-null
/// distinction is at least loud — a future phase will emit per-field
/// `fromJson`/`toJson` lambdas to restore round-trip parity.
fn type_ref_supports_optional_wrapper(type_ref: &TypeRef) -> bool {
    matches!(
        type_ref,
        TypeRef::String | TypeRef::Integer { .. } | TypeRef::Number { .. } | TypeRef::Boolean
    )
}

fn emit_field(
    field: &Field,
    schema_name: &str,
    schemas: &[Schema],
    registry: &EnumRegistry,
) -> String {
    let synth = registry.lookup_field(schema_name, &field.name);
    let dart_type = to_dart_type(&field.type_ref, synth);
    let dart_name = to_camel_case(&field.name);

    // Build up @JsonKey args (name-rewrite + includeIfNull) and any
    // sibling annotations (@OptionalConverter, @Default) separately.
    let mut json_key_args: Vec<String> = Vec::new();
    if dart_name != field.name {
        json_key_args.push(format!("name: '{}'", field.name));
    }
    let mut sibling_annotations: Vec<String> = Vec::new();

    if let TypeRef::Named(name) = &field.type_ref {
        if is_untagged_union(schemas, name) {
            let converter = format!("_{name}Converter");
            sibling_annotations.push(format!("@{converter}()"));
            // Also ensure the import of that file exists (via collect_field_imports)
        }
    }

    // Optional leading comment line (currently only used for the
    // non-primitive Optional<T?> fallback, where we want a visible
    // marker that the absent-vs-null distinction is being lost).
    let mut leading_comment: Option<String> = None;

    // Decide the typed fragment — `T name,`, `T? name,`, or
    // `Optional<T?> name,`.
    let typed_fragment = match (field.required, field.nullable) {
        // required + non-nullable: as before.
        (true, false) => format!("required {dart_type} {dart_name},\n"),

        // required + nullable: explicitly nullable, ALWAYS sent — no
        // includeIfNull suppression, that would lose the `null` wire form.
        (true, true) => format!("required {dart_type}? {dart_name},\n"),

        // optional + non-nullable: `T?` with `includeIfNull: false` so a
        // Dart-side null doesn't leak onto the wire as `"key": null`.
        (false, false) => {
            json_key_args.push("includeIfNull: false".to_string());
            format!("{dart_type}? {dart_name},\n")
        }

        // optional + nullable: the Optional<T?> case — but only if T is
        // a primitive whose JSON shape matches Dart's. For non-primitive
        // T we degrade to (false, false) shape with a TODO; a future
        // phase will emit per-field fromJson/toJson lambdas.
        (false, true) => {
            if type_ref_supports_optional_wrapper(&field.type_ref) {
                sibling_annotations.push("@OptionalConverter()".to_string());
                sibling_annotations.push(format!("@Default(Optional<{dart_type}?>.absent())"));
                format!("Optional<{dart_type}?> {dart_name},\n")
            } else {
                json_key_args.push("includeIfNull: false".to_string());
                leading_comment = Some(format!(
                    "// TODO(flap): nullable+optional non-primitive — \
                     `Optional<{dart_type}?>` not yet supported for this type"
                ));
                format!("{dart_type}? {dart_name},\n")
            }
        }
    };

    // Assemble:
    //     [    // TODO ...                      ]   (optional)
    //     [    @JsonKey(...) ]                      (optional)
    //     [    @OptionalConverter() @Default(...) ] (optional)
    //     <typed fragment>
    let mut out = String::new();
    if let Some(c) = leading_comment {
        out.push_str("    ");
        out.push_str(&c);
        out.push('\n');
    }
    out.push_str("    ");
    if !json_key_args.is_empty() {
        out.push_str(&format!("@JsonKey({}) ", json_key_args.join(", ")));
    }
    for ann in &sibling_annotations {
        out.push_str(ann);
        out.push(' ');
    }
    out.push_str(&typed_fragment);
    out
}

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

pub fn emit_client(api: &Api) -> (String, String) {
    let registry = EnumRegistry::build(api);
    let class_name = api_client_name(&api.title);
    let filename = format!("{}.dart", to_snake_case(&class_name));
    let source = emit_client_source(api, &class_name, &registry);
    (filename, source)
}

fn api_client_name(title: &str) -> String {
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

struct DartCredential<'a> {
    scheme: &'a SecurityScheme,
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

fn emit_constructor(class_name: &str, credentials: &[DartCredential]) -> String {
    let mut out = String::new();

    if credentials.is_empty() {
        out.push_str(&format!("  {class_name}({{required String baseUrl}})\n"));
        out.push_str("      : _dio = Dio(BaseOptions(baseUrl: baseUrl));\n");
        return out;
    }

    out.push_str(&format!("  {class_name}({{\n"));
    out.push_str("    required String baseUrl,\n");
    for cred in credentials {
        out.push_str(&format!("    String? {},\n", cred.dart_param_name));
    }
    out.push_str("  }) : _dio = Dio(BaseOptions(baseUrl: baseUrl)) {\n");

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

fn emit_credential_injection(cred: &DartCredential) -> String {
    let dart = &cred.dart_param_name;
    match cred.scheme {
        SecurityScheme::HttpBasic { .. } => format!(
            "          if ({dart} != null) {{\n            \
     final basic = 'Basic ${{base64Encode(utf8.encode({dart})))}}';\n            \
     options.headers['Authorization'] = basic;\n          }}\n"
        ),
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
        SecurityScheme::OAuth2 { .. } | SecurityScheme::OpenIdConnect { .. } => format!(
            "          if ({dart} != null) {{\n            \
             options.headers['Authorization'] = 'Bearer ${{{dart}}}';\n          }}\n"
        ),
    }
}

// ── Method emission ───────────────────────────────────────────────────────────

struct DartParam<'a> {
    spec_name: &'a str,
    dart_name: String,
    location: ParameterLocation,
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

    if dart_params.is_empty() && op.request_body.is_none() {
        out.push_str(&format!(
            "  Future<{return_type}> {method_name}() async {{\n"
        ));
    } else {
        out.push_str(&format!("  Future<{return_type}> {method_name}({{\n"));

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

    let mut templated_path = op.path.clone();
    for p in dart_params {
        if p.location == ParameterLocation::Path {
            let needle = format!("{{{}}}", p.spec_name);
            let repl = format!("${{{}}}", p.dart_name);
            templated_path = templated_path.replace(&needle, &repl);
        }
    }
    let dart_path_literal = format!("'{templated_path}'");

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

    let data_expr = op.request_body.as_ref().map(body_data_expression);

    let success_schema = success_response_schema(&op.responses);
    let needs_response_var = success_schema.is_some();

    let response_assign = if needs_response_var {
        "    final response = "
    } else {
        "    "
    };
    body.push_str(response_assign);
    body.push_str("await _dio.request<dynamic>(\n");
    body.push_str(&format!("      {dart_path_literal},\n"));

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

    if let Some(schema) = success_schema {
        body.push_str("    final data = response.data;\n");
        let expr = deserialize_expr(schema, schemas, registry, "data");
        body.push_str(&format!("    return {expr};\n"));
    }

    body
}

fn body_data_expression(body: &RequestBody) -> String {
    if body.is_multipart {
        return "FormData.fromMap(body.toJson())".into();
    }
    match &body.schema_ref {
        TypeRef::Named(_) => "body.toJson()".into(),
        _ => "body".into(),
    }
}

fn success_response_schema(responses: &[Response]) -> Option<&TypeRef> {
    success_response(responses).and_then(|r| r.schema_ref.as_ref())
}

fn success_response(responses: &[Response]) -> Option<&Response> {
    responses
        .iter()
        .find(|r| matches!(r.status_code.parse::<u16>(), Ok(c) if (200..300).contains(&c)))
}

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
        TypeRef::Enum(_) => format!("{data_var} as String"),
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
            Some(SchemaKind::Map { value }) => {
                let value_ty = to_dart_type(value, None);
                let inner_expr = deserialize_expr(value, schemas, registry, "v");
                format!(
                    "({data_var} as Map<String, dynamic>).map(\n      \
                     (k, v) => MapEntry(k, {inner_expr}),\n    ).cast<String, {value_ty}>()"
                )
            }
            Some(SchemaKind::UntaggedUnion { .. }) => {
                let cls = dart_class_name(name);
                // Untagged union fromJson accepts `dynamic`, so no cast.
                format!("{cls}.fromJson({data_var})")
            }
            None => {
                let cls = dart_class_name(name);
                format!("{cls}.fromJson({data_var} as Map<String, dynamic>)")
            }
        },
    }
}

fn is_untagged_union(schemas: &[Schema], name: &str) -> bool {
    schemas
        .iter()
        .any(|s| s.name == name && matches!(s.kind, SchemaKind::UntaggedUnion { .. }))
}

fn named_schema_kind<'a>(name: &str, schemas: &'a [Schema]) -> Option<&'a SchemaKind> {
    schemas.iter().find(|s| s.name == name).map(|s| &s.kind)
}

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

fn to_camel_case(s: &str) -> String {
    if !s.contains('_') && !s.contains('-') {
        return s.to_string();
    }
    let parts: Vec<&str> = s.split(['_', '-']).filter(|p| !p.is_empty()).collect();
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

fn to_pascal_case(s: &str) -> String {
    let camel = to_camel_case(s);
    let mut chars = camel.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn to_dart_enum_case(value: &str) -> String {
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

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn fields_of(schema: &Schema) -> Vec<Field> {
        match &schema.kind {
            SchemaKind::Object { fields } => fields
                .iter()
                .map(|f| Field {
                    name: f.name.clone(),
                    type_ref: f.type_ref.clone(),
                    required: f.required,
                    nullable: f.nullable,
                    is_recursive: f.is_recursive,
                })
                .collect(),
            _ => panic!("expected object schema"),
        }
    }

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
                        nullable: false,
                        is_recursive: false,
                    },
                    Field {
                        name: "name".into(),
                        type_ref: TypeRef::String,
                        required: true,
                        nullable: false,
                        is_recursive: false,
                    },
                    Field {
                        name: "tag".into(),
                        type_ref: TypeRef::String,
                        required: false,
                        nullable: false,
                        is_recursive: true,
                    },
                ],
            },
            internal: false,
            extends: None,
        }
    }

    fn pets_schema() -> Schema {
        Schema {
            name: "Pets".into(),
            kind: SchemaKind::Array {
                item: TypeRef::Named("Pet".into()),
            },
            internal: false,
            extends: None,
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
                        nullable: false,
                        is_recursive: true,
                    },
                    Field {
                        name: "message".into(),
                        type_ref: TypeRef::String,
                        required: true,
                        nullable: false,
                        is_recursive: true,
                    },
                ],
            },
            internal: false,
            extends: None,
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

    // ── Existing-shape regression tests ──────────────────────────────────────

    #[test]
    fn pet_emits_freezed_class() {
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Pet", "Pet", &fields_of(&pet_schema()), &[], &registry);
        assert!(src.contains("@freezed"));
        assert!(src.contains("class Pet with _$Pet"));
        assert!(src.contains("required int id"));
        assert!(src.contains("required String name"));
        assert!(src.contains("String? tag"));
        assert!(src.contains("@JsonKey(includeIfNull: false) String? tag"));
        assert!(src.contains(") = _Pet"));
        assert!(src.contains("_$PetFromJson(json)"));
        assert!(src.contains("part 'pet.freezed.dart'"));
        assert!(src.contains("part 'pet.g.dart'"));
    }

    #[test]
    fn pet_does_not_emit_optional_plumbing() {
        // None of Pet's fields are nullable, so no `flap_utils.dart`
        // import, no private constructor, no `toJson` override.
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Pet", "Pet", &fields_of(&pet_schema()), &[], &registry);
        assert!(!src.contains("flap_utils.dart"));
        assert!(!src.contains("const Pet._();"));
        assert!(!src.contains("Map<String, dynamic> toJson()"));
        assert!(!src.contains("stripOptionalAbsent"));
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
                        nullable: false,
                        is_recursive: false,
                    },
                    Field {
                        name: "id".into(),
                        type_ref: TypeRef::String,
                        required: true,
                        nullable: false,
                        is_recursive: false,
                    },
                ],
            },
            internal: false,
            extends: None,
        };
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("User", "User", &fields_of(&schema), &[], &registry);
        assert!(
            src.contains("@JsonKey(name: 'first_name') required String firstName"),
            "missing camelCased property + JsonKey, got:\n{src}"
        );
        assert!(src.contains("required String id,\n"));
    }

    // ── Phase 8: nullability + omission ──────────────────────────────────────

    fn schema_with_field(name: &str, field: Field) -> Schema {
        Schema {
            name: name.into(),
            kind: SchemaKind::Object {
                fields: vec![field],
            },
            internal: false,
            extends: None,

        }
    }

    #[test]
    fn required_nullable_string_keeps_required_keyword_and_question_mark() {
        // Cell (true, true): the receiver always sees the key, but the
        // value may be null.
        let schema = schema_with_field(
            "Profile",
            Field {
                name: "bio".into(),
                type_ref: TypeRef::String,
                required: true,
                nullable: true,
                is_recursive: false,
            },
        );
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Profile", "Profile", &fields_of(&schema), &[], &registry);
        assert!(
            src.contains("required String? bio,"),
            "expected `required String? bio,`, got:\n{src}"
        );
        // Must NOT suppress the null wire form — that would defeat the
        // whole point of (true, true).
        assert!(!src.contains("includeIfNull: false"));
        // (true, true) doesn't trigger the Optional wrapper.
        assert!(!src.contains("Optional<"));
        assert!(!src.contains("flap_utils.dart"));
    }

    #[test]
    fn optional_non_nullable_string_uses_includeifnull_false() {
        // Cell (false, false): client may omit the key, but if present
        // the value is never null. `includeIfNull: false` keeps Dart-side
        // nulls from leaking onto the wire as `"key": null`.
        let schema = schema_with_field(
            "Update",
            Field {
                name: "tag".into(),
                type_ref: TypeRef::String,
                required: false,
                nullable: false,
                is_recursive: false,
            },
        );
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Update", "Update", &fields_of(&schema), &[], &registry);
        assert!(
            src.contains("@JsonKey(includeIfNull: false) String? tag,"),
            "expected includeIfNull: false on optional non-nullable, got:\n{src}"
        );
        // Still no Optional wrapper for this cell.
        assert!(!src.contains("Optional<"));
    }

    #[test]
    fn optional_nullable_primitive_uses_optional_wrapper() {
        // Cell (false, true): the cell that motivates all of this. The
        // wrapper is the only way to encode all three wire forms (absent,
        // present-null, present-value) in a single Dart field.
        let schema = schema_with_field(
            "UpdateUserRequest",
            Field {
                name: "nickname".into(),
                type_ref: TypeRef::String,
                required: false,
                nullable: true,
                is_recursive: false,
            },
        );
        let registry = EnumRegistry::default();
        let src = emit_freezed_class(
            "UpdateUserRequest",
            "UpdateUserRequest",
            &fields_of(&schema),
            &[], // No additional imports needed
            &registry,
        );

        // Field-level shape.
        assert!(
            src.contains("@OptionalConverter() @Default(Optional<String?>.absent()) Optional<String?> nickname,"),
            "expected wrapper + converter + default, got:\n{src}"
        );
        // Class-level plumbing.
        assert!(
            src.contains("import 'flap_utils.dart';"),
            "expected flap_utils.dart import, got:\n{src}"
        );
        assert!(
            src.contains("const UpdateUserRequest._();"),
            "Freezed needs a private constructor to attach toJson, got:\n{src}"
        );
        assert!(
            src.contains("Map<String, dynamic> toJson() =>"),
            "expected toJson override, got:\n{src}"
        );
        assert!(
            src.contains(
                "stripOptionalAbsent(_$UpdateUserRequestToJson(this as _UpdateUserRequest));"
            ),
            "toJson body must strip absence sentinels, got:\n{src}"
        );
    }

    #[test]
    fn optional_nullable_int_uses_wrapper() {
        // Same cell but with `int` — confirms the wrapper extends to
        // every supported primitive, not just String.
        let schema = schema_with_field(
            "Patch",
            Field {
                name: "count".into(),
                type_ref: TypeRef::Integer { format: None },
                required: false,
                nullable: true,
                is_recursive: false,
            },
        );
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Patch", "Patch", &fields_of(&schema), &[], &registry);
        assert!(
            src.contains(
                "@OptionalConverter() @Default(Optional<int?>.absent()) Optional<int?> count,"
            ),
            "expected int wrapper, got:\n{src}"
        );
    }

    #[test]
    fn optional_nullable_named_falls_back_with_todo() {
        // A custom class is non-primitive — the canonical converter's
        // `as T?` cast won't survive (the JSON is a Map, not a `Pet`).
        // We degrade to (false, false) shape with a visible TODO so the
        // round-trip loss is loud rather than silent.
        let schema = schema_with_field(
            "Patch",
            Field {
                name: "owner".into(),
                type_ref: TypeRef::Named("Pet".into()),
                required: false,
                nullable: true,
                is_recursive: false,
            },
        );
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Patch", "Patch", &fields_of(&schema), &[], &registry);
        assert!(
            src.contains("// TODO(flap): nullable+optional non-primitive"),
            "expected fallback TODO comment, got:\n{src}"
        );
        assert!(
            src.contains("@JsonKey(includeIfNull: false) Pet? owner,"),
            "fallback should be the (false, false) shape, got:\n{src}"
        );
        // No Optional plumbing at the class level since the wrapper
        // wasn't used.
        assert!(!src.contains("flap_utils.dart"));
        assert!(!src.contains("const Patch._();"));
    }

    #[test]
    fn snake_case_field_with_optional_wrapper_combines_jsonkey_and_converter() {
        // Both annotations must coexist: @JsonKey carries the wire-side
        // name rewrite, @OptionalConverter and @Default sit alongside.
        let schema = schema_with_field(
            "Patch",
            Field {
                name: "display_name".into(),
                type_ref: TypeRef::String,
                required: false,
                nullable: true,
                is_recursive: false,
            },
        );
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("Patch", "Patch", &fields_of(&schema), &[], &registry);
        assert!(
            src.contains("@JsonKey(name: 'display_name') @OptionalConverter() @Default(Optional<String?>.absent()) Optional<String?> displayName,"),
            "expected JsonKey + converter + default + camelCased dart name, got:\n{src}"
        );
    }

    #[test]
    fn flap_utils_is_emitted_unconditionally() {
        // Even a spec without any nullable+optional fields gets the
        // utility file. Stable file set is more important than minimising
        // emission.
        let api = Api {
            title: "T".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![pet_schema()],
            security_schemes: vec![],
            security: vec![],
        };
        let files = emit_models(&api);
        assert!(files.contains_key("flap_utils.dart"));
        let utils = &files["flap_utils.dart"];
        assert!(utils.contains("sealed class Optional<T>"));
        assert!(utils.contains("class OptionalConverter<T>"));
        assert!(utils.contains("kOptionalAbsentSentinel"));
        assert!(utils.contains("stripOptionalAbsent"));
    }

    #[test]
    fn flap_utils_runtime_is_self_contained() {
        // The utility file must be drop-in: imports only the public
        // freezed_annotation package, no project-internal refs.
        let api = Api {
            title: "T".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![],
            security_schemes: vec![],
            security: vec![],
        };
        let files = emit_models(&api);
        let utils = &files["flap_utils.dart"];
        assert!(utils.contains("import 'package:freezed_annotation/freezed_annotation.dart';"));
        assert!(!utils.contains("import 'pet"));
        assert!(!utils.contains("import 'flap_utils"));
    }

    #[test]
    fn class_with_mixed_fields_only_adds_optional_plumbing_for_wrapper_cell() {
        // Realistic shape: a PATCH body with a required field, an
        // optional non-nullable field, and an optional nullable field.
        // Only the third cell triggers the wrapper, but the wrapper is
        // enough by itself to add the class-level plumbing.
        let schema = Schema {
            name: "PatchUser".into(),
            extends: None,
            kind: SchemaKind::Object {
                fields: vec![
                    Field {
                        name: "id".into(),
                        type_ref: TypeRef::String,
                        required: true,
                        nullable: false,
                        is_recursive: false,
                    },
                    Field {
                        name: "tag".into(),
                        type_ref: TypeRef::String,
                        required: false,
                        nullable: false,
                        is_recursive: false,
                    },
                    Field {
                        name: "nickname".into(),
                        type_ref: TypeRef::String,
                        required: false,
                        nullable: true,
                        is_recursive: false,
                    },
                ],
            },
            internal: false,
        };
        let registry = EnumRegistry::default();
        let src = emit_freezed_class(
            "PatchUser",
            "PatchUser",
            &fields_of(&schema),
            &[],
            &registry,
        );

        // Required, non-nullable: untouched.
        assert!(src.contains("required String id,"));
        // Optional non-nullable: includeIfNull suppression only.
        assert!(src.contains("@JsonKey(includeIfNull: false) String? tag,"));
        // Optional nullable: full wrapper.
        assert!(src.contains("Optional<String?> nickname,"));
        // Class-level plumbing (driven by the wrapper field).
        assert!(src.contains("import 'flap_utils.dart';"));
        assert!(src.contains("const PatchUser._();"));
        assert!(src.contains("stripOptionalAbsent(_$PatchUserToJson(this as _PatchUser));"));
    }

    #[test]
    fn d7_renamed_class_uses_renamed_in_tojson_cast() {
        // `Error` → `ErrorModel` per D7. The toJson cast must use the
        // renamed class; if it didn't, the generated `_$ErrorModelToJson`
        // would refuse the bare `Error` and the file wouldn't compile.
        let schema = Schema {
            name: "Error".into(),
            kind: SchemaKind::Object {
                fields: vec![Field {
                    name: "detail".into(),
                    type_ref: TypeRef::String,
                    required: false,
                    nullable: true,
                    is_recursive: false,
                }],
            },
            internal: false,
            extends: None,

        };
        let registry = EnumRegistry::default();
        let src = emit_freezed_class("ErrorModel", "Error", &fields_of(&schema), &[], &registry);
        assert!(
            src.contains("stripOptionalAbsent(_$ErrorModelToJson(this as _ErrorModel));"),
            "toJson cast must use the D7-renamed class, got:\n{src}"
        );
    }

    #[test]
    fn emit_anyof_union() {
        let union_name = "ExampleMixed";
        // No wrapper schemas needed; just the union schema itself.
        let union_schema = Schema {
            name: union_name.into(),
            kind: SchemaKind::UntaggedUnion {
                variants: vec![
                    TypeRef::Named("__internal_wrapper_string".into()), // dummy; we'll mock internal detection
                    TypeRef::Named("__internal_wrapper_int".into()),
                ],
            },
            internal: false,
            extends: None,

        };

        // We need the schemas to resolve internal wrappers; we'll add them as internal schemas.
        let wrapper_string = Schema {
            name: "__internal_wrapper_string".into(),
            kind: SchemaKind::Object {
                fields: vec![Field {
                    name: "value".into(),
                    type_ref: TypeRef::String,
                    required: true,
                    nullable: false,
                    is_recursive: false,
                }],
            },
            internal: true,
            extends: None,

        };
        let wrapper_int = Schema {
            name: "__internal_wrapper_int".into(),
            kind: SchemaKind::Object {
                fields: vec![Field {
                    name: "value".into(),
                    type_ref: TypeRef::Integer { format: None },
                    required: true,
                    nullable: false,
                    is_recursive: false,
                }],
            },
            internal: true,
            extends: None,

        };

        let api = Api {
            title: "Test".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![union_schema, wrapper_string, wrapper_int],
            security_schemes: vec![],
            security: vec![],
        };

        let files = emit_models(&api);
        // wrapper schemas should NOT be emitted
        assert!(!files.contains_key("__internal_wrapper_string.dart"));
        assert!(!files.contains_key("__internal_wrapper_int.dart"));

        let union_file = files.get("example_mixed.dart").expect("union file missing");
        assert!(union_file.contains("sealed class ExampleMixed {"));
        assert!(union_file.contains("factory ExampleMixed.fromJson(dynamic json)"));
        assert!(union_file.contains(
            "class _ExampleMixedConverter implements JsonConverter<ExampleMixed, Object?>"
        ));
        assert!(union_file.contains("if (json is String) return ExampleMixed.variant0(json);"));
        assert!(union_file.contains("if (json is int) return ExampleMixed.variant1(json);"));
        // The class should not have any Freezed annotations.
        assert!(!union_file.contains("@Freezed"));
        assert!(!union_file.contains("part '"));
    }

    #[test]
    fn untagged_union_field_gets_converter_annotation() {
        let union_name = "Mixed";
        let union_schema = Schema {
            name: union_name.into(),
            kind: SchemaKind::UntaggedUnion {
                variants: vec![TypeRef::Named("__internal_wrapper_string".into())],
            },
            internal: false,
            extends: None,

        };
        let wrapper = Schema {
            name: "__internal_wrapper_string".into(),
            kind: SchemaKind::Object {
                fields: vec![Field {
                    name: "value".into(),
                    type_ref: TypeRef::String,
                    required: true,
                    nullable: false,
                    is_recursive: false,
                }],
            },
            internal: true,
            extends: None,

        };
        let container = Schema {
            name: "Container".into(),
            kind: SchemaKind::Object {
                fields: vec![Field {
                    name: "data".into(),
                    type_ref: TypeRef::Named(union_name.into()),
                    required: true,
                    nullable: false,
                    is_recursive: false,
                }],
            },
            internal: false,
            extends: None,

        };
        let api = Api {
            title: "Test".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![container, union_schema, wrapper],
            security_schemes: vec![],
            security: vec![],
        };
        let files = emit_models(&api);
        let container_file = files.get("container.dart").unwrap();
        assert!(
            container_file.contains("@_MixedConverter()"),
            "Container.data should have the converter annotation"
        );
        assert!(
            container_file.contains("Mixed data,"),
            "field should be typed as the union class"
        );
    }

    // ── Existing client tests, abridged to confirm no regressions ────────────

    #[test]
    fn list_pets_signature_unchanged() {
        let (_, src) = emit_client(&petstore_api());
        assert!(src.contains("Future<Pets> listPets({"));
        assert!(src.contains("int? limit,"));
    }

    #[test]
    fn show_pet_body_path_templating_unchanged() {
        let (_, src) = emit_client(&petstore_api());
        assert!(src.contains("'/pets/${petId}',"));
        assert!(src.contains("return Pet.fromJson(data as Map<String, dynamic>);"));
    }

    // ── OAuth2 / OpenID Connect emission ─────────────────────────────────────

    fn oauth2_api() -> Api {
        use flap_ir::{OAuth2Flow, OAuth2FlowType};
        Api {
            title: "OAuth2 Petstore".into(),
            base_url: Some("https://api.example.com/v1".into()),
            operations: vec![],
            schemas: vec![],
            security_schemes: vec![SecurityScheme::OAuth2 {
                scheme_name: "oauth2Auth".into(),
                flows: vec![
                    OAuth2Flow {
                        flow_type: OAuth2FlowType::ClientCredentials,
                        token_url: Some("https://auth.example.com/token".into()),
                        authorization_url: None,
                        scopes: vec!["read:pets".into()],
                    },
                    OAuth2Flow {
                        flow_type: OAuth2FlowType::AuthorizationCode,
                        token_url: Some("https://auth.example.com/token".into()),
                        authorization_url: Some("https://auth.example.com/authorize".into()),
                        scopes: vec!["read:pets".into()],
                    },
                ],
            }],
            security: vec!["oauth2Auth".into()],
        }
    }

    fn oidc_api() -> Api {
        Api {
            title: "OIDC Petstore".into(),
            base_url: Some("https://api.example.com/v1".into()),
            operations: vec![],
            schemas: vec![],
            security_schemes: vec![SecurityScheme::OpenIdConnect {
                scheme_name: "oidcAuth".into(),
                openid_connect_url: "https://auth.example.com/.well-known/openid-configuration"
                    .into(),
            }],
            security: vec!["oidcAuth".into()],
        }
    }

    #[test]
    fn oauth2_emits_access_token_constructor_param() {
        let (_, src) = emit_client(&oauth2_api());
        assert!(
            src.contains("String? oauth2Auth,"),
            "expected oauth2Auth param, got:\n{src}"
        );
    }

    #[test]
    fn oauth2_emits_bearer_interceptor_injection() {
        let (_, src) = emit_client(&oauth2_api());
        assert!(
            src.contains("options.headers['Authorization'] = 'Bearer ${oauth2Auth}'"),
            "expected Bearer injection, got:\n{src}"
        );
    }

    #[test]
    fn oidc_emits_access_token_constructor_param() {
        let (_, src) = emit_client(&oidc_api());
        assert!(
            src.contains("String? oidcAuth,"),
            "expected oidcAuth param, got:\n{src}"
        );
    }

    #[test]
    fn oidc_emits_bearer_interceptor_injection() {
        let (_, src) = emit_client(&oidc_api());
        assert!(
            src.contains("options.headers['Authorization'] = 'Bearer ${oidcAuth}'"),
            "expected Bearer injection, got:\n{src}"
        );
    }

    #[test]
    fn oauth2_and_bearer_can_coexist_in_constructor() {
        use flap_ir::{OAuth2Flow, OAuth2FlowType};
        let api = Api {
            title: "Mixed".into(),
            base_url: None,
            operations: vec![],
            schemas: vec![],
            security_schemes: vec![
                SecurityScheme::HttpBearer {
                    scheme_name: "bearerAuth".into(),
                    bearer_format: Some("JWT".into()),
                },
                SecurityScheme::OAuth2 {
                    scheme_name: "oauth2Auth".into(),
                    flows: vec![OAuth2Flow {
                        flow_type: OAuth2FlowType::ClientCredentials,
                        token_url: Some("https://auth.example.com/token".into()),
                        authorization_url: None,
                        scopes: vec![],
                    }],
                },
            ],
            security: vec!["bearerAuth".into(), "oauth2Auth".into()],
        };
        let (_, src) = emit_client(&api);
        assert!(src.contains("String? bearerAuth,"));
        assert!(src.contains("String? oauth2Auth,"));
    }

    // #[test]
    // fn openapi_31_field_emits_optional_wrapper() {
    //     let api = load_str("... same YAML ...").unwrap();
    //     let registry = EnumRegistry::build(&api);
    //     // Pick the schema
    //     let schema = api.schemas.iter().find(|s| s.name == "Update").unwrap();
    //     let fields = match &schema.kind {
    //         SchemaKind::Object { fields } => fields,
    //         _ => panic!(),
    //     };
    //     let src = emit_freezed_class("Update", "Update", &fields, &registry);
    //     assert!(
    //         src.contains("Optional<String?> nickname,"),
    //         "3.1 nullable field should emit Optional<String?> wrapper, got:\n{src}"
    //     );
    //     // Must include the utils import and toJson override
    //     assert!(src.contains("import 'flap_utils.dart';"));
    //     assert!(src.contains("stripOptionalAbsent"));
    // }

    // #[test]
    // fn petstore_30_still_works() {
    //     let yaml = include_str!("../../../tests/fixtures/petstore.yaml");
    //     let api = load_str(yaml).expect("3.0 petstore should still parse");
    //     // Check a field that should not be nullable
    //     let pet = api.schemas.iter().find(|s| s.name == "Pet").unwrap();
    //     let SchemaKind::Object { fields } = &pet.kind else {
    //         panic!()
    //     };
    //     let name = fields.iter().find(|f| f.name == "name").unwrap();
    //     assert!(!name.nullable);
    //     // Also check the emitter output signature
    //     let registry = EnumRegistry::build(&api);
    //     let src = emit_freezed_class("Pet", "Pet", &fields, &registry);
    //     assert!(!src.contains("Optional<")); // 3.0 Pet has no Optional fields
    // }
}

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

/// Controls whether the emitted Dart code targets sound null safety (Dart ≥ 2.12)
/// or the legacy null-unsafe dialect (Dart < 2.12).
///
/// The two modes share every lowering and IR pass — only the Dart surface
/// syntax differs. Specifically:
///
/// | concern                  | Safe                  | Unsafe                    |
/// |--------------------------|-----------------------|---------------------------|
/// | optional type suffix     | `T?`                  | `T`                       |
/// | required param keyword   | `required T name`     | `@required T name`        |
/// | absent+nullable field    | `Optional<T?>`        | `T` (always nullable)     |
/// | response-header return   | Dart 3 record `({…})` | body-only (records n/a)   |
/// | flap_utils.dart          | emitted               | not emitted               |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullSafety {
    Safe,
    Unsafe,
}

// ── Synthetic enum registry ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SynthEnum {
    name: String,
    values: Vec<flap_ir::EnumValue>,
}

#[derive(Debug, Default)]
struct EnumRegistry {
    field_enums: HashMap<(String, String), String>,
    param_enums: HashMap<(String, String), String>,
    body_enums: HashMap<String, String>, // op_id → synth name
    response_enums: HashMap<(String, String), String>, // (op_id, status_code) → synth name
    enums: BTreeMap<String, SynthEnum>,
}

impl EnumRegistry {
    fn build(api: &Api) -> Self {
        let mut reg = Self::default();

        // Schema field enums.
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

            // Parameter enums.
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

            // Request body inline enum.
            if let Some(body) = &op.request_body {
                if let TypeRef::Enum(values) = &body.schema_ref {
                    let synth = format!("{op_pascal}Body");
                    reg.body_enums.insert(op_id.clone(), synth.clone());
                    reg.enums.insert(
                        synth.clone(),
                        SynthEnum {
                            name: synth,
                            values: values.clone(),
                        },
                    );
                }
            }

            // Response inline enums.
            for resp in &op.responses {
                if let Some(TypeRef::Enum(values)) = &resp.schema_ref {
                    let code = resp
                        .status_code
                        .chars()
                        .filter(|c| c.is_alphanumeric())
                        .collect::<String>();
                    let synth = format!("{op_pascal}{code}Response");
                    reg.response_enums
                        .insert((op_id.clone(), resp.status_code.clone()), synth.clone());
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

    fn lookup_body(&self, op_id: &str) -> Option<&str> {
        self.body_enums.get(op_id).map(String::as_str)
    }

    fn lookup_response(&self, op_id: &str, status_code: &str) -> Option<&str> {
        self.response_enums
            .get(&(op_id.to_string(), status_code.to_string()))
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
pub fn emit_models(api: &Api, mode: NullSafety) -> HashMap<String, String> {
    let registry = EnumRegistry::build(api);
    let mut files = HashMap::new();

    if mode == NullSafety::Safe {
        files.insert("flap_utils.dart".to_string(), emit_flap_utils());
    }

    for schema in &api.schemas {
        if schema.internal {
            continue;
        }
        let class_name = dart_class_name(&schema.name);
        let filename = format!("{}.dart", to_snake_case(&class_name));
        let source = emit_schema(schema, &class_name, &registry, &api.schemas, mode);
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
    mode: NullSafety,
) -> String {
    match &schema.kind {
        SchemaKind::Object { fields } => {
            emit_freezed_class(class_name, &schema.name, fields, schemas, registry, mode)
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
            mode,
        ),
        SchemaKind::UntaggedUnion { variants } => {
            emit_untagged_union(class_name, &schema.name, variants, schemas, registry, mode)
        }
        SchemaKind::Alias { target } => emit_alias_typedef(class_name, target),
    }
}

// ── Alias → typedef ───────────────────────────────────────────────────────────

fn emit_alias_typedef(alias_name: &str, target: &str) -> String {
    let target_cls = dart_class_name(target);
    let target_file = to_snake_case(&target_cls);
    format!(
        "// Generated from OpenAPI $ref alias `{alias_name}` → `{target}`.\n\
         import '{target_file}.dart';\n\
         typedef {alias_name} = {target_cls};\n"
    )
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
    mode: NullSafety,
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
                if field_uses_optional_wrapper(field, mode) {
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
            out.push_str(&emit_field(field, variant_name, schemas, registry, mode));
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
    mode: NullSafety,
) -> String {
    let snake = to_snake_case(class_name);
    let mut out = String::new();

    out.push_str("import 'dart:convert';\n");
    out.push_str("import 'package:flutter/foundation.dart';\n");

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

    out.push_str(&format!("sealed class {class_name} {{\n"));
    out.push_str(&format!("  const {class_name}._();\n\n"));

    for (i, variant) in variants.iter().enumerate() {
        let (variant_dart_type, _, _) = resolve_untagged_variant_info(variant, schemas, registry);
        let factory_name = format!("variant{}", i);
        let formal = format!("{variant_dart_type} value");
        out.push_str(&format!(
            "  const factory {class_name}.{factory_name}({formal}) = _Variant{i};\n"
        ));
    }

    out.push_str(&format!(
        "\n  factory {class_name}.fromJson(dynamic json) {{\n"
    ));

    for (i, variant) in variants.iter().enumerate() {
        let (variant_dart_type, _, is_primitive) =
            resolve_untagged_variant_info(variant, schemas, registry);
        let factory_name = format!("variant{}", i);

        if is_primitive {
            out.push_str(&format!(
                "    if (json is {variant_dart_type}) return {class_name}.{factory_name}(json);\n"
            ));
        } else {
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

    // Set up the mode-dependent nullability binding here
    let obj_nullable = if mode == NullSafety::Safe {
        "Object?"
    } else {
        "Object"
    };
    out.push_str(&format!("  {obj_nullable} toJson();\n"));
    out.push_str(&format!("}}\n\n"));

    for (i, variant) in variants.iter().enumerate() {
        let (variant_dart_type, _, _) = resolve_untagged_variant_info(variant, schemas, registry);

        out.push_str(&format!("class _Variant{i} extends {class_name} {{\n"));
        out.push_str(&format!("  final {variant_dart_type} value;\n"));
        out.push_str(&format!("  const _Variant{i}(this.value) : super._();\n\n"));

        // Use the mode-dependent binding for both primitive and nested object types
        if is_variant_primitive(variant, schemas) {
            out.push_str(&format!("  @override\n"));
            out.push_str(&format!("  {obj_nullable} toJson() => value;\n"));
        } else {
            out.push_str(&format!("  @override\n"));
            out.push_str(&format!("  {obj_nullable} toJson() => value.toJson();\n"));
        }

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

    // Apply the binding to the converter generic args and method signatures
    let converter_name = format!("_{class_name}Converter");
    out.push_str(&format!(
        "class {converter_name} implements JsonConverter<{class_name}, {obj_nullable}> {{\n"
    ));
    out.push_str(&format!("  const {converter_name}();\n\n"));
    out.push_str(&format!(
        "  @override\n  {class_name} fromJson({obj_nullable} json) =>\n      {class_name}.fromJson(json);\n\n"
    ));
    out.push_str(&format!(
        "  @override\n  {obj_nullable} toJson({class_name} object) => object.toJson();\n"
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
            if let Some(wrapper_schema) = schemas.iter().find(|s| s.name == *name)
                && wrapper_schema.internal
            {
                if let SchemaKind::Object { fields } = &wrapper_schema.kind
                    && fields.len() == 1
                    && fields[0].name == "value"
                {
                    let inner_type = &fields[0].type_ref;
                    let dart_inner = to_dart_type(inner_type, None);
                    return (dart_inner, "value".to_string(), true);
                }
                panic!("internal wrapper schema without a single 'value' field");
            }
            let cls = dart_class_name(name);
            (cls, "value".to_string(), false)
        }
        _ => panic!("unexpected TypeRef in untagged union variant"),
    }
}

fn is_internal_wrapper(schemas: &[Schema], variant_name: &str) -> bool {
    schemas.iter().any(|s| s.name == variant_name && s.internal)
}

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

fn field_uses_optional_wrapper(field: &Field, mode: NullSafety) -> bool {
    mode == NullSafety::Safe
        && !field.required
        && field.nullable
        && type_ref_supports_optional_wrapper(&field.type_ref)
}

fn class_has_optional_wrapper_field(fields: &[Field], mode: NullSafety) -> bool {
    fields.iter().any(|f| field_uses_optional_wrapper(f, mode))
}

fn emit_freezed_class(
    class_name: &str,
    schema_name: &str,
    fields: &[Field],
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
) -> String {
    let snake = to_snake_case(class_name);
    let has_optional = class_has_optional_wrapper_field(fields, mode);
    let mut out = String::new();

    out.push_str("import 'package:freezed_annotation/freezed_annotation.dart';\n");
    if mode == NullSafety::Unsafe {
        out.push_str("import 'package:meta/meta.dart';\n");
    }
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

    if has_optional {
        out.push_str(&format!("  const {class_name}._();\n\n"));
    }

    out.push_str(&format!("  const factory {class_name}({{\n"));
    for field in fields {
        out.push_str(&emit_field(field, schema_name, schemas, registry, mode));
    }
    out.push_str(&format!("  }}) = _{class_name};\n"));
    out.push('\n');
    out.push_str(&format!(
        "  factory {class_name}.fromJson(Map<String, dynamic> json) =>\n"
    ));
    out.push_str(&format!("      _${class_name}FromJson(json);\n"));

    if has_optional {
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
    mode: NullSafety, // ← new
) -> String {
    let synth = registry.lookup_field(schema_name, &field.name);
    let dart_type = to_dart_type(&field.type_ref, synth);
    let dart_name = to_camel_case(&field.name);
    let force_nullable_for_recursion =
        field.is_recursive && matches!(&field.type_ref, TypeRef::Named(_));

    let mut json_key_args: Vec<String> = Vec::new();
    if dart_name != field.name {
        json_key_args.push(format!("name: '{}'", field.name));
    }
    let mut sibling_annotations: Vec<String> = Vec::new();

    if let TypeRef::Named(name) = &field.type_ref
        && is_untagged_union(schemas, name)
    {
        sibling_annotations.push(format!("@_{name}Converter()"));
    }

    let mut leading_comment: Option<String> = None;

    let typed_fragment = match mode {
        // ── Null-unsafe: no `?`, no `required` keyword, `@required` annotation instead ──
        NullSafety::Unsafe => {
            // @required marks fields the caller must supply; everything is
            // technically nullable at the Dart type level, so we don't append `?`.
            let is_req = field.required && !force_nullable_for_recursion;
            if is_req {
                sibling_annotations.insert(0, "@required".to_string());
            } else {
                json_key_args.push("includeIfNull: false".to_string());
            }
            if let Some(default) = &field.default_value {
                let default_expr = dart_default_expr(default);
                sibling_annotations.push(format!("@Default({default_expr})"));
            }
            format!("{dart_type} {dart_name},\n")
        }

        // ── Null-safe: existing logic, unchanged ──────────────────────────────
        NullSafety::Safe => {
            if force_nullable_for_recursion {
                if field.required {
                    format!("required {dart_type}? {dart_name},\n")
                } else {
                    json_key_args.push("includeIfNull: false".to_string());
                    format!("{dart_type}? {dart_name},\n")
                }
            } else {
                match (field.required, field.nullable) {
                    (true, false) => format!("required {dart_type} {dart_name},\n"),
                    (true, true) => format!("required {dart_type}? {dart_name},\n"),
                    (false, false) => {
                        json_key_args.push("includeIfNull: false".to_string());
                        if let Some(default) = &field.default_value {
                            let default_expr = dart_default_expr(default);
                            sibling_annotations.push(format!("@Default({default_expr})"));
                            format!("{dart_type} {dart_name},\n")
                        } else {
                            format!("{dart_type}? {dart_name},\n")
                        }
                    }
                    (false, true) => {
                        if type_ref_supports_optional_wrapper(&field.type_ref) {
                            sibling_annotations.push("@OptionalConverter()".to_string());
                            sibling_annotations
                                .push(format!("@Default(Optional<{dart_type}?>.absent())"));
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
                }
            }
        }
    };

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
    // unknownValue routes any unrecognised wire value to the `unknown` sentinel
    // instead of throwing, making the client resilient to new server-side enum
    // additions that haven't been deployed to the client yet.
    out.push_str(&format!(
        "@JsonEnum(unknownValue: {}.unknown)\n",
        synth.name
    ));
    out.push_str(&format!("enum {} {{\n", synth.name));
    for value in &synth.values {
        let (dart_case, json_annotation) = match value {
            flap_ir::EnumValue::Str(s) => {
                let escaped = s.replace('\'', "\\'");
                (to_dart_enum_case(s), format!("@JsonValue('{escaped}')"))
            }
            flap_ir::EnumValue::Int(n) => {
                // Prefix with 'v' so the Dart identifier never starts with a digit.
                (format!("v{n}"), format!("@JsonValue({n})"))
            }
        };
        out.push_str(&format!("  {json_annotation}\n"));
        out.push_str(&format!("  {dart_case},\n"));
    }
    out.push_str("  @JsonValue(null)\n");
    out.push_str("  unknown;\n");
    out.push_str("}\n");
    out
}

// ── Public entry point: client ────────────────────────────────────────────────

pub fn emit_client(api: &Api, mode: NullSafety) -> (String, String) {
    let registry = EnumRegistry::build(api);
    let class_name = api_client_name(&api.title);
    let filename = format!("{}.dart", to_snake_case(&class_name));
    let source = emit_client_source(api, &class_name, &registry, mode);
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

fn emit_client_source(
    api: &Api,
    class_name: &str,
    registry: &EnumRegistry,
    mode: NullSafety,
) -> String {
    let mut out = String::new();

    // dart:convert needed for jsonEncode (inline enum bodies).
    out.push_str("import 'dart:convert';\n");
    out.push_str("import 'package:dio/dio.dart';\n");
    if mode == NullSafety::Unsafe {
        out.push_str("import 'package:meta/meta.dart';\n");
    }
    out.push('\n');

    // When the spec declares multiple servers, emit named constants so
    // callers can switch between them without hard-coding URLs.
    if api.base_urls.len() > 1 {
        out.push_str(&format!("abstract final class {class_name}Urls {{\n"));
        for (i, url) in api.base_urls.iter().enumerate() {
            out.push_str(&format!("  static const String server{i} = '{url}';\n"));
        }
        out.push_str("}\n\n");
    }

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
    out.push_str(&emit_constructor(class_name, &credentials, &api.base_urls));
    out.push('\n');
    out.push_str("  final Dio _dio;\n");

    for op in &api.operations {
        out.push('\n');
        out.push_str(&emit_method(op, &api.schemas, registry, mode));
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

fn emit_constructor(
    class_name: &str,
    credentials: &[DartCredential],
    base_urls: &[String],
) -> String {
    let default_url = base_urls
        .first()
        .map(|u| format!("'{u}'"))
        .unwrap_or_else(|| "''".to_string());

    let mut out = String::new();

    if credentials.is_empty() {
        out.push_str(&format!("  {class_name}({{\n"));
        out.push_str(&format!("    String baseUrl = {default_url},\n"));
        out.push_str("  }}) : _dio = Dio(BaseOptions(baseUrl: baseUrl));\n");
        return out;
    }

    out.push_str(&format!("  {class_name}({{\n"));
    out.push_str(&format!("    String baseUrl = {default_url},\n"));
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

fn emit_method(
    op: &Operation,
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
) -> String {
    let mut out = String::new();

    if let Some(summary) = &op.summary {
        out.push_str(&format!("  /// {summary}\n"));
    }
    out.push_str(&format!("  // {} {}\n", op.method, op.path));

    let method_name = op_method_name(op);
    let dart_params = build_dart_params(op, registry);
    let return_type = success_return_type(&op.responses, mode);

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
            let op_id = op.operation_id.as_deref().unwrap_or("");
            let body_synth = registry.lookup_body(op_id);
            let body_type = to_dart_type(&body.schema_ref, body_synth);
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

    out.push_str(&emit_method_body(op, &dart_params, schemas, registry, mode));
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

fn success_return_type(responses: &[Response], mode: NullSafety) -> String {
    let Some(resp) = success_response(responses) else {
        return "void".into();
    };

    if mode == NullSafety::Unsafe || resp.headers.is_empty() {
        return match &resp.schema_ref {
            Some(t) => to_dart_type(t, None),
            None => "void".into(),
        };
    }

    let body_type = match &resp.schema_ref {
        Some(t) => to_dart_type(t, None),
        None => "void".into(),
    };

    let mut fields: Vec<String> = Vec::new();
    if resp.schema_ref.is_some() {
        fields.push(format!("{body_type} body"));
    }
    for hdr in &resp.headers {
        let dart_type = to_dart_type(&hdr.type_ref, None);
        let dart_name = to_camel_case(&hdr.name.replace('-', "_"));
        if hdr.required {
            fields.push(format!("{dart_type} {dart_name}"));
        } else {
            fields.push(format!("{dart_type}? {dart_name}"));
        }
    }
    format!("({{{}}})", fields.join(", "))
}

fn emit_success_return(
    resp: &Response,
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
) -> String {
    // In unsafe mode, records don't exist — return body only, skip headers.
    if mode == NullSafety::Unsafe {
        if let Some(schema) = &resp.schema_ref {
            let expr = deserialize_expr(schema, schemas, registry, "response.data");
            return format!("    return {expr};\n");
        }
        return String::new();
    }

    let has_body = resp.schema_ref.is_some();
    let has_headers = !resp.headers.is_empty();

    if !has_headers {
        if let Some(schema) = &resp.schema_ref {
            let expr = deserialize_expr(schema, schemas, registry, "response.data");
            return format!("    return {expr};\n");
        }
        return String::new();
    }

    let mut out = String::new();

    for hdr in &resp.headers {
        let dart_name = to_camel_case(&hdr.name.replace('-', "_"));
        let raw_expr = format!("response.headers.value('{}')", hdr.name.to_lowercase());
        let typed_expr = header_deserialize_expr(&hdr.type_ref, &raw_expr);
        if hdr.required {
            out.push_str(&format!("    final {dart_name} = {typed_expr};\n"));
        } else {
            out.push_str(&format!("    final {dart_name}Raw = {raw_expr};\n"));
            let null_guarded =
                header_deserialize_expr_nullable(&hdr.type_ref, &format!("{dart_name}Raw"));
            out.push_str(&format!("    final {dart_name} = {null_guarded};\n"));
        }
    }

    let mut record_fields: Vec<String> = Vec::new();
    if has_body {
        let body_expr = resp
            .schema_ref
            .as_ref()
            .map(|s| deserialize_expr(s, schemas, registry, "response.data"))
            .unwrap_or_default();
        record_fields.push(format!("body: {body_expr}"));
    }
    for hdr in &resp.headers {
        let dart_name = to_camel_case(&hdr.name.replace('-', "_"));
        record_fields.push(format!("{dart_name}: {dart_name}"));
    }
    out.push_str(&format!("    return ({});\n", record_fields.join(", ")));
    out
}

fn header_deserialize_expr(type_ref: &TypeRef, raw: &str) -> String {
    match type_ref {
        TypeRef::String | TypeRef::DateTime => raw.to_string(),
        TypeRef::Integer { .. } => format!("int.parse({raw})"),
        TypeRef::Number { .. } => format!("num.parse({raw})"),
        TypeRef::Boolean => format!("({raw} == 'true')"),
        TypeRef::Array(inner) => {
            let item_expr = header_deserialize_expr(inner, "e");
            format!("{raw}.split(',').map((e) => {item_expr}).toList()")
        }
        _ => raw.to_string(),
    }
}

fn header_deserialize_expr_nullable(type_ref: &TypeRef, raw_var: &str) -> String {
    match type_ref {
        TypeRef::String | TypeRef::DateTime => raw_var.to_string(),
        TypeRef::Integer { .. } => {
            format!("{raw_var} != null ? int.parse({raw_var}) : null")
        }
        TypeRef::Number { .. } => {
            format!("{raw_var} != null ? num.parse({raw_var}) : null")
        }
        TypeRef::Boolean => {
            format!("{raw_var} != null ? ({raw_var} == 'true') : null")
        }
        TypeRef::Array(inner) => {
            let item_expr = header_deserialize_expr(inner, "e");
            format!(
                "{raw_var} != null \
                 ? {raw_var}!.split(',').map((e) => {item_expr}).toList() \
                 : null"
            )
        }
        _ => raw_var.to_string(),
    }
}

fn emit_method_body(
    op: &Operation,
    dart_params: &[DartParam],
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
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

    let needs_response_var = success_response(&op.responses)
        .map(|r| r.schema_ref.is_some() || !r.headers.is_empty())
        .unwrap_or(false);

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

    if let Some(resp) = success_response(&op.responses) {
        body.push_str(&emit_success_return(resp, schemas, registry, mode));
    }

    body
}

fn body_data_expression(body: &RequestBody) -> String {
    if !body.is_multipart {
        return match &body.schema_ref {
            TypeRef::Named(_) => "body.toJson()".into(),
            // json_serializable can encode an annotated enum directly via jsonEncode.
            TypeRef::Enum(_) => "jsonEncode(body)".into(),
            _ => "body".into(),
        };
    }

    match &body.schema_ref {
        TypeRef::Named(_) => "FormData.fromMap(body.toJson())".into(),
        TypeRef::Map(_) => "FormData.fromMap(body)".into(),
        TypeRef::Array(_) => "FormData.fromMap({'file': body})".into(),
        TypeRef::DateTime => "FormData.fromMap({'data': body.toIso8601String()})".into(),
        TypeRef::Enum(_) => "FormData.fromMap({'data': jsonEncode(body)})".into(),
        TypeRef::String | TypeRef::Integer { .. } | TypeRef::Number { .. } | TypeRef::Boolean => {
            "FormData.fromMap({'data': body})".into()
        }
    }
}

fn success_response(responses: &[Response]) -> Option<&Response> {
    responses
        .iter()
        .find(|r| matches!(r.status_code.parse::<u16>(), Ok(c) if (200..300).contains(&c)))
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
                format!("{cls}.fromJson({data_var})")
            }
            Some(SchemaKind::Alias { target }) => {
                let cls = dart_class_name(target);
                format!("{cls}.fromJson({data_var} as Map<String, dynamic>)")
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

fn dart_default_expr(default: &flap_ir::DefaultValue) -> String {
    use flap_ir::DefaultValue;
    match default {
        DefaultValue::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('\'', "\\'");
            format!("'{escaped}'")
        }
        DefaultValue::Integer(n) => n.to_string(),
        DefaultValue::Number(n) => {
            if n.fract() == 0.0 {
                format!("{n:.1}")
            } else {
                n.to_string()
            }
        }
        DefaultValue::Boolean(b) => b.to_string(),
    }
}

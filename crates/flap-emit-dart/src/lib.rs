//! Dart / Flutter code emitter (Phase 4 — production output).
//!
//! Public API:
//! - [`emit_models`] — one Dart source per top-level schema, plus one per
//!   synthesised inline enum, plus the shared `flap_utils.dart` runtime.
//! - [`emit_client`] — a single client file with one method per operation.
//!   Pass [`ClientBackend::Dio`] (default) or [`ClientBackend::Http`].

use std::collections::{BTreeMap, HashMap};

use flap_ir::{
    Api, ApiKeyLocation, Field, HttpMethod, Operation, ParameterLocation, RequestBody, Response,
    Schema, SchemaKind, SecurityScheme, TypeRef,
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

// ── Template support ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct TemplateConfig {
    /// Directory to search for template overrides.
    /// Resolution order per output file:
    ///   1. `{dir}/{exact_filename}`       — verbatim copy, no rendering
    ///   2. `{dir}/model.dart.jinja`       — Jinja2 template for every model
    ///   3. `{dir}/client.dart.jinja`      — Jinja2 template for the client
    ///   4. `{dir}/flap_utils.dart`        — verbatim override for the runtime file
    ///   5. built-in emitter               — fallback when nothing matches
    pub template_dir: Option<std::path::PathBuf>,
}

impl TemplateConfig {
    /// Returns the raw content of `{template_dir}/{filename}` if the file exists.
    fn verbatim(&self, filename: &str) -> Option<String> {
        let dir = self.template_dir.as_ref()?;
        std::fs::read_to_string(dir.join(filename)).ok()
    }

    /// Returns the raw content of `{template_dir}/{name}.jinja` if the file exists.
    fn jinja(&self, name: &str) -> Option<String> {
        let dir = self.template_dir.as_ref()?;
        std::fs::read_to_string(dir.join(format!("{name}.jinja"))).ok()
    }
}

// ── Jinja2 template contexts (serde::Serialize so minijinja can use them) ─────

#[derive(serde::Serialize)]
struct ModelTemplateCtx {
    /// Dart class name after collision-avoidance and type mapping.
    class_name: String,
    /// Original schema name from the spec.
    schema_name: String,
    /// snake_case class name — used for `part` directives.
    snake_name: String,
    fields: Vec<FieldTemplateCtx>,
    /// Ready-to-emit `import '...';` lines, sorted and deduped.
    imports: Vec<String>,
    has_optional_fields: bool,
    /// Parent schema name when the schema uses allOf inheritance.
    extends: Option<String>,
    /// "safe" or "unsafe"
    null_safety: String,
}

#[derive(serde::Serialize)]
struct FieldTemplateCtx {
    /// Original spec field name.
    spec_name: String,
    /// camelCase Dart identifier.
    dart_name: String,
    /// Full resolved Dart type, e.g. `"List<String>"`, `"Pet?"`, `"Optional<int?>"`.
    dart_type: String,
    required: bool,
    nullable: bool,
    /// True when this field uses the `Optional<T?>` absent/present wrapper.
    uses_optional_wrapper: bool,
    /// `@Default(...)` expression, if any.
    default_expr: Option<String>,
    /// Non-null when the Dart identifier differs from the spec name.
    json_name: Option<String>,
}

#[derive(serde::Serialize)]
struct ClientTemplateCtx {
    class_name: String,
    /// First server URL, or empty string.
    default_base_url: String,
    base_urls: Vec<String>,
    operations: Vec<OperationTemplateCtx>,
    credentials: Vec<CredentialTemplateCtx>,
    /// "dio" or "http"
    backend: String,
    /// "safe" or "unsafe"
    null_safety: String,
}

#[derive(serde::Serialize)]
struct OperationTemplateCtx {
    method: String,
    path: String,
    method_name: String,
    summary: Option<String>,
    return_type: String,
    parameters: Vec<ParamTemplateCtx>,
    has_body: bool,
    body_type: Option<String>,
    body_required: bool,
    is_multipart: bool,
}

#[derive(serde::Serialize)]
struct ParamTemplateCtx {
    spec_name: String,
    dart_name: String,
    dart_type: String,
    /// "query", "path", "header", or "cookie"
    location: String,
    required: bool,
}

#[derive(serde::Serialize)]
struct CredentialTemplateCtx {
    dart_param_name: String,
    /// "apiKey", "httpBearer", "httpBasic", "oauth2", "openIdConnect"
    scheme_type: String,
}

// ── Jinja renderer ────────────────────────────────────────────────────────────

fn render_jinja(template_src: &str, context: impl serde::Serialize) -> Result<String, String> {
    let mut env = minijinja::Environment::new();
    env.add_template("t", template_src)
        .map_err(|e| format!("template parse error: {e}"))?;
    let tmpl = env.get_template("t").unwrap();
    let ctx = minijinja::Value::from_serialize(&context);
    tmpl.render(ctx).map_err(|e| format!("template render error: {e}"))
}

// ── Context builders ──────────────────────────────────────────────────────────

fn build_model_ctx(
    schema: &Schema,
    class_name: &str,
    registry: &EnumRegistry,
    schemas: &[Schema],
    mode: NullSafety,
    mappings: &MappingConfig,
) -> ModelTemplateCtx {
    let snake_name = to_snake_case(class_name);
    let null_safety = if mode == NullSafety::Safe { "safe" } else { "unsafe" }.to_string();

    let fields = match &schema.kind {
        SchemaKind::Object { fields } => fields
            .iter()
            .map(|f| build_field_ctx(f, &schema.name, registry, mode, mappings))
            .collect(),
        _ => vec![],
    };

    let has_optional_fields = fields.iter().any(|f: &FieldTemplateCtx| f.uses_optional_wrapper);

    let mut imports: Vec<String> = Vec::new();
    if let SchemaKind::Object { fields } = &schema.kind {
        for field in fields {
            collect_field_imports(
                &field.type_ref, &field.name, &schema.name, class_name,
                registry, &mut imports, mappings,
            );
        }
    }
    imports.sort();
    imports.dedup();

    ModelTemplateCtx {
        class_name: class_name.to_string(),
        schema_name: schema.name.clone(),
        snake_name,
        fields,
        imports,
        has_optional_fields,
        extends: schema.extends.clone(),
        null_safety,
    }
}

fn build_field_ctx(
    field: &Field,
    schema_name: &str,
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> FieldTemplateCtx {
    let synth = registry.lookup_field(schema_name, &field.name);
    let dart_type = to_dart_type(&field.type_ref, synth, mappings);
    let dart_name = to_camel_case(&field.name);
    let uses_optional_wrapper = mode == NullSafety::Safe
        && !field.required
        && field.nullable
        && type_ref_supports_optional_wrapper(&field.type_ref);

    let dart_type_in_ctx = if uses_optional_wrapper {
        format!("Optional<{dart_type}?>")
    } else if !field.required || field.nullable {
        format!("{dart_type}?")
    } else {
        dart_type
    };

    let default_expr = field
        .default_value
        .as_ref()
        .map(dart_default_expr);

    let json_name = if dart_name != field.name {
        Some(field.name.clone())
    } else {
        None
    };

    FieldTemplateCtx {
        spec_name: field.name.clone(),
        dart_name,
        dart_type: dart_type_in_ctx,
        required: field.required,
        nullable: field.nullable,
        uses_optional_wrapper,
        default_expr,
        json_name,
    }
}

fn build_client_ctx(
    api: &Api,
    class_name: &str,
    registry: &EnumRegistry,
    mode: NullSafety,
    backend: ClientBackend,
    mappings: &MappingConfig,
) -> ClientTemplateCtx {
    let null_safety = if mode == NullSafety::Safe { "safe" } else { "unsafe" }.to_string();
    let backend_str = match backend {
        ClientBackend::Dio  => "dio",
        ClientBackend::Http => "http",
    }
    .to_string();

    let default_base_url = api.base_urls.first().cloned().unwrap_or_default();

    let credentials: Vec<CredentialTemplateCtx> = api
        .security_schemes
        .iter()
        .map(|s| CredentialTemplateCtx {
            dart_param_name: escape_dart_keyword(&to_camel_case(s.scheme_name())),
            scheme_type: match s {
                SecurityScheme::ApiKey { .. }      => "apiKey",
                SecurityScheme::HttpBasic { .. }   => "httpBasic",
                SecurityScheme::HttpBearer { .. }  => "httpBearer",
                SecurityScheme::OAuth2 { .. }      => "oauth2",
                SecurityScheme::OpenIdConnect { .. } => "openIdConnect",
            }
            .to_string(),
        })
        .collect();

    let operations = api
        .operations
        .iter()
        .map(|op| {
            let dart_params = build_dart_params(op, registry, mappings);
            let return_type = success_return_type(&op.responses, mode, mappings);
            let op_id = op.operation_id.as_deref().unwrap_or("");

            let (has_body, body_type, body_required, is_multipart) =
                if let Some(rb) = &op.request_body {
                    let bt = to_dart_type(&rb.schema_ref, registry.lookup_body(op_id), mappings);
                    (true, Some(bt), rb.required, rb.is_multipart)
                } else {
                    (false, None, false, false)
                };

            OperationTemplateCtx {
                method: op.method.to_string(),
                path: op.path.clone(),
                method_name: op_method_name(op),
                summary: op.summary.clone(),
                return_type,
                parameters: dart_params
                    .iter()
                    .map(|p| ParamTemplateCtx {
                        spec_name: p.spec_name.to_string(),
                        dart_name: p.dart_name.clone(),
                        dart_type: p.non_null_type.clone(),
                        location: p.location.to_string(),
                        required: p.required,
                    })
                    .collect(),
                has_body,
                body_type,
                body_required,
                is_multipart,
            }
        })
        .collect();

    ClientTemplateCtx {
        class_name: class_name.to_string(),
        default_base_url,
        base_urls: api.base_urls.clone(),
        operations,
        credentials,
        backend: backend_str,
        null_safety,
    }
}

// ── Public enums ──────────────────────────────────────────────────────────────

/// Controls whether the emitted Dart code targets sound null safety (Dart ≥ 2.12)
/// or the legacy null-unsafe dialect (Dart < 2.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullSafety {
    Safe,
    Unsafe,
}

/// Controls which HTTP client package the generated client uses.
///
/// | concern              | Dio                          | Http                        |
/// |----------------------|------------------------------|-----------------------------|
/// | package              | `package:dio/dio.dart`       | `package:http/http.dart`    |
/// | CancelToken          | ✅ per-method param           | ❌                          |
/// | Interceptors         | ✅ constructor param          | ❌                          |
/// | BaseOptions          | ✅ constructor param          | ❌                          |
/// | HttpClientAdapter    | ✅ constructor param          | ❌                          |
/// | Injectable client    | ✅ via BaseOptions            | ✅ `http.Client?` param      |
/// | Multipart / FormData | ✅ `FormData`                 | ✅ `MultipartRequest`        |
/// | Response headers     | ✅ typed record               | ✅ typed record (via Map)   |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientBackend {
    /// `package:dio` — full feature set. This is the default.
    Dio,
    /// `package:http` — simpler, no interceptors or cancel tokens.
    Http,
}

// ── NEW: add near NullSafety and ClientBackend ────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct MappingConfig {
    /// Schema name → replacement Dart type.  e.g. `"Pet" → "ExamplePet"`.
    pub type_map: std::collections::HashMap<String, String>,
    /// Dart type name → import URI.  e.g. `"ExamplePet" → "package:myapp/example_pet.dart"`.
    pub import_map: std::collections::HashMap<String, String>,
}

impl MappingConfig {
    pub fn is_empty(&self) -> bool {
        self.type_map.is_empty() && self.import_map.is_empty()
    }

    /// Resolve a schema name to its Dart class name, honouring any type mapping.
    pub fn resolve_class(&self, schema_name: &str) -> String {
        if let Some(mapped) = self.type_map.get(schema_name) {
            mapped.clone()
        } else {
            dart_class_name(schema_name)
        }
    }

    /// Returns a ready-to-emit import line for a Dart type, if one is registered.
    pub fn import_for(&self, dart_type: &str) -> Option<String> {
        self.import_map
            .get(dart_type)
            .map(|path| format!("import '{path}';"))
    }
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
    body_enums: HashMap<String, String>,
    response_enums: HashMap<(String, String), String>,
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
pub fn emit_models(
    api: &Api,
    mode: NullSafety,
    mappings: &MappingConfig,
    templates: &TemplateConfig,
) -> HashMap<String, String> {
    let registry = EnumRegistry::build(api);
    let mut files = HashMap::new();

    if mode == NullSafety::Safe {
        // Allow verbatim override of the runtime file.
        let utils = templates
            .verbatim("flap_utils.dart")
            .unwrap_or_else(emit_flap_utils);
        files.insert("flap_utils.dart".to_string(), utils);
    }

    // Pre-load the global model Jinja template once (if present).
    let model_jinja = templates.jinja("model.dart");

    for schema in &api.schemas {
        if schema.internal { continue; }
        if mappings.type_map.contains_key(&schema.name) { continue; }

        let class_name = mappings.resolve_class(&schema.name);
        let filename = format!("{}.dart", to_snake_case(&class_name));

        let source = if let Some(verbatim) = templates.verbatim(&filename) {
            // 1. Exact file override — highest priority.
            verbatim
        } else if let Some(ref jinja_src) = model_jinja {
            // 2. Global model Jinja template.
            let ctx = build_model_ctx(schema, &class_name, &registry, &api.schemas, mode, mappings);
            match render_jinja(jinja_src, ctx) {
                Ok(rendered) => rendered,
                Err(e) => {
                    eprintln!("warning: model.dart.jinja error for `{}`: {e}", schema.name);
                    // Fall through to built-in on render error.
                    emit_schema(schema, &class_name, &registry, &api.schemas, mode, mappings)
                }
            }
        } else {
            // 3. Built-in emitter.
            emit_schema(schema, &class_name, &registry, &api.schemas, mode, mappings)
        };

        files.insert(filename, source);
    }

    for synth in registry.enums.values() {
        let filename = format!("{}.dart", to_snake_case(&synth.name));
        // Enum files support verbatim override only — no global Jinja template.
        let source = templates
            .verbatim(&filename)
            .unwrap_or_else(|| emit_synth_enum(synth));
        files.insert(filename, source);
    }

    files
}

// ── Public entry point: client ────────────────────────────────────────────────

/// Returns `(filename, dart_source)`.
pub fn emit_client(
    api: &Api,
    mode: NullSafety,
    backend: ClientBackend,
    mappings: &MappingConfig,
    templates: &TemplateConfig,
) -> (String, String) {
    let registry = EnumRegistry::build(api);
    let class_name = api_client_name(&api.title);
    let filename = format!("{}.dart", to_snake_case(&class_name));

    let source = if let Some(verbatim) = templates.verbatim(&filename) {
        // 1. Exact file override.
        verbatim
    } else if let Some(jinja_src) = templates.jinja("client.dart") {
        // 2. Global client Jinja template.
        let ctx = build_client_ctx(api, &class_name, &registry, mode, backend, mappings);
        match render_jinja(&jinja_src, ctx) {
            Ok(rendered) => rendered,
            Err(e) => {
                eprintln!("warning: client.dart.jinja render error: {e}");
                // Fall through to built-in on render error.
                match backend {
                    ClientBackend::Dio  => emit_client_dio(api, &class_name, &registry, mode, mappings),
                    ClientBackend::Http => emit_client_http(api, &class_name, &registry, mode, mappings),
                }
            }
        }
    } else {
        // 3. Built-in emitter.
        match backend {
            ClientBackend::Dio  => emit_client_dio(api, &class_name, &registry, mode, mappings),
            ClientBackend::Http => emit_client_http(api, &class_name, &registry, mode, mappings),
        }
    };

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

// ── flap_utils.dart runtime ───────────────────────────────────────────────────

fn emit_flap_utils() -> String {
    r#"// GENERATED — do not edit by hand.
import 'package:freezed_annotation/freezed_annotation.dart';

sealed class Optional<T> {
  const Optional();
  const factory Optional.present(T value) = _Present<T>;
  const factory Optional.absent() = _Absent<T>;

  bool get isPresent => this is _Present<T>;
  bool get isAbsent => this is _Absent<T>;

  T get value => switch (this) {
        _Present<T>(:final value) => value,
        _Absent<T>() => throw StateError('Optional.value called on Optional.absent()'),
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
      identical(this, other) || (other is _Present<T> && other.value == value);
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

const Object kOptionalAbsentSentinel = _OptionalAbsentSentinel();

class _OptionalAbsentSentinel {
  const _OptionalAbsentSentinel();
}

Map<String, dynamic> stripOptionalAbsent(Map<String, dynamic> m) {
  m.removeWhere((_, v) => identical(v, kOptionalAbsentSentinel));
  return m;
}

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
    mappings: &MappingConfig,
) -> String {
    match &schema.kind {
        SchemaKind::Object { fields } => emit_freezed_class(
            class_name,
            &schema.name,
            fields,
            schemas,
            registry,
            mode,
            mappings,
        ),
        SchemaKind::Array { item } => emit_array_typedef(class_name, item, mappings),
        SchemaKind::Map { value } => emit_map_typedef(class_name, value, mappings),
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
            mappings,
        ),
        SchemaKind::UntaggedUnion { variants } => emit_untagged_union(
            class_name,
            &schema.name,
            variants,
            schemas,
            registry,
            mode,
            mappings,
        ),
        SchemaKind::Alias { target } => emit_alias_typedef(class_name, target, mappings),
    }
}

fn emit_alias_typedef(alias_name: &str, target: &str, mappings: &MappingConfig) -> String {
    let target_cls = mappings.resolve_class(target);
    let target_file = to_snake_case(&target_cls);
    let import_line = mappings
        .import_for(&target_cls)
        .unwrap_or_else(|| format!("import '{target_file}.dart';"));
    format!(
        "// Generated from OpenAPI $ref alias `{alias_name}` → `{target}`.\n\
         {import_line}\n\
         typedef {alias_name} = {target_cls};\n"
    )
}

fn emit_array_typedef(name: &str, item: &TypeRef, mappings: &MappingConfig) -> String {
    let dart_item = to_dart_type(item, None, mappings);
    format!(
        "// Generated from OpenAPI array schema `{name}`.\n\
         typedef {name} = List<{dart_item}>;\n"
    )
}

fn emit_map_typedef(name: &str, value: &TypeRef, mappings: &MappingConfig) -> String {
    let dart_value = to_dart_type(value, None, mappings);
    format!(
        "// Generated from OpenAPI map schema `{name}`\n\
         // (object with `additionalProperties` and no fixed properties).\n\
         typedef {name} = Map<String, {dart_value}>;\n"
    )
}

// ── @freezed class ────────────────────────────────────────────────────────────

fn field_uses_optional_wrapper(field: &Field, mode: NullSafety) -> bool {
    mode == NullSafety::Safe
        && !field.required
        && field.nullable
        && type_ref_supports_optional_wrapper(&field.type_ref)
}

fn class_has_optional_wrapper_field(fields: &[Field], mode: NullSafety) -> bool {
    fields.iter().any(|f| field_uses_optional_wrapper(f, mode))
}

fn type_ref_supports_optional_wrapper(type_ref: &TypeRef) -> bool {
    matches!(
        type_ref,
        TypeRef::String | TypeRef::Integer { .. } | TypeRef::Number { .. } | TypeRef::Boolean
    )
}

fn emit_freezed_class(
    class_name: &str,
    schema_name: &str,
    fields: &[Field],
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
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
            mappings,
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
    out.push_str(&format!("part '{snake}.g.dart';\n\n"));
    out.push_str("@freezed\n");
    out.push_str(&format!("class {class_name} with _${class_name} {{\n"));
    if has_optional {
        out.push_str(&format!("  const {class_name}._();\n\n"));
    }
    out.push_str(&format!("  const factory {class_name}({{\n"));
    for field in fields {
        out.push_str(&emit_field(
            field,
            schema_name,
            schemas,
            registry,
            mode,
            mappings,
        ));
    }
    out.push_str(&format!("  }}) = _{class_name};\n\n"));
    out.push_str(&format!(
        "  factory {class_name}.fromJson(Map<String, dynamic> json) =>\n      _${class_name}FromJson(json);\n"
    ));
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

fn emit_field(
    field: &Field,
    schema_name: &str,
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    let synth = registry.lookup_field(schema_name, &field.name);
    let dart_type = to_dart_type(&field.type_ref, synth, mappings);
    let dart_name = to_camel_case(&field.name);
    let force_nullable_for_recursion =
        field.is_recursive && matches!(&field.type_ref, TypeRef::Named(_));

    let mut json_key_args: Vec<String> = Vec::new();
    if dart_name != field.name {
        json_key_args.push(format!("name: '{}'", field.name));
    }
    let mut sibling_annotations: Vec<String> = Vec::new();

    // Only emit the converter annotation when the schema is NOT mapped away —
    // if it is mapped, the external type handles its own serialization.
    if let TypeRef::Named(name) = &field.type_ref
        && is_untagged_union(schemas, name)
        && !mappings.type_map.contains_key(name.as_str())
    {
        sibling_annotations.push(format!("@_{name}Converter()"));
    }

    let mut leading_comment: Option<String> = None;

    let typed_fragment = match mode {
        NullSafety::Unsafe => {
            let is_req = field.required && !force_nullable_for_recursion;
            if is_req {
                sibling_annotations.insert(0, "@required".to_string());
            } else {
                json_key_args.push("includeIfNull: false".to_string());
            }
            if let Some(default) = &field.default_value {
                sibling_annotations.push(format!("@Default({})", dart_default_expr(default)));
            }
            format!("{dart_type} {dart_name},\n")
        }
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
                            sibling_annotations
                                .push(format!("@Default({})", dart_default_expr(default)));
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
    mappings: &MappingConfig,
) {
    match type_ref {
        TypeRef::Enum(_) => {
            if let Some(synth) = registry.lookup_field(schema_name, field_name) {
                imports.push(format!("import '{}.dart';", to_snake_case(synth)));
            }
        }
        TypeRef::Named(name) => {
            let cls = mappings.resolve_class(name);
            if cls == class_name {
                return;
            }
            // Use the registered import if one exists, otherwise assume a sibling file.
            if let Some(import_line) = mappings.import_for(&cls) {
                imports.push(import_line);
            } else {
                imports.push(format!("import '{}.dart';", to_snake_case(&cls)));
            }
        }
        TypeRef::Map(inner) | TypeRef::Array(inner) => {
            collect_field_imports(
                inner,
                field_name,
                schema_name,
                class_name,
                registry,
                imports,
                mappings,
            );
        }
        _ => {}
    }
}

// ── @Freezed union ────────────────────────────────────────────────────────────

fn emit_freezed_union(
    class_name: &str,
    _schema_name: &str,
    variants: &[TypeRef],
    discriminator: &str,
    variant_tags: &[String],
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
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
                    mappings,
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
    out.push_str(&format!("part '{snake}.g.dart';\n\n"));
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
            out.push_str(&emit_field(
                field,
                variant_name,
                schemas,
                registry,
                mode,
                mappings,
            ));
        }
        out.push_str(&format!("  }}) = {variant_class};\n\n"));
    }

    out.push_str(&format!(
        "  factory {class_name}.fromJson(Map<String, dynamic> json) =>\n      _${class_name}FromJson(json);\n"
    ));
    out.push_str("}\n");
    out
}

// ── Untagged union ────────────────────────────────────────────────────────────

fn emit_untagged_union(
    class_name: &str,
    _schema_name: &str,
    variants: &[TypeRef],
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    let mut out = String::new();
    out.push_str("import 'dart:convert';\n");
    out.push_str("import 'package:flutter/foundation.dart';\n");

    let mut imports: Vec<String> = Vec::new();
    for variant in variants {
        if let TypeRef::Named(variant_name) = variant
            && !is_internal_wrapper(schemas, variant_name)
        {
            let cls = mappings.resolve_class(variant_name);
            if let Some(import_line) = mappings.import_for(&cls) {
                imports.push(import_line);
            } else {
                imports.push(format!("import '{}.dart';", to_snake_case(&cls)));
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

    out.push_str(&format!("sealed class {class_name} {{\n"));
    out.push_str(&format!("  const {class_name}._();\n\n"));

    for (i, variant) in variants.iter().enumerate() {
        let (variant_dart_type, _, _) =
            resolve_untagged_variant_info(variant, schemas, registry, mappings);
        out.push_str(&format!(
            "  const factory {class_name}.variant{i}({variant_dart_type} value) = _Variant{i};\n"
        ));
    }

    out.push_str(&format!(
        "\n  factory {class_name}.fromJson(dynamic json) {{\n"
    ));
    for (i, variant) in variants.iter().enumerate() {
        let (variant_dart_type, _, is_primitive) =
            resolve_untagged_variant_info(variant, schemas, registry, mappings);
        if is_primitive {
            out.push_str(&format!(
                "    if (json is {variant_dart_type}) return {class_name}.variant{i}(json);\n"
            ));
        } else {
            out.push_str("    if (json is Map<String, dynamic>) {\n");
            out.push_str("      try {\n");
            out.push_str(&format!(
                "        return {class_name}.variant{i}({variant_dart_type}.fromJson(json));\n"
            ));
            out.push_str("      } catch (_) {}\n");
            out.push_str("    }\n");
        }
    }
    out.push_str(&format!(
        "    throw ArgumentError('Cannot deserialize into {class_name}: $json');\n  }}\n\n"
    ));

    let obj_nullable = if mode == NullSafety::Safe {
        "Object?"
    } else {
        "Object"
    };
    out.push_str(&format!("  {obj_nullable} toJson();\n}}\n\n"));

    for (i, variant) in variants.iter().enumerate() {
        let (variant_dart_type, _, _) =
            resolve_untagged_variant_info(variant, schemas, registry, mappings);
        out.push_str(&format!("class _Variant{i} extends {class_name} {{\n"));
        out.push_str(&format!("  final {variant_dart_type} value;\n"));
        out.push_str(&format!("  const _Variant{i}(this.value) : super._();\n\n"));
        out.push_str("  @override\n");
        if is_variant_primitive(variant, schemas) {
            out.push_str(&format!("  {obj_nullable} toJson() => value;\n"));
        } else {
            out.push_str(&format!("  {obj_nullable} toJson() => value.toJson();\n"));
        }
        out.push_str("  @override\n");
        out.push_str(&format!(
            "  bool operator ==(Object other) => other is _Variant{i} && other.value == value;\n"
        ));
        out.push_str("  @override\n");
        out.push_str(&format!(
            "  int get hashCode => Object.hash(_Variant{i}, value);\n}}\n\n"
        ));
    }

    let converter_name = format!("_{class_name}Converter");
    out.push_str(&format!(
        "class {converter_name} implements JsonConverter<{class_name}, {obj_nullable}> {{\n  const {converter_name}();\n\n"
    ));
    out.push_str(&format!(
        "  @override\n  {class_name} fromJson({obj_nullable} json) => {class_name}.fromJson(json);\n\n"
    ));
    out.push_str(&format!(
        "  @override\n  {obj_nullable} toJson({class_name} object) => object.toJson();\n}}\n"
    ));
    out
}

fn resolve_untagged_variant_info(
    type_ref: &TypeRef,
    schemas: &[Schema],
    _registry: &EnumRegistry,
    mappings: &MappingConfig,
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
                    return (
                        to_dart_type(&fields[0].type_ref, None, mappings),
                        "value".to_string(),
                        true,
                    );
                }
                panic!("internal wrapper schema without a single 'value' field");
            }
            (mappings.resolve_class(name), "value".to_string(), false)
        }
        _ => panic!("unexpected TypeRef in untagged union variant"),
    }
}

fn is_internal_wrapper(schemas: &[Schema], variant_name: &str) -> bool {
    schemas.iter().any(|s| s.name == variant_name && s.internal)
}

fn is_variant_primitive(variant_type_ref: &TypeRef, schemas: &[Schema]) -> bool {
    matches!(variant_type_ref, TypeRef::Named(name) if is_internal_wrapper(schemas, name))
}

// ── Synthesised enum ──────────────────────────────────────────────────────────

fn emit_synth_enum(synth: &SynthEnum) -> String {
    let mut out = String::new();
    out.push_str("import 'package:freezed_annotation/freezed_annotation.dart';\n\n");
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
            flap_ir::EnumValue::Int(n) => (format!("v{n}"), format!("@JsonValue({n})")),
        };
        out.push_str(&format!("  {json_annotation}\n  {dart_case},\n"));
    }
    out.push_str("  @JsonValue(null)\n  unknown;\n}\n");
    out
}

// ── DIO client emitter ────────────────────────────────────────────────────────

fn emit_client_dio(
    api: &Api,
    class_name: &str,
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    let mut out = String::new();
    out.push_str("import 'dart:convert';\n");
    out.push_str("import 'package:dio/dio.dart';\n");
    if mode == NullSafety::Unsafe {
        out.push_str("import 'package:meta/meta.dart';\n");
    }
    out.push('\n');

    emit_server_urls(&mut out, class_name, &api.base_urls);
    emit_model_imports(&mut out, api, registry, mappings);

    let credentials: Vec<DartCredential> = api
        .security_schemes
        .iter()
        .map(DartCredential::from_scheme)
        .collect();

    out.push_str(&format!("class {class_name} {{\n"));
    out.push_str(&emit_constructor_dio(
        class_name,
        &credentials,
        &api.base_urls,
    ));
    out.push('\n');
    out.push_str("  late final Dio _dio;\n");

    for op in &api.operations {
        out.push('\n');
        out.push_str(&emit_method_dio(op, &api.schemas, registry, mode, mappings));
    }

    out.push_str("}\n");
    out
}

fn emit_constructor_dio(
    class_name: &str,
    credentials: &[DartCredential],
    base_urls: &[String],
) -> String {
    let default_url = base_urls
        .first()
        .map(|u| format!("'{u}'"))
        .unwrap_or_else(|| "''".to_string());
    let mut out = String::new();
    out.push_str(&format!("  {class_name}({{\n"));
    out.push_str(&format!("    String baseUrl = {default_url},\n"));
    for cred in credentials {
        out.push_str(&format!("    String? {},\n", cred.dart_param_name));
    }
    out.push_str("    BaseOptions? options,\n");
    out.push_str("    List<Interceptor> interceptors = const [],\n");
    out.push_str("    HttpClientAdapter? httpClientAdapter,\n");
    out.push_str("  }) {\n");
    out.push_str("    final _opts = (options ?? BaseOptions()).copyWith(baseUrl: baseUrl);\n");
    out.push_str("    _dio = Dio(_opts);\n");
    out.push_str("    if (httpClientAdapter != null) {\n");
    out.push_str("      _dio.httpClientAdapter = httpClientAdapter;\n");
    out.push_str("    }\n");
    out.push_str("    _dio.interceptors.addAll(interceptors);\n");

    if !credentials.is_empty() {
        out.push_str("    _dio.interceptors.add(\n");
        out.push_str("      InterceptorsWrapper(\n");
        out.push_str("        onRequest: (options, handler) {\n");
        for cred in credentials {
            out.push_str(&emit_credential_injection_dio(cred));
        }
        out.push_str("          handler.next(options);\n");
        out.push_str("        },\n");
        out.push_str("      ),\n");
        out.push_str("    );\n");
    }

    out.push_str("  }\n");
    out
}

fn emit_credential_injection_dio(cred: &DartCredential) -> String {
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
                 options.headers['Cookie'] = existing == null ? cookie : '$existing; $cookie';\n          }}\n"
            ),
        },
        SecurityScheme::OAuth2 { .. } | SecurityScheme::OpenIdConnect { .. } => format!(
            "          if ({dart} != null) {{\n            \
             options.headers['Authorization'] = 'Bearer ${{{dart}}}';\n          }}\n"
        ),
    }
}

struct DartParam<'a> {
    spec_name: &'a str,
    dart_name: String,
    location: ParameterLocation,
    non_null_type: String,
    required: bool,
}

fn build_dart_params<'a>(
    op: &'a Operation,
    registry: &EnumRegistry,
    mappings: &MappingConfig,
) -> Vec<DartParam<'a>> {
    let op_id = op.operation_id.as_deref().unwrap_or("");
    let mut out = Vec::with_capacity(op.parameters.len());
    let mut seen: HashMap<&str, ParameterLocation> = HashMap::new();

    for param in &op.parameters {
        if let Some(prev) = seen.get(param.name.as_str()) {
            panic!(
                "parameter `{}` of operation `{}` appears in both `{}` and `{}` locations",
                param.name, op_id, prev, param.location
            );
        }
        seen.insert(&param.name, param.location);
        let synth = registry.lookup_param(op_id, &param.name);
        out.push(DartParam {
            spec_name: &param.name,
            dart_name: escape_dart_keyword(&to_camel_case(&param.name)),
            location: param.location,
            non_null_type: to_dart_type(&param.type_ref, synth, mappings),
            required: param.required,
        });
    }
    out
}

fn emit_method_dio(
    op: &Operation,
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    let mut out = String::new();
    if let Some(summary) = &op.summary {
        out.push_str(&format!("  /// {summary}\n"));
    }
    out.push_str(&format!("  // {} {}\n", op.method, op.path));

    let method_name = op_method_name(op);
    let dart_params = build_dart_params(op, registry, mappings);
    let return_type = success_return_type(&op.responses, mode, mappings);
    let has_params = !dart_params.is_empty() || op.request_body.is_some();

    out.push_str(&format!("  Future<{return_type}> {method_name}({{\n"));
    if has_params {
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
            let body_type = to_dart_type(&body.schema_ref, registry.lookup_body(op_id), mappings);
            if body.required {
                out.push_str(&format!("    required {body_type} body,\n"));
            } else {
                out.push_str(&format!("    {body_type}? body,\n"));
            }
        }
        for p in &optional {
            out.push_str(&format!("    {}? {},\n", p.non_null_type, p.dart_name));
        }
    }
    out.push_str("    CancelToken? cancelToken,\n");
    out.push_str("  }) async {\n");

    out.push_str(&emit_method_body_dio(
        op,
        &dart_params,
        schemas,
        registry,
        mode,
        mappings,
    ));
    out.push_str("  }\n");
    out
}

fn emit_method_body_dio(
    op: &Operation,
    dart_params: &[DartParam],
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    let mut body = String::new();

    let mut templated_path = op.path.clone();
    for p in dart_params {
        if p.location == ParameterLocation::Path {
            templated_path = templated_path.replace(
                &format!("{{{}}}", p.spec_name),
                &format!("${{{}}}", p.dart_name),
            );
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

    let data_expr = op.request_body.as_ref().map(body_data_expression_dio);

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
    body.push_str("      cancelToken: cancelToken,\n");
    body.push_str("    );\n");

    if let Some(resp) = success_response(&op.responses) {
        body.push_str(&emit_success_return_dio(
            resp, schemas, registry, mode, mappings,
        ));
    }

    body
}

fn body_data_expression_dio(body: &RequestBody) -> String {
    if !body.is_multipart {
        return match &body.schema_ref {
            TypeRef::Named(_) => "body.toJson()".into(),
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
        _ => "FormData.fromMap({'data': body})".into(),
    }
}

fn emit_success_return_dio(
    resp: &Response,
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    if mode == NullSafety::Unsafe {
        if let Some(schema) = &resp.schema_ref {
            return format!(
                "    return {};\n",
                deserialize_expr(schema, schemas, registry, "response.data", mappings)
            );
        }
        return String::new();
    }

    let has_headers = !resp.headers.is_empty();
    if !has_headers {
        if let Some(schema) = &resp.schema_ref {
            return format!(
                "    return {};\n",
                deserialize_expr(schema, schemas, registry, "response.data", mappings)
            );
        }
        return String::new();
    }

    let mut out = String::new();
    for hdr in &resp.headers {
        let dart_name = to_camel_case(&hdr.name.replace('-', "_"));
        let raw_expr = format!("response.headers.value('{}')", hdr.name.to_lowercase());
        if hdr.required {
            out.push_str(&format!(
                "    final {dart_name} = {};\n",
                header_deserialize_expr(&hdr.type_ref, &raw_expr)
            ));
        } else {
            out.push_str(&format!("    final {dart_name}Raw = {raw_expr};\n"));
            out.push_str(&format!(
                "    final {dart_name} = {};\n",
                header_deserialize_expr_nullable(&hdr.type_ref, &format!("{dart_name}Raw"))
            ));
        }
    }

    let mut record_fields: Vec<String> = Vec::new();
    if let Some(s) = &resp.schema_ref {
        record_fields.push(format!(
            "body: {}",
            deserialize_expr(s, schemas, registry, "response.data", mappings)
        ));
    }
    for hdr in &resp.headers {
        let dart_name = to_camel_case(&hdr.name.replace('-', "_"));
        record_fields.push(format!("{dart_name}: {dart_name}"));
    }
    out.push_str(&format!("    return ({});\n", record_fields.join(", ")));
    out
}

// ── HTTP client emitter ───────────────────────────────────────────────────────

fn emit_client_http(
    api: &Api,
    class_name: &str,
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    let mut out = String::new();
    out.push_str("import 'dart:convert';\n");
    out.push_str("import 'package:http/http.dart' as http;\n");
    if mode == NullSafety::Unsafe {
        out.push_str("import 'package:meta/meta.dart';\n");
    }
    out.push('\n');

    emit_server_urls(&mut out, class_name, &api.base_urls);
    emit_model_imports(&mut out, api, registry, mappings);

    let credentials: Vec<DartCredential> = api
        .security_schemes
        .iter()
        .map(DartCredential::from_scheme)
        .collect();

    out.push_str(&format!("class {class_name} {{\n"));
    out.push_str(&emit_constructor_http(
        class_name,
        &credentials,
        &api.base_urls,
    ));
    out.push('\n');
    out.push_str("  final String _baseUrl;\n");
    out.push_str("  final http.Client _client;\n");

    if credentials.is_empty() {
        out.push_str(
            "  Map<String, String> get _headers => {'Content-Type': 'application/json'};\n",
        );
    } else {
        out.push_str("  Map<String, String> get _headers => {\n");
        out.push_str("    'Content-Type': 'application/json',\n");
        for cred in &credentials {
            out.push_str(&emit_credential_header_http(cred));
        }
        out.push_str("  };\n");
        out.push('\n');
        for cred in &credentials {
            out.push_str(&format!("  final String? _{};\n", cred.dart_param_name));
        }
    }

    for op in &api.operations {
        out.push('\n');
        out.push_str(&emit_method_http(
            op,
            &api.schemas,
            registry,
            mode,
            mappings,
        ));
    }

    out.push_str("}\n");
    out
}

fn emit_constructor_http(
    class_name: &str,
    credentials: &[DartCredential],
    base_urls: &[String],
) -> String {
    let default_url = base_urls
        .first()
        .map(|u| format!("'{u}'"))
        .unwrap_or_else(|| "''".to_string());
    let mut out = String::new();
    out.push_str(&format!("  {class_name}({{\n"));
    out.push_str(&format!("    String baseUrl = {default_url},\n"));
    for cred in credentials {
        out.push_str(&format!("    String? {},\n", cred.dart_param_name));
    }
    out.push_str("    http.Client? client,\n");
    out.push_str("  }) : _baseUrl = baseUrl.endsWith('/') ? baseUrl.substring(0, baseUrl.length - 1) : baseUrl,\n");
    out.push_str("       _client = client ?? http.Client()");
    for cred in credentials {
        out.push_str(&format!(
            ",\n       _{} = {}",
            cred.dart_param_name, cred.dart_param_name
        ));
    }
    out.push_str(";\n");
    out
}

fn emit_credential_header_http(cred: &DartCredential) -> String {
    let dart = &cred.dart_param_name;
    match cred.scheme {
        SecurityScheme::HttpBearer { .. } => {
            format!("    if (_{dart} != null) 'Authorization': 'Bearer ${{_{dart}!}}',\n")
        }
        SecurityScheme::HttpBasic { .. } => {
            format!(
                "    if (_{dart} != null) 'Authorization': 'Basic ${{base64Encode(utf8.encode(_{dart}!))}}',\n"
            )
        }
        SecurityScheme::ApiKey {
            parameter_name,
            location,
            ..
        } => match location {
            ApiKeyLocation::Header => {
                format!("    if (_{dart} != null) '{parameter_name}': _{dart}!,\n")
            }
            // Query and cookie auth are handled at the request level, not in headers
            _ => String::new(),
        },
        SecurityScheme::OAuth2 { .. } | SecurityScheme::OpenIdConnect { .. } => {
            format!("    if (_{dart} != null) 'Authorization': 'Bearer ${{_{dart}!}}',\n")
        }
    }
}

fn emit_method_http(
    op: &Operation,
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    let mut out = String::new();
    if let Some(summary) = &op.summary {
        out.push_str(&format!("  /// {summary}\n"));
    }
    out.push_str(&format!("  // {} {}\n", op.method, op.path));

    let method_name = op_method_name(op);
    let dart_params = build_dart_params(op, registry, mappings);
    let return_type = success_return_type(&op.responses, mode, mappings);
    let has_params = !dart_params.is_empty() || op.request_body.is_some();

    if !has_params {
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
            let body_type = to_dart_type(&body.schema_ref, registry.lookup_body(op_id), mappings);
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

    out.push_str(&emit_method_body_http(
        op,
        &dart_params,
        schemas,
        registry,
        mode,
        mappings,
    ));
    out.push_str("  }\n");
    out
}

fn emit_method_body_http(
    op: &Operation,
    dart_params: &[DartParam],
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    let mut body = String::new();

    let mut templated_path = op.path.clone();
    for p in dart_params {
        if p.location == ParameterLocation::Path {
            templated_path = templated_path.replace(
                &format!("{{{}}}", p.spec_name),
                &format!("${{{}}}", p.dart_name),
            );
        }
    }

    let query_params: Vec<&DartParam> = dart_params
        .iter()
        .filter(|p| p.location == ParameterLocation::Query)
        .collect();

    if query_params.is_empty() {
        body.push_str(&format!(
            "    final uri = Uri.parse('$_baseUrl{templated_path}');\n"
        ));
    } else {
        body.push_str("    final _queryParams = <String, dynamic>{\n");
        for p in &query_params {
            if p.required {
                body.push_str(&format!("      '{}': ${{{}}},\n", p.spec_name, p.dart_name));
            } else {
                body.push_str(&format!(
                    "      if ({} != null) '{}': ${{{}}},\n",
                    p.dart_name, p.spec_name, p.dart_name
                ));
            }
        }
        body.push_str("    };\n");
        body.push_str(&format!(
            "    final uri = Uri.parse('$_baseUrl{templated_path}').replace(\n\
                   queryParameters: _queryParams.map((k, v) => MapEntry(k, v.toString())),\n    );\n"
        ));
    }

    let header_params: Vec<&DartParam> = dart_params
        .iter()
        .filter(|p| p.location == ParameterLocation::Header)
        .collect();
    if !header_params.is_empty() {
        body.push_str("    final _extraHeaders = <String, String>{\n");
        for p in &header_params {
            if p.required {
                body.push_str(&format!(
                    "      '{}': ${{{}}}.toString(),\n",
                    p.spec_name, p.dart_name
                ));
            } else {
                body.push_str(&format!(
                    "      if ({} != null) '{}': ${{{}}}.toString(),\n",
                    p.dart_name, p.spec_name, p.dart_name
                ));
            }
        }
        body.push_str("    };\n");
        body.push_str("    final _allHeaders = {..._headers, ..._extraHeaders};\n");
    } else {
        body.push_str("    final _allHeaders = _headers;\n");
    }

    let method_lower = op.method.to_string().to_lowercase();

    if let Some(req_body) = &op.request_body {
        if req_body.is_multipart {
            let path_expr = format!("Uri.parse('$_baseUrl{templated_path}')");
            body.push_str(&format!(
                "    final _request = http.MultipartRequest('{}', {path_expr});\n",
                op.method
            ));
            body.push_str("    _request.headers.addAll(_allHeaders);\n");
            match &req_body.schema_ref {
                TypeRef::Named(_) => {
                    body.push_str(
                        "    body.toJson().forEach((k, v) => _request.fields[k] = v.toString());\n",
                    );
                }
                _ => {
                    body.push_str("    _request.fields['data'] = body.toString();\n");
                }
            }
            body.push_str("    final _streamed = await _client.send(_request);\n");
            body.push_str("    final _response = await http.Response.fromStream(_streamed);\n");
        } else {
            let body_expr = match &req_body.schema_ref {
                TypeRef::Named(_) => "jsonEncode(body.toJson())".to_string(),
                TypeRef::Enum(_) => "jsonEncode(body)".to_string(),
                _ => "body.toString()".to_string(),
            };
            body.push_str(&format!(
                "    final _response = await _client.{method_lower}(uri, headers: _allHeaders, body: {body_expr});\n"
            ));
        }
    } else {
        body.push_str(&format!(
            "    final _response = await _client.{method_lower}(uri, headers: _allHeaders);\n"
        ));
    }

    body.push_str(&format!(
        "    if (_response.statusCode < 200 || _response.statusCode >= 300) {{\n      \
         throw Exception('{} {} returned ${{_response.statusCode}}: ${{_response.body}}');\n    }}\n",
        op.method, op.path
    ));

    if let Some(resp) = success_response(&op.responses) {
        body.push_str(&emit_success_return_http(
            resp, schemas, registry, mode, mappings,
        ));
    }

    body
}

fn emit_success_return_http(
    resp: &Response,
    schemas: &[Schema],
    registry: &EnumRegistry,
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    let has_headers = !resp.headers.is_empty();

    if !has_headers {
        if let Some(schema) = &resp.schema_ref {
            return format!(
                "    return {};\n",
                deserialize_expr(
                    schema,
                    schemas,
                    registry,
                    "jsonDecode(_response.body)",
                    mappings
                )
            );
        }
        return String::new();
    }

    if mode == NullSafety::Unsafe {
        if let Some(schema) = &resp.schema_ref {
            return format!(
                "    return {};\n",
                deserialize_expr(
                    schema,
                    schemas,
                    registry,
                    "jsonDecode(_response.body)",
                    mappings
                )
            );
        }
        return String::new();
    }

    let mut out = String::new();
    for hdr in &resp.headers {
        let dart_name = to_camel_case(&hdr.name.replace('-', "_"));
        let map_key = hdr.name.to_lowercase();
        let raw_expr = format!("_response.headers['{map_key}']");
        if hdr.required {
            out.push_str(&format!(
                "    final {dart_name} = {};\n",
                header_deserialize_expr(&hdr.type_ref, &raw_expr)
            ));
        } else {
            out.push_str(&format!("    final {dart_name}Raw = {raw_expr};\n"));
            out.push_str(&format!(
                "    final {dart_name} = {};\n",
                header_deserialize_expr_nullable(&hdr.type_ref, &format!("{dart_name}Raw"))
            ));
        }
    }

    let mut record_fields: Vec<String> = Vec::new();
    if let Some(s) = &resp.schema_ref {
        record_fields.push(format!(
            "body: {}",
            deserialize_expr(s, schemas, registry, "jsonDecode(_response.body)", mappings)
        ));
    }
    for hdr in &resp.headers {
        let dart_name = to_camel_case(&hdr.name.replace('-', "_"));
        record_fields.push(format!("{dart_name}: {dart_name}"));
    }
    out.push_str(&format!("    return ({});\n", record_fields.join(", ")));
    out
}

// ── Shared emitter helpers ────────────────────────────────────────────────────

fn emit_server_urls(out: &mut String, class_name: &str, base_urls: &[String]) {
    if base_urls.len() > 1 {
        out.push_str(&format!("abstract final class {class_name}Urls {{\n"));
        for (i, url) in base_urls.iter().enumerate() {
            out.push_str(&format!("  static const String server{i} = '{url}';\n"));
        }
        out.push_str("}\n\n");
    }
}

fn emit_model_imports(
    out: &mut String,
    api: &Api,
    registry: &EnumRegistry,
    mappings: &MappingConfig,
) {
    let mut imports: Vec<String> = Vec::new();

    for schema in &api.schemas {
        if schema.internal {
            continue;
        }
        let cls = mappings.resolve_class(&schema.name);
        // If the schema is mapped to an external type, use its registered import.
        // If there is no registered import, fall back to the generated sibling file.
        if let Some(import_line) = mappings.import_for(&cls) {
            imports.push(import_line);
        } else if !mappings.type_map.contains_key(&schema.name) {
            // Only emit a sibling-file import when we actually generated a file for it.
            imports.push(format!("import '{}.dart';", to_snake_case(&cls)));
        }
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
}

// ── Credential helper (shared by both backends) ───────────────────────────────

struct DartCredential<'a> {
    scheme: &'a SecurityScheme,
    dart_param_name: String,
}

impl<'a> DartCredential<'a> {
    fn from_scheme(scheme: &'a SecurityScheme) -> Self {
        Self {
            scheme,
            dart_param_name: escape_dart_keyword(&to_camel_case(scheme.scheme_name())),
        }
    }
}

// ── Response helpers (shared) ─────────────────────────────────────────────────

fn success_return_type(
    responses: &[Response],
    mode: NullSafety,
    mappings: &MappingConfig,
) -> String {
    let Some(resp) = success_response(responses) else {
        return "void".into();
    };

    if mode == NullSafety::Unsafe || resp.headers.is_empty() {
        return match &resp.schema_ref {
            Some(t) => to_dart_type(t, None, mappings),
            None => "void".into(),
        };
    }

    let body_type = match &resp.schema_ref {
        Some(t) => to_dart_type(t, None, mappings),
        None => "void".into(),
    };

    let mut fields: Vec<String> = Vec::new();
    if resp.schema_ref.is_some() {
        fields.push(format!("{body_type} body"));
    }
    for hdr in &resp.headers {
        let dart_type = to_dart_type(&hdr.type_ref, None, mappings);
        let dart_name = to_camel_case(&hdr.name.replace('-', "_"));
        if hdr.required {
            fields.push(format!("{dart_type} {dart_name}"));
        } else {
            fields.push(format!("{dart_type}? {dart_name}"));
        }
    }
    format!("({{{}}})", fields.join(", "))
}

fn success_response(responses: &[Response]) -> Option<&Response> {
    responses
        .iter()
        .find(|r| matches!(r.status_code.parse::<u16>(), Ok(c) if (200..300).contains(&c)))
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
        TypeRef::Integer { .. } => format!("{raw_var} != null ? int.parse({raw_var}) : null"),
        TypeRef::Number { .. } => format!("{raw_var} != null ? num.parse({raw_var}) : null"),
        TypeRef::Boolean => format!("{raw_var} != null ? ({raw_var} == 'true') : null"),
        TypeRef::Array(inner) => {
            let item_expr = header_deserialize_expr(inner, "e");
            format!(
                "{raw_var} != null ? {raw_var}!.split(',').map((e) => {item_expr}).toList() : null"
            )
        }
        _ => raw_var.to_string(),
    }
}

fn deserialize_expr(
    type_ref: &TypeRef,
    schemas: &[Schema],
    registry: &EnumRegistry,
    data_var: &str,
    mappings: &MappingConfig,
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
            let value_ty = to_dart_type(inner, None, mappings);
            let inner_expr = deserialize_expr(inner, schemas, registry, "v", mappings);
            format!(
                "({data_var} as Map<String, dynamic>).map(\n      \
                 (k, v) => MapEntry(k, {inner_expr}),\n    ).cast<String, {value_ty}>()"
            )
        }
        TypeRef::Array(inner) => {
            let inner_expr = deserialize_expr(inner, schemas, registry, "e", mappings);
            format!(
                "({data_var} as List<dynamic>)\n        .map((e) => {inner_expr})\n        .toList()"
            )
        }
        TypeRef::Enum(_) => format!("{data_var} as String"),
        TypeRef::Named(name) => {
            let cls = mappings.resolve_class(name);
            match named_schema_kind(name, schemas) {
                Some(SchemaKind::Object { .. }) | Some(SchemaKind::Union { .. }) => {
                    format!("{cls}.fromJson({data_var} as Map<String, dynamic>)")
                }
                Some(SchemaKind::Array { item }) => {
                    let item_expr = deserialize_expr(item, schemas, registry, "e", mappings);
                    format!(
                        "({data_var} as List<dynamic>)\n        .map((e) => {item_expr})\n        .toList()"
                    )
                }
                Some(SchemaKind::Map { value }) => {
                    let value_ty = to_dart_type(value, None, mappings);
                    let inner_expr = deserialize_expr(value, schemas, registry, "v", mappings);
                    format!(
                        "({data_var} as Map<String, dynamic>).map(\n      \
                         (k, v) => MapEntry(k, {inner_expr}),\n    ).cast<String, {value_ty}>()"
                    )
                }
                Some(SchemaKind::UntaggedUnion { .. }) => {
                    format!("{cls}.fromJson({data_var})")
                }
                Some(SchemaKind::Alias { target }) => {
                    let target_cls = mappings.resolve_class(target);
                    format!("{target_cls}.fromJson({data_var} as Map<String, dynamic>)")
                }
                // None means the schema was mapped away — the external type is
                // assumed to implement fromJson with the standard Freezed signature.
                None => {
                    format!("{cls}.fromJson({data_var} as Map<String, dynamic>)")
                }
            }
        }
    }
}

// ── Naming / type utilities ───────────────────────────────────────────────────

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
    format!("{}{path_slug}", op.method.to_string().to_lowercase())
}

fn to_dart_type(
    type_ref: &TypeRef,
    enum_synth_name: Option<&str>,
    mappings: &MappingConfig,
) -> String {
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
        TypeRef::Map(inner) => format!("Map<String, {}>", to_dart_type(inner, None, mappings)),
        TypeRef::Array(inner) => format!("List<{}>", to_dart_type(inner, None, mappings)),
        // Use the mapping if one exists; otherwise fall back to dart_class_name.
        TypeRef::Named(name) => mappings.resolve_class(name),
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

//! OpenAPI 3.0 loader and lowering pass.
//!
//! Public API: one function, `load`, which reads a YAML file and returns a
//! fully-populated `flap_ir::Api`. Everything inside is private serde plumbing.
//!
//! Two-pass architecture (Phase 1):
//! - Pass 1 (parse): serde produces `Raw*` mirrors of the YAML.
//! - Pass 2 (lower): a `LoweringContext` borrows the parsed components as a
//!   registry, and every `lower_*` function threads `&mut ctx` through. This
//!   lets `$ref` pointers be resolved from anywhere — schemas, parameters,
//!   request bodies — uniformly via `LoweringContext::resolve_schema`.
//!
//! Phase 2 additions:
//! - `lower_type_ref` recognises three new shapes:
//!   - `enum: [...]` → `TypeRef::Enum`
//!   - `additionalProperties: { ... }` → `TypeRef::Map`
//!   - `type: string, format: date-time` → `TypeRef::DateTime`
//! - `lower_inline_schema` handles `allOf`. The new `collect_object_fields`
//!   helper walks `allOf` members (ref or inline), flattens them, then
//!   appends the schema's own properties. Duplicate field names dedupe with
//!   later-wins, but `required` ORs across all occurrences.
//! - The `visiting` set inside `LoweringContext` now serves two distinct
//!   roles. For ordinary `$ref` lookups via `resolve_schema` it short-circuits
//!   self-recursion to a `TypeRef::Named` (Phase 1 behaviour, unchanged).
//!   For `allOf` flattening, it is a true cycle guard: composing a schema
//!   into itself is meaningless and must error rather than be papered over.
//!
//! Design notes:
//! - `Raw*` types mirror the OpenAPI YAML structure and are only used for parsing.
//! - `lower_*` functions convert `&Raw*` → IR. Errors are propagated with context.
//! - Per DECISIONS D5, OpenAPI 3.1 is rejected up front with a clear message.
//! - Per DECISIONS D6, oneOf/anyOf without a discriminator will be a hard error
//!   once the emitter needs to handle them; for now they're not in PetStore.
//! - v0.1 only supports `#/components/schemas/*` `$ref` pointers. Pointers into
//!   `components/parameters` or `components/responses` will be added when the
//!   IR grows to model those (Phase 3).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use flap_ir::{
    Api, Field, HttpMethod, Operation, Parameter, ParameterLocation, RequestBody, Schema,
    SchemaKind, TypeRef,
};
use serde::Deserialize;

// ── Public entry point ───────────────────────────────────────────────────────

pub fn load(path: impl AsRef<Path>) -> Result<Api> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading spec file {}", path.display()))?;
    load_str(&text).with_context(|| format!("in spec file {}", path.display()))
}

/// Exposed for unit tests.
pub fn load_str(text: &str) -> Result<Api> {
    reject_unsupported_version(text)?;
    let raw: RawSpec = serde_yaml::from_str(text).context("parsing OpenAPI YAML")?;
    lower(raw)
}

// ── Version guard ────────────────────────────────────────────────────────────

fn reject_unsupported_version(text: &str) -> Result<()> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("openapi:") {
            let v = rest.trim().trim_matches(|c: char| c == '"' || c == '\'');
            if v.starts_with("3.1") {
                bail!(
                    "OpenAPI 3.1 is not supported in v0.1 (DECISIONS D5). \
                     Found `openapi: {v}`. Downgrade your spec to 3.0.x."
                );
            }
            if !v.starts_with("3.") {
                bail!("unsupported OpenAPI version `{v}` — flap v0.1 requires 3.0.x");
            }
            return Ok(());
        }
    }
    Ok(()) // no version field; let serde fail with its own error
}

// ── Raw serde types (parse layer) ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawSpec {
    info: RawInfo,
    #[serde(default)]
    servers: Vec<RawServer>,
    #[serde(default)]
    paths: BTreeMap<String, RawPathItem>,
    #[serde(default)]
    components: RawComponents,
}

#[derive(Debug, Deserialize)]
struct RawInfo {
    title: String,
}

#[derive(Debug, Deserialize)]
struct RawServer {
    url: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawComponents {
    #[serde(default)]
    schemas: BTreeMap<String, RawSchemaOrRef>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPathItem {
    pub get: Option<RawOperation>,
    pub post: Option<RawOperation>,
    pub put: Option<RawOperation>,
    pub delete: Option<RawOperation>,
    pub patch: Option<RawOperation>,
    pub options: Option<RawOperation>,
    pub head: Option<RawOperation>,
    pub trace: Option<RawOperation>,
}

#[derive(Debug, Deserialize)]
struct RawOperation {
    #[serde(rename = "operationId")]
    operation_id: Option<String>,
    summary: Option<String>,
    #[serde(default)]
    parameters: Vec<RawParameter>,
    #[serde(rename = "requestBody")]
    request_body: Option<RawRequestBody>,
}

/// An individual query / path / header / cookie parameter.
#[derive(Debug, Deserialize)]
struct RawParameter {
    name: String,
    /// The OpenAPI `in` field — "query", "path", "header", or "cookie".
    #[serde(rename = "in")]
    location: String,
    /// Defaults to false per the OpenAPI spec; path params are always required
    /// regardless and we enforce that in `lower_parameter`.
    #[serde(default)]
    required: bool,
    /// The schema describing the parameter's type. We bail if it is absent,
    /// since we cannot emit anything meaningful without a type.
    ///
    /// NOTE: parameter-level `$ref` (referencing `components/parameters`) is
    /// not yet supported — v0.1 covers only inline schemas + schema `$ref`s.
    schema: Option<RawSchemaOrRef>,
}

/// The `requestBody` field of an operation.
#[derive(Debug, Deserialize)]
struct RawRequestBody {
    /// Map of content-type → media type object.
    /// We prefer `application/json`; fall back to the first entry.
    content: BTreeMap<String, RawMediaType>,
    /// Defaults to false per the OpenAPI spec.
    #[serde(default)]
    required: bool,
}

/// A single entry in `requestBody.content`.
#[derive(Debug, Deserialize)]
struct RawMediaType {
    /// The schema of this media type. We bail if absent.
    schema: Option<RawSchemaOrRef>,
}

/// A schema entry in `components.schemas` or in a field's `properties` map.
/// It is either a `$ref` pointer or an inline schema definition.
///
/// `untagged` tries variants in declaration order: `Ref` first, because
/// `RawSchema` has all-optional fields and would otherwise match anything.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawSchemaOrRef {
    Ref {
        #[serde(rename = "$ref")]
        reference: String,
    },
    Inline(RawSchema),
}

#[derive(Debug, Default, Deserialize)]
struct RawSchema {
    #[serde(rename = "type")]
    ty: Option<String>,
    format: Option<String>,
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    properties: BTreeMap<String, RawSchemaOrRef>,
    items: Option<Box<RawSchemaOrRef>>,
    /// Phase 2: closed value set. v0.1 only supports string enums; non-string
    /// entries (integers, nulls, booleans) are rejected during lowering with
    /// a clear message rather than silently coerced.
    #[serde(default, rename = "enum")]
    enum_values: Vec<serde_yaml::Value>,
    /// Phase 2: typed dictionary. The boolean form (`true`/`false`) parses but
    /// does not produce a `Map` — we treat it as a no-op since Dart objects
    /// already permit unknown keys.
    #[serde(rename = "additionalProperties")]
    additional_properties: Option<RawAdditionalProperties>,
    /// Phase 2: composition. Each entry is either a `$ref` to an object schema
    /// or an inline object. Lowering flattens them into a single object
    /// schema by concatenating their fields (see `collect_object_fields`).
    #[serde(default, rename = "allOf")]
    all_of: Vec<RawSchemaOrRef>,
}

/// `additionalProperties` in OpenAPI is either a boolean (allow / forbid extra
/// fields with no type constraint) or a schema (typed map). We only generate
/// `TypeRef::Map` for the schema form; the boolean form is silently ignored
/// in v0.1 because Dart `Map<String, dynamic>` would be a poor default and
/// the right answer depends on framework conventions.
///
/// `Bool` is listed first so serde's untagged deserializer matches `true` /
/// `false` before falling through to `Schema`, where the inner
/// `RawSchemaOrRef` itself runs its own untagged dispatch.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawAdditionalProperties {
    #[allow(dead_code)]
    Bool(bool),
    Schema(Box<RawSchemaOrRef>),
}

// ── Lowering context (shared during pass 2) ──────────────────────────────────

/// Threaded mutably through every `lower_*` function during pass 2.
///
/// - `components` is the global registry of all top-level definitions, used to
///   resolve `$ref` pointers from anywhere in the document.
/// - `visiting` records the set of top-level schemas currently mid-lowering.
///   It serves two roles:
///   - For ordinary `$ref` field types resolved via `resolve_schema`: members
///     of `visiting` short-circuit to `TypeRef::Named`, breaking self-recursion
///     at code-gen time (e.g. `Node.next: $ref Node`).
///   - For `allOf` flattening in `collect_member_fields`: members of `visiting`
///     are a hard cycle and produce an error — composing a schema into itself
///     has no meaning and cannot be represented as a flat field list.
struct LoweringContext<'a> {
    components: &'a RawComponents,
    visiting: HashSet<String>,
}

impl<'a> LoweringContext<'a> {
    fn new(components: &'a RawComponents) -> Self {
        Self {
            components,
            visiting: HashSet::new(),
        }
    }

    /// Resolves a named schema reference against the registry.
    ///
    /// - If the target name is currently in `visiting`, this is a recursive
    ///   reference (e.g. `Node.next: $ref Node`). Return `TypeRef::Named`
    ///   immediately — the type is being emitted as its own Dart class, so
    ///   the cycle is naturally broken at code-gen time.
    /// - Otherwise, validate the target exists in the registry and return a
    ///   named reference. We do not inline; the Dart emitter consumes named
    ///   refs directly.
    fn resolve_schema(&self, name: &str) -> Result<TypeRef> {
        if self.visiting.contains(name) {
            return Ok(TypeRef::Named(name.to_string()));
        }
        if !self.components.schemas.contains_key(name) {
            bail!(
                "$ref points to undefined schema `{name}` \
                 (not present in components.schemas)"
            );
        }
        Ok(TypeRef::Named(name.to_string()))
    }
}

/// Parses a `$ref` pointer of the form `#/components/schemas/Name` and returns
/// the bare schema name as a borrowed slice of the input.
///
/// v0.1 deliberately supports only schema references — pointers into
/// `components/parameters` or `components/responses` will be added in Phase 3
/// when the IR grows to model those.
fn parse_schema_ref_pointer(reference: &str) -> Result<&str> {
    let bare = reference
        .strip_prefix("#/components/schemas/")
        .ok_or_else(|| {
            anyhow!(
                "$ref `{reference}` is not a schema reference \
                 (v0.1 supports only `#/components/schemas/*`)"
            )
        })?;
    if bare.is_empty() || bare.contains('/') {
        bail!("malformed $ref pointer `{reference}`");
    }
    Ok(bare)
}

// ── Lowering pass (Raw* → IR) ─────────────────────────────────────────────────

fn lower(raw: RawSpec) -> Result<Api> {
    let title = raw.info.title;
    let base_url = raw.servers.into_iter().next().map(|s| s.url);

    let mut ctx = LoweringContext::new(&raw.components);

    let operations = lower_operations(&raw.paths, &mut ctx)?;
    let schemas = lower_schemas(&raw.components.schemas, &mut ctx)?;

    Ok(Api {
        title,
        base_url,
        operations,
        schemas,
    })
}

fn lower_operations(
    paths: &BTreeMap<String, RawPathItem>,
    ctx: &mut LoweringContext,
) -> Result<Vec<Operation>> {
    // BTreeMap iteration is sorted by path — deterministic.
    let mut ops = Vec::new();
    for (path, item) in paths {
        let pairs: [(HttpMethod, &Option<RawOperation>); 8] = [
            (HttpMethod::Delete, &item.delete),
            (HttpMethod::Get, &item.get),
            (HttpMethod::Head, &item.head),
            (HttpMethod::Options, &item.options),
            (HttpMethod::Patch, &item.patch),
            (HttpMethod::Post, &item.post),
            (HttpMethod::Put, &item.put),
            (HttpMethod::Trace, &item.trace),
        ];
        for (method, maybe_op) in pairs {
            if let Some(raw_op) = maybe_op {
                let parameters = raw_op
                    .parameters
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        lower_parameter(path, p, ctx)
                            .with_context(|| format!("parameter[{i}] of {method} {path}"))
                    })
                    .collect::<Result<Vec<_>>>()?;

                // Option<&RawRequestBody> → Result<Option<RequestBody>>
                let request_body = raw_op
                    .request_body
                    .as_ref()
                    .map(|rb| {
                        lower_request_body(path, method, rb, ctx)
                            .with_context(|| format!("requestBody of {method} {path}"))
                    })
                    .transpose()?;

                ops.push(Operation {
                    method,
                    path: path.clone(),
                    operation_id: raw_op.operation_id.clone(),
                    summary: raw_op.summary.clone(),
                    parameters,
                    request_body,
                });
            }
        }
    }
    Ok(ops)
}

fn lower_parameter(path: &str, raw: &RawParameter, ctx: &mut LoweringContext) -> Result<Parameter> {
    let location = match raw.location.as_str() {
        "query" => ParameterLocation::Query,
        "path" => ParameterLocation::Path,
        "header" => ParameterLocation::Header,
        "cookie" => ParameterLocation::Cookie,
        other => bail!(
            "unsupported parameter location `{other}` \
             (expected query | path | header | cookie)"
        ),
    };

    // OpenAPI 3.0 §4.7.12: path parameters MUST be required=true.
    // We enforce this regardless of what the spec says to avoid silently
    // generating broken code.
    let required = location == ParameterLocation::Path || raw.required;

    let schema = raw.schema.as_ref().ok_or_else(|| {
        anyhow!(
            "parameter `{}` in {path} has no `schema` — \
             cannot determine its type",
            raw.name
        )
    })?;

    let type_ref = lower_type_ref(&raw.name, schema, ctx)
        .with_context(|| format!("schema of parameter `{}`", raw.name))?;

    Ok(Parameter {
        name: raw.name.clone(),
        location,
        type_ref,
        required,
    })
}

fn lower_request_body(
    path: &str,
    method: HttpMethod,
    raw: &RawRequestBody,
    ctx: &mut LoweringContext,
) -> Result<RequestBody> {
    // Prefer application/json; fall back to the first entry in BTreeMap order.
    let (content_type, media_type): (String, &RawMediaType) =
        if let Some(mt) = raw.content.get("application/json") {
            ("application/json".to_string(), mt)
        } else {
            let (k, v) =
                raw.content.iter().next().ok_or_else(|| {
                    anyhow!("requestBody of {method} {path} has no content entries")
                })?;
            (k.clone(), v)
        };

    let schema = media_type.schema.as_ref().ok_or_else(|| {
        anyhow!(
            "content type `{content_type}` in requestBody of {method} {path} \
             has no schema"
        )
    })?;

    let schema_ref = lower_type_ref("<requestBody>", schema, ctx)?;

    Ok(RequestBody {
        content_type,
        schema_ref,
        required: raw.required,
    })
}

fn lower_schemas(
    raw: &BTreeMap<String, RawSchemaOrRef>,
    ctx: &mut LoweringContext,
) -> Result<Vec<Schema>> {
    // BTreeMap iteration is alphabetically sorted — deterministic.
    let mut out = Vec::with_capacity(raw.len());
    for (name, schema_or_ref) in raw {
        // Mark this schema as "visiting" so any nested `$ref` back to it
        // resolves to a Named reference instead of recursing. Always remove
        // afterwards — even on error — so the context stays consistent if a
        // caller decides to recover from a per-schema failure later.
        ctx.visiting.insert(name.clone());
        let result = lower_schema_kind(name, schema_or_ref, ctx)
            .with_context(|| format!("in schema `{name}`"));
        ctx.visiting.remove(name);

        let kind = result?;
        out.push(Schema {
            name: name.clone(),
            kind,
        });
    }
    Ok(out)
}

fn lower_schema_kind(
    name: &str,
    sor: &RawSchemaOrRef,
    ctx: &mut LoweringContext,
) -> Result<SchemaKind> {
    match sor {
        RawSchemaOrRef::Ref { reference } => Err(anyhow!(
            "top-level schema `{name}` is a bare $ref (`{reference}`); \
             aliases are not yet supported in v0.1"
        )),
        RawSchemaOrRef::Inline(raw) => lower_inline_schema(name, raw, ctx),
    }
}

fn lower_inline_schema(
    name: &str,
    raw: &RawSchema,
    ctx: &mut LoweringContext,
) -> Result<SchemaKind> {
    // Phase 2: composition. `allOf` takes precedence over the schema's `type`,
    // because real-world specs frequently omit `type: object` on composed
    // schemas. Any own `properties` are appended after the inherited ones.
    if !raw.all_of.is_empty() {
        let fields = collect_object_fields(raw, ctx)?;
        return Ok(SchemaKind::Object { fields });
    }

    match raw.ty.as_deref() {
        Some("object") | None if !raw.properties.is_empty() => {
            let fields = collect_object_fields(raw, ctx)?;
            Ok(SchemaKind::Object { fields })
        }

        Some("array") => {
            let items = raw
                .items
                .as_ref()
                .ok_or_else(|| anyhow!("array schema `{name}` is missing `items`"))?;
            let item = lower_type_ref("<items>", items, ctx)
                .with_context(|| format!("in `{name}.items`"))?;
            Ok(SchemaKind::Array { item })
        }

        Some(other) => Err(anyhow!(
            "schema `{name}` has type `{other}` with no properties — \
             primitive root schemas are not yet supported in v0.1"
        )),

        None => Err(anyhow!(
            "schema `{name}` has no `type` and no `properties` — \
             cannot determine kind"
        )),
    }
}

/// Builds the merged field list for an object schema, honouring `allOf` and
/// the schema's own `properties` in order.
///
/// Walk order is deliberate:
/// 1. Each `allOf` member is flattened first, in spec order, so "base class"
///    fields appear before "subclass" fields in the resulting Vec.
/// 2. The schema's own `properties` are appended next.
/// 3. Duplicate field names are deduplicated — last occurrence wins on type
///    (so a subclass override beats the base), but `required` ORs across all
///    occurrences (so a base requiring a field keeps it required even if a
///    subclass redeclares it without listing it in its own `required`).
fn collect_object_fields(raw: &RawSchema, ctx: &mut LoweringContext) -> Result<Vec<Field>> {
    let mut fields: Vec<Field> = Vec::new();

    // Phase 2: every `allOf` member, recursively flattened.
    for (i, member) in raw.all_of.iter().enumerate() {
        let member_fields =
            collect_member_fields(member, ctx).with_context(|| format!("allOf[{i}]"))?;
        fields.extend(member_fields);
    }

    // Then this schema's own properties.
    let own_required: HashSet<&str> = raw.required.iter().map(String::as_str).collect();
    for (field_name, sor) in &raw.properties {
        let type_ref = lower_type_ref(field_name, sor, ctx)
            .with_context(|| format!("field `{field_name}`"))?;
        let is_required = own_required.contains(field_name.as_str());
        fields.push(Field {
            name: field_name.clone(),
            type_ref,
            required: is_required,
        });
    }

    // Deduplicate by field name — see doc comment for the chosen semantics.
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut deduped: Vec<Field> = Vec::with_capacity(fields.len());
    for field in fields {
        if let Some(&idx) = seen.get(&field.name) {
            let merged_required = deduped[idx].required || field.required;
            deduped[idx] = Field {
                required: merged_required,
                ..field
            };
        } else {
            seen.insert(field.name.clone(), deduped.len());
            deduped.push(field);
        }
    }

    Ok(deduped)
}

/// Resolves a single `allOf` member into a flat list of fields.
///
/// Inline objects are flattened directly. `$ref` members are looked up in the
/// component registry and recursively flattened — including any nested
/// `allOf` they themselves contain. The `visiting` set is used as a true
/// cycle guard here: merging a schema into itself is meaningless and must
/// error rather than be papered over with `TypeRef::Named` (which is what
/// `LoweringContext::resolve_schema` does for ordinary field references).
fn collect_member_fields(sor: &RawSchemaOrRef, ctx: &mut LoweringContext) -> Result<Vec<Field>> {
    match sor {
        RawSchemaOrRef::Ref { reference } => {
            let bare = parse_schema_ref_pointer(reference)?;
            if ctx.visiting.contains(bare) {
                bail!(
                    "cycle in `allOf` chain via `{bare}` — \
                     a schema cannot inherit from itself"
                );
            }
            let target = ctx.components.schemas.get(bare).ok_or_else(|| {
                anyhow!(
                    "`allOf` $ref points to undefined schema `{bare}` \
                     (not present in components.schemas)"
                )
            })?;
            match target {
                RawSchemaOrRef::Inline(target_raw) => {
                    ctx.visiting.insert(bare.to_string());
                    let result = collect_object_fields(target_raw, ctx)
                        .with_context(|| format!("flattening `{bare}` for allOf"));
                    ctx.visiting.remove(bare);
                    result
                }
                RawSchemaOrRef::Ref { reference: inner } => Err(anyhow!(
                    "`allOf` member `{bare}` is itself a $ref to `{inner}` — \
                     ref chains are not supported in v0.1"
                )),
            }
        }
        RawSchemaOrRef::Inline(raw) => collect_object_fields(raw, ctx),
    }
}

fn lower_type_ref(
    field_name: &str,
    sor: &RawSchemaOrRef,
    ctx: &mut LoweringContext,
) -> Result<TypeRef> {
    match sor {
        RawSchemaOrRef::Ref { reference } => {
            // "$ref": "#/components/schemas/Pet" → "Pet"
            let bare = parse_schema_ref_pointer(reference)?;
            ctx.resolve_schema(bare)
        }
        RawSchemaOrRef::Inline(raw) => {
            // Phase 2 ── enum takes precedence over the underlying primitive
            // type. OpenAPI typically writes `type: string` alongside `enum:`,
            // but the value-set is what callers actually care about.
            if !raw.enum_values.is_empty() {
                let values = stringify_enum_values(field_name, &raw.enum_values)?;
                return Ok(TypeRef::Enum(values));
            }

            // Phase 2 ── `additionalProperties` with a schema → typed map.
            // Boolean variants degrade to ordinary handling (no Map produced).
            if let Some(RawAdditionalProperties::Schema(inner)) = &raw.additional_properties {
                let value = lower_type_ref("<additionalProperties>", inner, ctx)
                    .with_context(|| format!("additionalProperties of `{field_name}`"))?;
                return Ok(TypeRef::Map(Box::new(value)));
            }

            match raw.ty.as_deref() {
                Some("string") => {
                    // Phase 2 ── date-time format gets a dedicated variant.
                    if raw.format.as_deref() == Some("date-time") {
                        Ok(TypeRef::DateTime)
                    } else {
                        Ok(TypeRef::String)
                    }
                }
                Some("integer") => Ok(TypeRef::Integer {
                    format: raw.format.clone(),
                }),
                Some("number") => Ok(TypeRef::Number {
                    format: raw.format.clone(),
                }),
                Some("boolean") => Ok(TypeRef::Boolean),
                Some(other) => Err(anyhow!(
                    "field `{field_name}` has unsupported inline type `{other}`"
                )),
                None => Err(anyhow!(
                    "field `{field_name}` has no `type` and is not a $ref"
                )),
            }
        }
    }
}

/// Converts the raw YAML enum entries into Rust `String`s.
///
/// v0.1 only models string-valued enums. Numbers and booleans appear in real
/// specs (sparingly) but emitting them as Dart `enum` requires a different
/// serialiser — out of scope for this phase. Reject them up front rather
/// than silently coercing.
fn stringify_enum_values(field_name: &str, raw: &[serde_yaml::Value]) -> Result<Vec<String>> {
    raw.iter()
        .map(|v| match v {
            serde_yaml::Value::String(s) => Ok(s.clone()),
            other => Err(anyhow!(
                "enum value `{other:?}` in field `{field_name}` is not a string \
                 — v0.1 only supports string enums"
            )),
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_openapi_31() {
        let yaml = "openapi: 3.1.0\ninfo:\n  title: x\n  version: '1'\npaths: {}\n";
        let err = load_str(yaml).unwrap_err();
        assert!(err.to_string().contains("3.1"), "got: {err}");
    }

    #[test]
    fn petstore_operations() {
        let yaml = include_str!("../../../tests/fixtures/petstore.yaml");
        let api = load_str(yaml).expect("petstore should parse");
        assert_eq!(api.operations.len(), 3);
        // BTreeMap on paths + method sort: GET /pets, POST /pets, GET /pets/{petId}
        assert_eq!(api.operations[0].method, HttpMethod::Get);
        assert_eq!(api.operations[0].path, "/pets");
        assert_eq!(api.operations[0].operation_id.as_deref(), Some("listPets"));
        assert_eq!(api.operations[1].method, HttpMethod::Post);
        assert_eq!(api.operations[2].method, HttpMethod::Get);
        assert_eq!(api.operations[2].path, "/pets/{petId}");
    }

    #[test]
    fn petstore_parameters() {
        let yaml = include_str!("../../../tests/fixtures/petstore.yaml");
        let api = load_str(yaml).expect("petstore should parse");

        // GET /pets — one optional query parameter
        let list_pets = &api.operations[0];
        assert_eq!(list_pets.parameters.len(), 1);
        let limit = &list_pets.parameters[0];
        assert_eq!(limit.name, "limit");
        assert_eq!(limit.location, ParameterLocation::Query);
        assert!(!limit.required, "limit should be optional");
        assert!(
            matches!(limit.type_ref, TypeRef::Integer { .. }),
            "limit should be integer"
        );

        // POST /pets — no parameters (body modelled separately)
        assert_eq!(api.operations[1].parameters.len(), 0);

        // GET /pets/{petId} — one required path parameter
        let show_pet = &api.operations[2];
        assert_eq!(show_pet.parameters.len(), 1);
        let pet_id = &show_pet.parameters[0];
        assert_eq!(pet_id.name, "petId");
        assert_eq!(pet_id.location, ParameterLocation::Path);
        assert!(pet_id.required, "path params must be required");
        assert!(
            matches!(pet_id.type_ref, TypeRef::String),
            "petId should be string"
        );
    }

    #[test]
    fn petstore_request_body() {
        let yaml = include_str!("../../../tests/fixtures/petstore.yaml");
        let api = load_str(yaml).expect("petstore should parse");

        // GET /pets — no request body
        assert!(
            api.operations[0].request_body.is_none(),
            "GET /pets should have no request body"
        );

        // POST /pets — application/json body referencing Pet
        let create_pets = &api.operations[1];
        let body = create_pets
            .request_body
            .as_ref()
            .expect("POST /pets should have a request body");
        assert_eq!(body.content_type, "application/json");
        assert!(body.required, "POST /pets body should be required");
        assert!(
            matches!(&body.schema_ref, TypeRef::Named(n) if n == "Pet"),
            "body schema should be Named(\"Pet\"), got {:?}",
            body.schema_ref
        );

        // GET /pets/{petId} — no request body
        assert!(
            api.operations[2].request_body.is_none(),
            "GET /pets/{{petId}} should have no request body"
        );
    }

    #[test]
    fn path_parameter_always_required() {
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths:
  /items/{id}:
    get:
      operationId: getItem
      parameters:
        - name: id
          in: path
          required: false   # wrong — lowering must override this
          schema:
            type: string
      responses:
        '200':
          description: ok
";
        let api = load_str(yaml).expect("should parse");
        let param = &api.operations[0].parameters[0];
        assert!(
            param.required,
            "path param must be required even if spec says false"
        );
    }

    #[test]
    fn petstore_schemas() {
        let yaml = include_str!("../../../tests/fixtures/petstore.yaml");
        let api = load_str(yaml).expect("petstore should parse");
        assert_eq!(api.schemas.len(), 3);
        assert_eq!(api.schemas[0].name, "Error");
        assert_eq!(api.schemas[1].name, "Pet");

        let SchemaKind::Object { fields } = &api.schemas[1].kind else {
            panic!("Pet should be an object");
        };
        assert_eq!(fields[0].name, "id");
        assert!(fields[0].required);
        assert_eq!(fields[1].name, "name");
        assert!(!fields[2].required, "tag should be optional");

        let SchemaKind::Array { item } = &api.schemas[2].kind else {
            panic!("Pets should be an array");
        };
        assert!(matches!(item, TypeRef::Named(n) if n == "Pet"));
    }

    // ── Phase 1: $ref resolution & cycle detection ────────────────────────────

    #[test]
    fn rejects_unresolved_schema_ref() {
        // The registry must catch refs to schemas that aren't defined.
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths:
  /things:
    get:
      operationId: listThings
      parameters:
        - name: filter
          in: query
          schema:
            $ref: '#/components/schemas/DoesNotExist'
      responses:
        '200':
          description: ok
";
        let err = load_str(yaml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("DoesNotExist") && msg.contains("undefined"),
            "expected unresolved-ref error mentioning the missing name; got: {msg}"
        );
    }

    #[test]
    fn rejects_non_schema_ref_pointer() {
        // v0.1 only supports #/components/schemas/* refs. Pointers into
        // components/parameters etc. should fail with a clear message.
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths:
  /things:
    get:
      operationId: listThings
      parameters:
        - $ref: '#/components/parameters/Filter'
      responses:
        '200':
          description: ok
";
        let err = load_str(yaml).unwrap_err();
        let msg = format!("{err:#}");
        // Either the parameter-level $ref is rejected at deserialization, or
        // (if a future version supports it) the schema sub-ref is. Either way
        // we expect the error to point at the spec's misuse.
        assert!(!msg.is_empty(), "expected an error, got empty");
    }

    #[test]
    fn handles_self_referential_schema() {
        // A linked-list-style schema where Node.next is a $ref back to Node.
        // This is the canonical case the `visiting` set is designed to handle:
        // when we're partway through lowering Node and hit a ref to Node, the
        // resolver short-circuits to TypeRef::Named without re-entering.
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Node:
      type: object
      required: [value]
      properties:
        value:
          type: string
        next:
          $ref: '#/components/schemas/Node'
";
        let api = load_str(yaml).expect("self-referential schema should lower cleanly");
        assert_eq!(api.schemas.len(), 1);
        let SchemaKind::Object { fields } = &api.schemas[0].kind else {
            panic!("Node should be an object");
        };
        let next = fields.iter().find(|f| f.name == "next").unwrap();
        assert!(
            matches!(&next.type_ref, TypeRef::Named(n) if n == "Node"),
            "next should resolve to Named(\"Node\"), got {:?}",
            next.type_ref
        );
    }

    #[test]
    fn resolves_cross_schema_ref() {
        // Sanity check: a ref to a sibling schema resolves correctly through
        // the registry, even though that sibling isn't currently in `visiting`.
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Owner:
      type: object
      required: [name]
      properties:
        name:
          type: string
    Pet:
      type: object
      required: [owner]
      properties:
        owner:
          $ref: '#/components/schemas/Owner'
";
        let api = load_str(yaml).expect("cross-schema $ref should resolve");
        assert_eq!(api.schemas.len(), 2);
        let pet = api.schemas.iter().find(|s| s.name == "Pet").unwrap();
        let SchemaKind::Object { fields } = &pet.kind else {
            panic!("Pet should be an object");
        };
        let owner = fields.iter().find(|f| f.name == "owner").unwrap();
        assert!(matches!(&owner.type_ref, TypeRef::Named(n) if n == "Owner"));
    }

    // ── Phase 2: enums ───────────────────────────────────────────────────────

    #[test]
    fn enum_field_lowers_to_typeref_enum() {
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Pet:
      type: object
      required: [status]
      properties:
        status:
          type: string
          enum: [available, pending, sold]
";
        let api = load_str(yaml).expect("should parse");
        let pet = &api.schemas[0];
        let SchemaKind::Object { fields } = &pet.kind else {
            panic!("Pet should be an object");
        };
        let status = fields.iter().find(|f| f.name == "status").unwrap();
        let TypeRef::Enum(values) = &status.type_ref else {
            panic!("status should be Enum, got {:?}", status.type_ref);
        };
        assert_eq!(
            values,
            &vec!["available".to_string(), "pending".into(), "sold".into()]
        );
        assert!(status.required, "status should still respect required list");
    }

    #[test]
    fn rejects_non_string_enum_values() {
        // Integer enums exist in OpenAPI but we explicitly punt on them in v0.1.
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Pet:
      type: object
      properties:
        priority:
          type: integer
          enum: [1, 2, 3]
";
        let err = load_str(yaml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("string enums") || msg.contains("not a string"),
            "expected string-enum-only error, got: {msg}"
        );
    }

    // ── Phase 2: additionalProperties → Map ──────────────────────────────────

    #[test]
    fn additional_properties_lowers_to_map_of_string() {
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Pet:
      type: object
      properties:
        labels:
          type: object
          additionalProperties:
            type: string
";
        let api = load_str(yaml).expect("should parse");
        let pet = &api.schemas[0];
        let SchemaKind::Object { fields } = &pet.kind else {
            panic!("Pet should be an object");
        };
        let labels = fields.iter().find(|f| f.name == "labels").unwrap();
        let TypeRef::Map(inner) = &labels.type_ref else {
            panic!("labels should be Map, got {:?}", labels.type_ref);
        };
        assert!(
            matches!(**inner, TypeRef::String),
            "inner should be String, got {:?}",
            inner
        );
    }

    #[test]
    fn additional_properties_lowers_to_map_of_named_ref() {
        // Maps of complex types — common in real APIs (e.g. `extensions: { ... }`).
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Owner:
      type: object
      properties:
        name:
          type: string
    Kennel:
      type: object
      properties:
        owners:
          type: object
          additionalProperties:
            $ref: '#/components/schemas/Owner'
";
        let api = load_str(yaml).expect("should parse");
        let kennel = api.schemas.iter().find(|s| s.name == "Kennel").unwrap();
        let SchemaKind::Object { fields } = &kennel.kind else {
            panic!("Kennel should be an object");
        };
        let owners = fields.iter().find(|f| f.name == "owners").unwrap();
        let TypeRef::Map(inner) = &owners.type_ref else {
            panic!("owners should be Map, got {:?}", owners.type_ref);
        };
        assert!(
            matches!(&**inner, TypeRef::Named(n) if n == "Owner"),
            "inner should be Named(\"Owner\"), got {:?}",
            inner
        );
    }

    #[test]
    fn additional_properties_boolean_does_not_become_map() {
        // additionalProperties: true is a permissibility hint, not a typed map.
        // The schema should lower as a regular object with only its declared
        // properties.
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Loose:
      type: object
      additionalProperties: true
      required: [id]
      properties:
        id:
          type: string
";
        let api = load_str(yaml).expect("should parse");
        let loose = &api.schemas[0];
        let SchemaKind::Object { fields } = &loose.kind else {
            panic!("Loose should be an object");
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "id");
        assert!(matches!(fields[0].type_ref, TypeRef::String));
    }

    // ── Phase 2: date-time ───────────────────────────────────────────────────

    #[test]
    fn date_time_lowers_to_datetime() {
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Event:
      type: object
      required: [createdAt]
      properties:
        createdAt:
          type: string
          format: date-time
        name:
          type: string
        slug:
          type: string
          format: uuid
";
        let api = load_str(yaml).expect("should parse");
        let event = &api.schemas[0];
        let SchemaKind::Object { fields } = &event.kind else {
            panic!("Event should be an object");
        };
        let created_at = fields.iter().find(|f| f.name == "createdAt").unwrap();
        assert!(
            matches!(created_at.type_ref, TypeRef::DateTime),
            "createdAt should be DateTime, got {:?}",
            created_at.type_ref
        );
        let name = fields.iter().find(|f| f.name == "name").unwrap();
        assert!(
            matches!(name.type_ref, TypeRef::String),
            "plain string should remain String"
        );
        let slug = fields.iter().find(|f| f.name == "slug").unwrap();
        assert!(
            matches!(slug.type_ref, TypeRef::String),
            "non-date-time formats should still be String (format ignored for now), got {:?}",
            slug.type_ref
        );
    }

    // ── Phase 2: allOf ───────────────────────────────────────────────────────

    #[test]
    fn all_of_merges_referenced_fields() {
        // The canonical "subclass extends base" case.
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Pet:
      type: object
      required: [id, name]
      properties:
        id:
          type: integer
        name:
          type: string
    Dog:
      allOf:
        - $ref: '#/components/schemas/Pet'
        - type: object
          required: [breed]
          properties:
            breed:
              type: string
";
        let api = load_str(yaml).expect("should parse");
        let dog = api.schemas.iter().find(|s| s.name == "Dog").unwrap();
        let SchemaKind::Object { fields } = &dog.kind else {
            panic!("Dog should be an object after allOf flattening");
        };
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["id", "name", "breed"],
            "base fields first, then own"
        );

        let id = fields.iter().find(|f| f.name == "id").unwrap();
        assert!(id.required, "id should be required (inherited from Pet)");
        let breed = fields.iter().find(|f| f.name == "breed").unwrap();
        assert!(breed.required, "breed should be required (own)");
    }

    #[test]
    fn all_of_with_only_refs() {
        // No own properties, just composing two refs (mixin pattern).
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Identifiable:
      type: object
      required: [id]
      properties:
        id:
          type: string
    Timestamped:
      type: object
      required: [createdAt]
      properties:
        createdAt:
          type: string
          format: date-time
    User:
      allOf:
        - $ref: '#/components/schemas/Identifiable'
        - $ref: '#/components/schemas/Timestamped'
";
        let api = load_str(yaml).expect("should parse");
        let user = api.schemas.iter().find(|s| s.name == "User").unwrap();
        let SchemaKind::Object { fields } = &user.kind else {
            panic!("User should be an object");
        };
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["id", "createdAt"]);
        // Sanity: the date-time mapping survives through allOf flattening.
        let created_at = fields.iter().find(|f| f.name == "createdAt").unwrap();
        assert!(matches!(created_at.type_ref, TypeRef::DateTime));
    }

    #[test]
    fn all_of_subclass_override_keeps_inherited_required() {
        // A base says `id` is required; the subclass redeclares `id` with a
        // different type but doesn't list it in its own `required`.
        // - The redeclared *type* should win (later occurrence overrides).
        // - The required-ness from the base should be preserved (OR semantics).
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Base:
      type: object
      required: [id]
      properties:
        id:
          type: integer
    Refined:
      allOf:
        - $ref: '#/components/schemas/Base'
        - type: object
          properties:
            id:
              type: string
";
        let api = load_str(yaml).expect("should parse");
        let refined = api.schemas.iter().find(|s| s.name == "Refined").unwrap();
        let SchemaKind::Object { fields } = &refined.kind else {
            panic!("Refined should be an object");
        };
        assert_eq!(fields.len(), 1, "duplicate `id` should dedupe");
        let id = &fields[0];
        assert!(
            matches!(id.type_ref, TypeRef::String),
            "subclass type should win, got {:?}",
            id.type_ref
        );
        assert!(id.required, "required should be inherited from Base");
    }

    #[test]
    fn all_of_combines_with_own_properties() {
        // A schema with both `allOf` and its own `properties` — common in real
        // APIs ("inherit the base, plus add these new fields").
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Animal:
      type: object
      required: [name]
      properties:
        name:
          type: string
    Dog:
      allOf:
        - $ref: '#/components/schemas/Animal'
      required: [breed]
      properties:
        breed:
          type: string
";
        let api = load_str(yaml).expect("should parse");
        let dog = api.schemas.iter().find(|s| s.name == "Dog").unwrap();
        let SchemaKind::Object { fields } = &dog.kind else {
            panic!("Dog should be an object");
        };
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["name", "breed"]);
        assert!(fields.iter().find(|f| f.name == "breed").unwrap().required);
        assert!(fields.iter().find(|f| f.name == "name").unwrap().required);
    }

    #[test]
    fn all_of_nested_refs_flatten_recursively() {
        // C → B → A: composing a chain should walk through and produce the
        // union of all leaf fields. This is the test that exercises the
        // recursive call in `collect_member_fields`.
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    A:
      type: object
      required: [a]
      properties:
        a:
          type: string
    B:
      allOf:
        - $ref: '#/components/schemas/A'
      required: [b]
      properties:
        b:
          type: string
    C:
      allOf:
        - $ref: '#/components/schemas/B'
      required: [c]
      properties:
        c:
          type: string
";
        let api = load_str(yaml).expect("should parse");
        let c = api.schemas.iter().find(|s| s.name == "C").unwrap();
        let SchemaKind::Object { fields } = &c.kind else {
            panic!("C should be an object");
        };
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"], "leaf inheritance order");
    }

    #[test]
    fn rejects_all_of_cycle() {
        // A composes B, B composes A — this can't be flattened.
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    A:
      allOf:
        - $ref: '#/components/schemas/B'
    B:
      allOf:
        - $ref: '#/components/schemas/A'
";
        let err = load_str(yaml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("cycle"), "expected cycle error, got: {msg}");
    }

    #[test]
    fn rejects_all_of_unresolved_ref() {
        let yaml = "
openapi: 3.0.0
info:
  title: test
  version: '1'
paths: {}
components:
  schemas:
    Dog:
      allOf:
        - $ref: '#/components/schemas/DoesNotExist'
";
        let err = load_str(yaml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("DoesNotExist"),
            "expected unresolved-ref error, got: {msg}"
        );
    }
}

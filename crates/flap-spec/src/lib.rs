//! OpenAPI 3.0 loader and lowering pass.
//!
//! Public API: one function, `load`, which reads a YAML file and returns a
//! fully-populated `flap_ir::Api`. Everything inside is private serde plumbing.
//!
//! # Phase 8 (nullability)
//!
//! `RawSchema.nullable` mirrors OpenAPI 3.0's `nullable: true` flag. It's
//! propagated into `Field.nullable` during `collect_object_fields`, where
//! it's also OR-merged across `allOf` parents (matching the merge rule
//! already used for `required` — any source declaring a wider semantics
//! wins, because narrowing later would silently drop wire forms a base
//! schema legitimately accepts).
//!
//! OpenAPI 3.1's `type: [string, "null"]` is intentionally not parsed
//! here. The `reject_unsupported_version` guard runs before serde, so a
//! 3.1 document never reaches this layer. When the 3.1 ban drops, the
//! type-array form will need translating into the same boolean before
//! reaching `collect_object_fields`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use flap_ir::{
    Api, ApiKeyLocation, Field, HttpMethod, Operation, Parameter, ParameterLocation, RequestBody,
    Response, Schema, SchemaKind, SecurityScheme, TypeRef,
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
    Ok(())
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
    #[serde(default)]
    security: Vec<BTreeMap<String, Vec<String>>>,
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
    #[serde(default, rename = "securitySchemes")]
    security_schemes: BTreeMap<String, RawSecurityScheme>,
}

#[derive(Debug, Deserialize)]
struct RawSecurityScheme {
    #[serde(rename = "type")]
    ty: String,
    name: Option<String>,
    #[serde(rename = "in")]
    location: Option<String>,
    scheme: Option<String>,
    #[serde(rename = "bearerFormat")]
    bearer_format: Option<String>,
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
    #[serde(default)]
    responses: BTreeMap<String, RawResponse>,
    security: Option<Vec<BTreeMap<String, Vec<String>>>>,
}

#[derive(Debug, Deserialize)]
struct RawParameter {
    name: String,
    #[serde(rename = "in")]
    location: String,
    #[serde(default)]
    required: bool,
    schema: Option<RawSchemaOrRef>,
}

#[derive(Debug, Deserialize)]
struct RawRequestBody {
    content: BTreeMap<String, RawMediaType>,
    #[serde(default)]
    required: bool,
}

#[derive(Debug, Deserialize)]
struct RawMediaType {
    schema: Option<RawSchemaOrRef>,
}

#[derive(Debug, Deserialize)]
struct RawResponse {
    #[allow(dead_code)]
    description: Option<String>,
    #[serde(default)]
    content: BTreeMap<String, RawMediaType>,
}

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
    #[serde(default, rename = "enum")]
    enum_values: Vec<serde_yaml::Value>,
    #[serde(rename = "additionalProperties")]
    additional_properties: Option<RawAdditionalProperties>,
    #[serde(default, rename = "allOf")]
    all_of: Vec<RawSchemaOrRef>,
    #[serde(default, rename = "oneOf")]
    one_of: Vec<RawSchemaOrRef>,
    discriminator: Option<RawDiscriminator>,
    /// OpenAPI 3.0 `nullable: true`. Defaults to false. v0.1 rejects 3.1
    /// up front (DECISIONS D5), so 3.1's type-array form
    /// (`type: [string, "null"]`) is intentionally not modelled here —
    /// the version guard runs before serde, so we'd never see one. When
    /// we drop the 3.1 ban, lowering will need a small pass to translate
    /// that shape into the same `nullable` boolean before reaching
    /// `collect_object_fields`.
    #[serde(default)]
    nullable: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawDiscriminator {
    #[serde(rename = "propertyName")]
    property_name: String,
    #[serde(default)]
    #[allow(dead_code)]
    mapping: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawAdditionalProperties {
    #[allow(dead_code)]
    Bool(bool),
    Schema(Box<RawSchemaOrRef>),
}

// ── Lowering context ─────────────────────────────────────────────────────────

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

    let security_schemes = lower_security_schemes(&raw.components.security_schemes)?;
    let security = flatten_security_requirements(&raw.security);

    Ok(Api {
        title,
        base_url,
        operations,
        schemas,
        security_schemes,
        security,
    })
}

// ── Security lowering ────────────────────────────────────────────────────────

fn lower_security_schemes(
    raw: &BTreeMap<String, RawSecurityScheme>,
) -> Result<Vec<SecurityScheme>> {
    let mut out = Vec::with_capacity(raw.len());
    for (name, scheme) in raw {
        let lowered = lower_security_scheme(name, scheme)
            .with_context(|| format!("in securityScheme `{name}`"))?;
        out.push(lowered);
    }
    Ok(out)
}

fn lower_security_scheme(name: &str, raw: &RawSecurityScheme) -> Result<SecurityScheme> {
    match raw.ty.as_str() {
        "apiKey" => {
            let parameter_name = raw.name.clone().ok_or_else(|| {
                anyhow!("apiKey security scheme is missing the required `name` field")
            })?;
            let location_str = raw.location.as_deref().ok_or_else(|| {
                anyhow!("apiKey security scheme is missing the required `in` field")
            })?;
            let location = match location_str {
                "header" => ApiKeyLocation::Header,
                "query" => ApiKeyLocation::Query,
                "cookie" => ApiKeyLocation::Cookie,
                other => bail!(
                    "apiKey `in: {other}` is invalid \
                     (expected `header`, `query`, or `cookie`)"
                ),
            };
            Ok(SecurityScheme::ApiKey {
                scheme_name: name.to_string(),
                parameter_name,
                location,
            })
        }
        "http" => {
            let scheme = raw.scheme.as_deref().unwrap_or("");
            if scheme.eq_ignore_ascii_case("bearer") {
                Ok(SecurityScheme::HttpBearer {
                    scheme_name: name.to_string(),
                    bearer_format: raw.bearer_format.clone(),
                })
            } else if scheme.is_empty() {
                bail!("http security scheme is missing the required `scheme` field")
            } else {
                bail!(
                    "http `scheme: {scheme}` is not supported in v0.1 \
                     (only `bearer` is implemented)"
                )
            }
        }
        "oauth2" | "openIdConnect" => bail!(
            "security scheme type `{}` is not supported in v0.1 \
             (apiKey and http-bearer only)",
            raw.ty
        ),
        other => bail!("unknown security scheme type `{other}`"),
    }
}

fn flatten_security_requirements(reqs: &[BTreeMap<String, Vec<String>>]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for req in reqs {
        for name in req.keys() {
            if seen.insert(name.clone()) {
                out.push(name.clone());
            }
        }
    }
    out
}

// ── Operation lowering ───────────────────────────────────────────────────────

fn lower_operations(
    paths: &BTreeMap<String, RawPathItem>,
    ctx: &mut LoweringContext,
) -> Result<Vec<Operation>> {
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

                let request_body = raw_op
                    .request_body
                    .as_ref()
                    .map(|rb| {
                        lower_request_body(path, method, rb, ctx)
                            .with_context(|| format!("requestBody of {method} {path}"))
                    })
                    .transpose()?;

                let responses = lower_responses(path, method, &raw_op.responses, ctx)?;

                let security = raw_op
                    .security
                    .as_ref()
                    .map(|reqs| flatten_security_requirements(reqs));

                ops.push(Operation {
                    method,
                    path: path.clone(),
                    operation_id: raw_op.operation_id.clone(),
                    summary: raw_op.summary.clone(),
                    parameters,
                    request_body,
                    responses,
                    security,
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

fn pick_request_body_content<'a>(
    content: &'a BTreeMap<String, RawMediaType>,
) -> Option<(String, &'a RawMediaType, bool)> {
    if let Some(mt) = content.get("application/json") {
        return Some(("application/json".to_string(), mt, false));
    }
    if let Some(mt) = content.get("multipart/form-data") {
        return Some(("multipart/form-data".to_string(), mt, true));
    }
    content.iter().next().map(|(k, v)| (k.clone(), v, false))
}

fn lower_request_body(
    path: &str,
    method: HttpMethod,
    raw: &RawRequestBody,
    ctx: &mut LoweringContext,
) -> Result<RequestBody> {
    let (content_type, media_type, is_multipart) = pick_request_body_content(&raw.content)
        .ok_or_else(|| anyhow!("requestBody of {method} {path} has no content entries"))?;

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
        is_multipart,
    })
}

// ── Response lowering ────────────────────────────────────────────────────────

fn lower_responses(
    path: &str,
    method: HttpMethod,
    raw: &BTreeMap<String, RawResponse>,
    ctx: &mut LoweringContext,
) -> Result<Vec<Response>> {
    let mut keys: Vec<&String> = raw.keys().collect();
    keys.sort_by(|a, b| {
        let key = |s: &str| -> (u8, i64, String) {
            if s == "default" {
                (2, 0, String::new())
            } else if let Ok(n) = s.parse::<i64>() {
                (0, n, String::new())
            } else {
                (1, 0, s.to_string())
            }
        };
        key(a).cmp(&key(b))
    });

    let mut out = Vec::with_capacity(raw.len());
    for status_code in keys {
        let raw_resp = &raw[status_code];
        let response = lower_response(status_code, raw_resp, ctx)
            .with_context(|| format!("response `{status_code}` of {method} {path}"))?;
        out.push(response);
    }
    Ok(out)
}

fn lower_response(
    status_code: &str,
    raw: &RawResponse,
    ctx: &mut LoweringContext,
) -> Result<Response> {
    if raw.content.is_empty() {
        return Ok(Response {
            status_code: status_code.to_string(),
            schema_ref: None,
        });
    }

    let media_type = raw
        .content
        .get("application/json")
        .or_else(|| raw.content.values().next())
        .ok_or_else(|| anyhow!("response `{status_code}` has empty content map"))?;

    let schema_ref = match &media_type.schema {
        Some(sor) => Some(
            lower_type_ref("<response>", sor, ctx)
                .with_context(|| format!("schema of response `{status_code}`"))?,
        ),
        None => None,
    };

    Ok(Response {
        status_code: status_code.to_string(),
        schema_ref,
    })
}

// ── Schema lowering ──────────────────────────────────────────────────────────

fn lower_schemas(
    raw: &BTreeMap<String, RawSchemaOrRef>,
    ctx: &mut LoweringContext,
) -> Result<Vec<Schema>> {
    let mut out = Vec::with_capacity(raw.len());
    for (name, schema_or_ref) in raw {
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
    if !raw.one_of.is_empty() {
        return lower_one_of(name, raw, ctx);
    }

    if !raw.all_of.is_empty() {
        let fields = collect_object_fields(raw, ctx)?;
        return Ok(SchemaKind::Object { fields });
    }

    match raw.ty.as_deref() {
        Some("object") | None if !raw.properties.is_empty() => {
            let fields = collect_object_fields(raw, ctx)?;
            Ok(SchemaKind::Object { fields })
        }

        Some("object") | None
            if raw.properties.is_empty()
                && matches!(
                    &raw.additional_properties,
                    Some(RawAdditionalProperties::Schema(_))
                ) =>
        {
            let Some(RawAdditionalProperties::Schema(inner)) = &raw.additional_properties else {
                unreachable!("guarded by matches! above");
            };
            let value = lower_type_ref("<additionalProperties>", inner, ctx)
                .with_context(|| format!("in `{name}.additionalProperties`"))?;
            Ok(SchemaKind::Map { value })
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

/// Builds the merged field list for an object schema, honouring `allOf`
/// and the schema's own `properties` in order.
///
/// Walk order:
/// 1. Each `allOf` member is flattened first, in spec order.
/// 2. The schema's own `properties` are appended next.
/// 3. Duplicate field names are deduplicated — last occurrence wins on
///    type, but `required`, `nullable`, and `is_recursive` OR across all
///    occurrences. (For all three: any source declaring the wider
///    semantics wins, because narrowing later would silently drop wire
///    forms a base schema legitimately accepts.)
fn collect_object_fields(raw: &RawSchema, ctx: &mut LoweringContext) -> Result<Vec<Field>> {
    let mut fields: Vec<Field> = Vec::new();

    for (i, member) in raw.all_of.iter().enumerate() {
        let member_fields =
            collect_member_fields(member, ctx).with_context(|| format!("allOf[{i}]"))?;
        fields.extend(member_fields);
    }

    let own_required: HashSet<&str> = raw.required.iter().map(String::as_str).collect();
    for (field_name, sor) in &raw.properties {
        let type_ref = lower_type_ref(field_name, sor, ctx)
            .with_context(|| format!("field `{field_name}`"))?;
        let is_required = own_required.contains(field_name.as_str());

        // OpenAPI 3.0: `nullable` is only meaningful on inline schemas.
        // A bare `$ref` cannot carry a `nullable` flag — that's a 3.0
        // spec limitation, not ours; specs that need it use an `allOf`
        // wrapper around the ref.
        let is_nullable = match sor {
            RawSchemaOrRef::Inline(raw) => raw.nullable.unwrap_or(false),
            RawSchemaOrRef::Ref { .. } => false,
        };

        let is_recursive = type_ref_is_recursive(&type_ref, &ctx.visiting);
        fields.push(Field {
            name: field_name.clone(),
            type_ref,
            required: is_required,
            nullable: is_nullable,
            is_recursive,
        });
    }

    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut deduped: Vec<Field> = Vec::with_capacity(fields.len());
    for field in fields {
        if let Some(&idx) = seen.get(&field.name) {
            let merged_required = deduped[idx].required || field.required;
            let merged_nullable = deduped[idx].nullable || field.nullable;
            let merged_recursive = deduped[idx].is_recursive || field.is_recursive;
            deduped[idx] = Field {
                required: merged_required,
                nullable: merged_nullable,
                is_recursive: merged_recursive,
                ..field
            };
        } else {
            seen.insert(field.name.clone(), deduped.len());
            deduped.push(field);
        }
    }

    Ok(deduped)
}

fn lower_one_of(name: &str, raw: &RawSchema, ctx: &mut LoweringContext) -> Result<SchemaKind> {
    let discriminator = raw.discriminator.as_ref().ok_or_else(|| {
        anyhow!(
            "schema `{name}` uses `oneOf` without a `discriminator` block. \
             Per DECISIONS D6, only discriminated unions are supported in \
             v0.1 — add a `discriminator: {{ propertyName: <field> }}` \
             block alongside the `oneOf` list, or replace the `oneOf` \
             with an `allOf` composition if the schemas share a base."
        )
    })?;

    let property_name = discriminator.property_name.trim();
    if property_name.is_empty() {
        bail!(
            "schema `{name}` has a `discriminator` with an empty \
             `propertyName` — set it to the wire-side field whose value \
             selects the variant."
        );
    }

    let mut tag_by_schema: BTreeMap<String, String> = BTreeMap::new();
    for (wire_tag, schema_ref) in &discriminator.mapping {
        let bare = parse_mapping_target(schema_ref)
            .with_context(|| format!("discriminator mapping entry `{wire_tag}` of `{name}`"))?;
        tag_by_schema.insert(bare.to_string(), wire_tag.clone());
    }

    let mut variants = Vec::with_capacity(raw.one_of.len());
    let mut variant_tags = Vec::with_capacity(raw.one_of.len());

    for (i, member) in raw.one_of.iter().enumerate() {
        let (variant, variant_name) = match member {
            RawSchemaOrRef::Ref { reference } => {
                let bare = parse_schema_ref_pointer(reference)
                    .with_context(|| format!("oneOf[{i}] of `{name}`"))?;
                let resolved = ctx
                    .resolve_schema(bare)
                    .with_context(|| format!("oneOf[{i}] of `{name}` references `{bare}`"))?;
                (resolved, bare.to_string())
            }
            RawSchemaOrRef::Inline(_) => bail!(
                "oneOf[{i}] of `{name}` is an inline schema. \
                 v0.1 requires every `oneOf` variant to be a $ref into \
                 `components.schemas` so each variant has a stable class \
                 name. Lift the inline schema into a named component."
            ),
        };

        let wire_tag = tag_by_schema
            .get(&variant_name)
            .cloned()
            .unwrap_or_else(|| variant_name.clone());

        variants.push(variant);
        variant_tags.push(wire_tag);
    }

    Ok(SchemaKind::Union {
        variants,
        discriminator: property_name.to_string(),
        variant_tags,
    })
}

fn parse_mapping_target(value: &str) -> Result<&str> {
    if value.starts_with("#/") {
        return parse_schema_ref_pointer(value);
    }
    if value.is_empty() || value.contains('/') {
        bail!("malformed mapping target `{value}` — expected schema name or $ref");
    }
    Ok(value)
}

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
            let bare = parse_schema_ref_pointer(reference)?;
            ctx.resolve_schema(bare)
        }
        RawSchemaOrRef::Inline(raw) => {
            if !raw.enum_values.is_empty() {
                let values = stringify_enum_values(field_name, &raw.enum_values)?;
                return Ok(TypeRef::Enum(values));
            }

            if let Some(RawAdditionalProperties::Schema(inner)) = &raw.additional_properties {
                let value = lower_type_ref("<additionalProperties>", inner, ctx)
                    .with_context(|| format!("additionalProperties of `{field_name}`"))?;
                return Ok(TypeRef::Map(Box::new(value)));
            }

            match raw.ty.as_deref() {
                Some("string") => {
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
                Some("array") => {
                    let items = raw.items.as_ref().ok_or_else(|| {
                        anyhow!("field `{field_name}` is `type: array` but has no `items`")
                    })?;
                    let inner = lower_type_ref("<items>", items, ctx)
                        .with_context(|| format!("in `{field_name}.items`"))?;
                    Ok(TypeRef::Array(Box::new(inner)))
                }
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

fn type_ref_is_recursive(t: &TypeRef, visiting: &HashSet<String>) -> bool {
    match t {
        TypeRef::Named(n) => visiting.contains(n),
        TypeRef::Array(inner) | TypeRef::Map(inner) => type_ref_is_recursive(inner, visiting),
        _ => false,
    }
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
        assert_eq!(api.operations[0].method, HttpMethod::Get);
        assert_eq!(api.operations[0].path, "/pets");
        assert_eq!(api.operations[0].operation_id.as_deref(), Some("listPets"));
    }

    #[test]
    fn petstore_fields_default_to_not_nullable() {
        // Sanity check: existing specs without `nullable: true` lower
        // every field with `nullable = false`. Future-proofing — if this
        // ever flips, every Phase-8 test below has to be rewritten.
        let yaml = include_str!("../../../tests/fixtures/petstore.yaml");
        let api = load_str(yaml).expect("petstore should parse");
        for schema in &api.schemas {
            if let SchemaKind::Object { fields } = &schema.kind {
                for f in fields {
                    assert!(
                        !f.nullable,
                        "{}.{} should not be nullable",
                        schema.name, f.name
                    );
                }
            }
        }
    }

    // ── Phase 8: nullability lowering ────────────────────────────────────────

    #[test]
    fn nullable_true_propagates_into_field() {
        let yaml = "
openapi: 3.0.0
info: { title: t, version: '1' }
paths: {}
components:
  schemas:
    UpdateUserRequest:
      type: object
      properties:
        nickname:
          type: string
          nullable: true
";
        let api = load_str(yaml).expect("should lower");
        let schema = api
            .schemas
            .iter()
            .find(|s| s.name == "UpdateUserRequest")
            .unwrap();
        let SchemaKind::Object { fields } = &schema.kind else {
            panic!("expected object");
        };
        let nickname = fields.iter().find(|f| f.name == "nickname").unwrap();
        assert!(nickname.nullable);
        assert!(!nickname.required, "field is not in `required:` list");
    }

    #[test]
    fn nullable_omitted_means_not_nullable() {
        let yaml = "
openapi: 3.0.0
info: { title: t, version: '1' }
paths: {}
components:
  schemas:
    Pet:
      type: object
      required: [id]
      properties:
        id: { type: integer }
        tag: { type: string }
";
        let api = load_str(yaml).expect("should lower");
        let SchemaKind::Object { fields } = &api.schemas[0].kind else {
            panic!();
        };
        for f in fields {
            assert!(!f.nullable, "{} should not be nullable", f.name);
        }
    }

    #[test]
    fn required_and_nullable_are_independent_axes() {
        // The cell that motivates all of this: required=true + nullable=true.
        // The receiver is meant to send the key with a literal null.
        let yaml = "
openapi: 3.0.0
info: { title: t, version: '1' }
paths: {}
components:
  schemas:
    Profile:
      type: object
      required: [bio]
      properties:
        bio:
          type: string
          nullable: true
";
        let api = load_str(yaml).expect("should lower");
        let SchemaKind::Object { fields } = &api.schemas[0].kind else {
            panic!();
        };
        let bio = fields.iter().find(|f| f.name == "bio").unwrap();
        assert!(bio.required);
        assert!(bio.nullable);
    }

    #[test]
    fn allof_or_merges_nullable() {
        // Base declares `email` as nullable; a subclass redeclares it
        // without nullable. The merge must keep nullable=true — narrowing
        // would silently drop `null` as a valid wire form for clients
        // that target the base schema.
        let yaml = "
openapi: 3.0.0
info: { title: t, version: '1' }
paths: {}
components:
  schemas:
    Base:
      type: object
      properties:
        email:
          type: string
          nullable: true
    Derived:
      allOf:
        - $ref: '#/components/schemas/Base'
        - type: object
          properties:
            email:
              type: string
";
        let api = load_str(yaml).expect("should lower");
        let derived = api.schemas.iter().find(|s| s.name == "Derived").unwrap();
        let SchemaKind::Object { fields } = &derived.kind else {
            panic!();
        };
        let email = fields.iter().find(|f| f.name == "email").unwrap();
        assert!(
            email.nullable,
            "OR-merge must preserve nullable=true from the base schema"
        );
    }

    #[test]
    fn ref_property_is_not_nullable() {
        // A bare `$ref` cannot carry `nullable: true` per the OpenAPI 3.0
        // spec. The lowering pass treats refs as non-nullable; specs that
        // need it use an `allOf` wrapper. This pins down the choice so
        // it can't silently flip.
        let yaml = "
openapi: 3.0.0
info: { title: t, version: '1' }
paths: {}
components:
  schemas:
    Pet:
      type: object
      properties:
        name: { type: string }
    Owner:
      type: object
      properties:
        pet:
          $ref: '#/components/schemas/Pet'
";
        let api = load_str(yaml).expect("should lower");
        let owner = api.schemas.iter().find(|s| s.name == "Owner").unwrap();
        let SchemaKind::Object { fields } = &owner.kind else {
            panic!();
        };
        let pet = fields.iter().find(|f| f.name == "pet").unwrap();
        assert!(!pet.nullable);
    }
}

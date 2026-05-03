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
//! OpenAPI 3.1's `type: [T, "null"]` is supported: `deserialize_openapi_type`
//! already accepts both a bare string and an array of strings, `primary_type`
//! filters out `"null"` to find the real type, and `is_nullable` detects it.
//! The 3.0 `nullable: true` keyword and the 3.1 array form are OR-merged in
//! `collect_object_fields`, so both formats produce identical IR.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use flap_ir::{
    Api, ApiKeyLocation, EnumValue, Field, HttpMethod, OAuth2Flow, OAuth2FlowType, Operation,
    Parameter, ParameterLocation, RequestBody, Response, Schema, SchemaKind, SecurityScheme,
    TypeRef,
};
use serde::Deserialize;
pub mod swagger;
// ── Public entry point ───────────────────────────────────────────────────────

pub fn load(path: impl AsRef<Path>) -> Result<Api> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading spec file {}", path.display()))?;

    // Detect version from first line
    let first_line = text.lines().next().unwrap_or("");
    if first_line.trim().starts_with("swagger:") {
        return load_swagger_str(&text).with_context(|| format!("in spec file {}", path.display()));
    }

    // existing 3.x path
    check_openapi_version(&text)?;
    let raw: RawSpec = serde_yaml::from_str(&text).context("parsing OpenAPI YAML")?;
    lower(raw)
}

fn lower_swagger_inline_type(
    ty: Option<&str>,
    format: Option<&str>,
    items: Option<&SwaggerItems>,
    enum_values: &[serde_yaml::Value],
) -> Result<TypeRef> {
    if !enum_values.is_empty() {
        let values = lower_enum_values("anonymous_enum", enum_values)
            .with_context(|| "invalid enum value")?;
        return Ok(TypeRef::Enum(values));
    }
    match ty {
        Some("string") => {
            if format == Some("date-time") {
                Ok(TypeRef::DateTime)
            } else {
                Ok(TypeRef::String)
            }
        }
        Some("integer") => Ok(TypeRef::Integer {
            format: format.map(|s| s.to_string()),
        }),
        Some("number") => Ok(TypeRef::Number {
            format: format.map(|s| s.to_string()),
        }),
        Some("boolean") => Ok(TypeRef::Boolean),
        Some("array") => {
            let items = items.ok_or_else(|| anyhow!("array type missing items"))?;
            let inner =
                lower_swagger_inline_type(Some(&items.ty), items.format.as_deref(), None, &[])?;
            Ok(TypeRef::Array(Box::new(inner)))
        }
        Some("file") => bail!("file parameters are not supported"),
        _ => bail!("unsupported inline type {:?}", ty),
    }
}

fn lower_swagger_schema_or_ref(
    sor: &SwaggerSchemaOrRef,
    definitions: &BTreeMap<String, SwaggerSchemaOrRef>,
    visiting: &HashSet<String>,
) -> Result<TypeRef> {
    match sor {
        SwaggerSchemaOrRef::Ref { reference } => {
            let name = parse_swagger_ref(reference)?; // extracts from "#/definitions/XXX"
            if !definitions.contains_key(name) {
                bail!("unknown definition {name}");
            }
            Ok(TypeRef::Named(name.to_string()))
        }
        SwaggerSchemaOrRef::Inline(raw) => {
            // if it has type, format, items, enum → return primitive/array
            if let Some(ty) = &raw.ty {
                return lower_swagger_inline_type(
                    Some(ty),
                    raw.format.as_deref(),
                    None,
                    &raw.enum_values,
                );
            }
            // otherwise it's an inline object/map/array, but those would become named schemas
            // (unlikely at top-level)
            bail!("inline schema without type not supported")
        }
    }
}

pub fn load_swagger_str(text: &str) -> Result<Api> {
    let raw: SwaggerSpec = serde_yaml::from_str(text).context("parsing Swagger 2.0 YAML")?;
    lower_swagger(raw)
}
/// Extracts the schema name from a Swagger‑style `$ref` pointer.
///
/// Example: `"#/definitions/Pet"` → `"Pet"`.
fn parse_swagger_ref(reference: &str) -> Result<&str> {
    let bare = reference.strip_prefix("#/definitions/").ok_or_else(|| {
        anyhow!(
            "`$ref` `{reference}` is not a definition reference – \
             Swagger 2.0 only supports `#/definitions/*`"
        )
    })?;
    if bare.is_empty() || bare.contains('/') {
        bail!("malformed $ref pointer `{reference}`");
    }
    Ok(bare)
}

fn lower_swagger(spec: SwaggerSpec) -> Result<Api> {
    let title = spec.info.title;
    let base_urls = build_swagger_base_url(&spec.host, &spec.base_path)
        .into_iter()
        .collect::<Vec<_>>();
    let mut ctx = SwaggerContext::new(&spec.definitions);
    let operations = lower_swagger_operations(&spec.paths, &mut ctx)?;
    let schemas = lower_swagger_schemas(&spec.definitions, &mut ctx)?;
    let security_schemes = lower_swagger_security(&spec.security_definitions)?;
    let security = flatten_security_requirements(&spec.security);
    Ok(Api {
        title,
        base_urls,
        operations,
        schemas,
        security_schemes,
        security,
    })
}

fn build_swagger_base_url(host: &Option<String>, base_path: &Option<String>) -> Option<String> {
    match (host.as_deref(), base_path.as_deref()) {
        (Some(h), Some(bp)) => Some(format!("https://{h}{bp}")), // assume https; could add schemes
        (Some(h), None) => Some(format!("https://{h}")),
        (None, Some(bp)) => Some(bp.to_string()),
        (None, None) => None,
    }
}

fn lower_swagger_security(
    raw: &BTreeMap<String, SwaggerSecurityDefinition>,
) -> Result<Vec<SecurityScheme>> {
    let mut out = Vec::with_capacity(raw.len());
    for (name, def) in raw {
        let scheme = lower_one_swagger_security(name, def)?;
        out.push(scheme);
    }
    Ok(out)
}

fn lower_one_swagger_security(
    name: &str,
    def: &SwaggerSecurityDefinition,
) -> Result<SecurityScheme> {
    match def.ty.as_str() {
        "apiKey" => {
            let parameter_name = def
                .name
                .clone()
                .ok_or_else(|| anyhow!("apiKey missing `name`"))?;
            let location = match def.location.as_deref() {
                Some("header") => ApiKeyLocation::Header,
                Some("query") => ApiKeyLocation::Query,
                Some("cookie") => ApiKeyLocation::Cookie,
                other => bail!("invalid apiKey location: {:?}", other),
            };
            Ok(SecurityScheme::ApiKey {
                scheme_name: name.to_string(),
                parameter_name,
                location,
            })
        }
        "basic" => {
            // Add HttpBasic to your IR if not already present; otherwise use a bearer fallback
            Ok(SecurityScheme::HttpBasic {
                scheme_name: name.to_string(),
            })
        }
        "oauth2" => {
            let flow = def.flow.as_deref().unwrap_or("implicit");
            let flow_type = match flow {
                "implicit" => OAuth2FlowType::Implicit,
                "password" => OAuth2FlowType::Password,
                "application" => OAuth2FlowType::ClientCredentials,
                "accessCode" => OAuth2FlowType::AuthorizationCode,
                other => bail!("unsupported OAuth2 flow: {other}"),
            };
            let flows = vec![OAuth2Flow {
                flow_type,
                token_url: def.token_url.clone(),
                authorization_url: def.authorization_url.clone(),
                scopes: def.scopes.keys().cloned().collect(),
            }];
            Ok(SecurityScheme::OAuth2 {
                scheme_name: name.to_string(),
                flows,
            })
        }
        other => bail!("unsupported security type: {other}"),
    }
}

fn lower_swagger_schemas(
    definitions: &BTreeMap<String, SwaggerSchemaOrRef>,
    ctx: &mut SwaggerContext,
) -> Result<Vec<Schema>> {
    let mut out = Vec::with_capacity(definitions.len());
    for (name, sor) in definitions {
        ctx.visiting.insert(name.clone());
        let kind = lower_swagger_schema_kind(name, sor, definitions, ctx)?;
        ctx.visiting.remove(name);

        // Detect allOf-based single inheritance: if the first allOf entry
        // is a $ref, that target is the logical parent. Swagger 2.0 has no
        // discriminator, so `extends` is purely informational for emitters.
        let extends = if let SwaggerSchemaOrRef::Inline(raw) = sor {
            raw.all_of.first().and_then(|first| {
                if let SwaggerSchemaOrRef::Ref { reference } = first {
                    parse_swagger_ref(reference).ok().map(str::to_string)
                } else {
                    None
                }
            })
        } else {
            None
        };

        out.push(Schema {
            name: name.clone(),
            kind,
            internal: false, // none are synthetic for Swagger
            extends,
        });
    }
    Ok(out)
}

fn lower_swagger_schema_kind(
    name: &str,
    sor: &SwaggerSchemaOrRef,
    definitions: &BTreeMap<String, SwaggerSchemaOrRef>,
    ctx: &SwaggerContext,
) -> Result<SchemaKind> {
    match sor {
        SwaggerSchemaOrRef::Ref { .. } => {
            // Swagger does not allow top-level definitions to be bare $refs, but if it happens,
            // we bail.
            bail!("top-level definition `{name}` is a bare $ref – not supported")
        }
        SwaggerSchemaOrRef::Inline(raw) => {
            if !raw.all_of.is_empty() {
                let fields = collect_swagger_object_fields(raw, definitions, ctx)?;
                return Ok(SchemaKind::Object { fields });
            }
            match raw.ty.as_deref() {
                Some("object") | None if !raw.properties.is_empty() => {
                    let fields = collect_swagger_object_fields(raw, definitions, ctx)?;
                    Ok(SchemaKind::Object { fields })
                }
                Some("object") | None
                    if raw.properties.is_empty()
                        && matches!(
                            &raw.additional_properties,
                            Some(SwaggerAdditionalProperties::Schema(_))
                        ) =>
                {
                    let Some(SwaggerAdditionalProperties::Schema(inner)) =
                        &raw.additional_properties
                    else {
                        unreachable!()
                    };
                    let value = lower_swagger_schema_or_ref(inner, definitions, &ctx.visiting)?;
                    Ok(SchemaKind::Map { value })
                }
                Some("array") => {
                    let items = raw
                        .items
                        .as_ref()
                        .ok_or_else(|| anyhow!("array schema missing items"))?;
                    let item = lower_swagger_schema_or_ref(items, definitions, &ctx.visiting)?;
                    Ok(SchemaKind::Array { item })
                }
                other => bail!("unsupported schema type {:?} for `{name}`", other),
            }
        }
    }
}

fn collect_swagger_object_fields(
    raw: &SwaggerSchema,
    definitions: &BTreeMap<String, SwaggerSchemaOrRef>,
    ctx: &SwaggerContext,
) -> Result<Vec<Field>> {
    let mut fields = Vec::new();

    // allOf members (they contribute properties)
    for member in &raw.all_of {
        fields.extend(collect_allof_fields_swagger(member, definitions, ctx)?);
    }

    let own_required: HashSet<&str> = raw.required.iter().map(String::as_str).collect();
    for (field_name, sor) in &raw.properties {
        let type_ref = lower_swagger_schema_or_ref(sor, definitions, &ctx.visiting)?;
        let is_required = own_required.contains(field_name.as_str());
        let is_recursive = type_ref_is_recursive(&type_ref, &ctx.visiting);
        fields.push(Field {
            name: field_name.clone(),
            type_ref,
            required: is_required,
            nullable: false, // Swagger 2.0 has no nullable
            is_recursive,
            default_value: None,
        });
    }

    // deduplicate (same as in spec.rs)
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut deduped: Vec<Field> = Vec::with_capacity(fields.len());
    for field in fields {
        if let Some(&idx) = seen.get(&field.name) {
            let merged_required = deduped[idx].required || field.required;
            let merged_recursive = deduped[idx].is_recursive || field.is_recursive;
            deduped[idx] = Field {
                name: field.name.clone(),         // clone because field is moved
                type_ref: field.type_ref.clone(), // clone if needed (TypeRef is Clone)
                required: merged_required,
                nullable: false, // Swagger has no nullable
                is_recursive: merged_recursive,
                default_value: None, // Swagger has no default values
            };
        } else {
            seen.insert(field.name.clone(), deduped.len());
            deduped.push(Field {
                name: field.name,
                type_ref: field.type_ref,
                required: field.required,
                nullable: false,
                is_recursive: field.is_recursive,
                default_value: None,
            });
        }
    }
    Ok(deduped)
}

fn collect_allof_fields_swagger(
    sor: &SwaggerSchemaOrRef,
    definitions: &BTreeMap<String, SwaggerSchemaOrRef>,
    ctx: &SwaggerContext,
) -> Result<Vec<Field>> {
    match sor {
        SwaggerSchemaOrRef::Ref { reference } => {
            let name = parse_swagger_ref(reference)?;
            let target = definitions
                .get(name)
                .ok_or_else(|| anyhow!("unknown allOf ref {name}"))?;
            if ctx.visiting.contains(name) {
                bail!("cycle in allOf chain via `{name}`");
            }
            match target {
                SwaggerSchemaOrRef::Inline(raw) => {
                    // Build a temporary context that marks `name` as visiting
                    // so cycles through this ref are caught on the next level.
                    let inner_ctx = ctx.with_visiting(name);
                    collect_swagger_object_fields(raw, definitions, &inner_ctx)
                }
                SwaggerSchemaOrRef::Ref { .. } => bail!("chained $ref not supported"),
            }
        }
        SwaggerSchemaOrRef::Inline(raw) => collect_swagger_object_fields(raw, definitions, ctx),
    }
}

fn lower_swagger_operations(
    paths: &BTreeMap<String, SwaggerPathItem>,
    ctx: &mut SwaggerContext,
) -> Result<Vec<Operation>> {
    let mut ops = Vec::new();
    for (path, item) in paths {
        let pairs: [(HttpMethod, &Option<SwaggerOperation>); 7] = [
            (HttpMethod::Delete, &item.delete),
            (HttpMethod::Get, &item.get),
            (HttpMethod::Head, &item.head),
            (HttpMethod::Options, &item.options),
            (HttpMethod::Patch, &item.patch),
            (HttpMethod::Post, &item.post),
            (HttpMethod::Put, &item.put),
        ];
        for (method, maybe_op) in pairs {
            if let Some(raw_op) = maybe_op {
                // Collect path-level params as references, then override with
                // operation-level params (same name+location = operation wins).
                let mut merged_params: Vec<&SwaggerParameter> = item.parameters.iter().collect();
                for op_param in &raw_op.parameters {
                    if let Some(pos) = merged_params
                        .iter()
                        .position(|p| p.name == op_param.name && p.location == op_param.location)
                    {
                        merged_params.remove(pos);
                    }
                    merged_params.push(op_param);
                }

                // Separate body from the merged list before lowering.
                let body_param = merged_params.iter().find(|p| p.location == "body").copied();
                let non_body_params: Vec<&SwaggerParameter> = merged_params
                    .iter()
                    .filter(|p| p.location != "body")
                    .copied()
                    .collect();

                let parameters = non_body_params
                    .iter()
                    .map(|p| lower_swagger_parameter(p, ctx.definitions, &ctx.visiting))
                    .collect::<Result<Vec<_>>>()?;

                let request_body = body_param
                    .map(|p| lower_swagger_body_param(p, ctx.definitions, &ctx.visiting))
                    .transpose()?;

                let responses = raw_op
                    .responses
                    .iter()
                    .map(|(code, resp)| {
                        Ok(Response {
                            headers: vec![],
                            status_code: code.clone(),
                            schema_ref: resp
                                .schema
                                .as_ref()
                                .map(|s| {
                                    lower_swagger_schema_or_ref(s, ctx.definitions, &ctx.visiting)
                                })
                                .transpose()?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;

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

fn lower_swagger_parameter(
    param: &SwaggerParameter,
    definitions: &BTreeMap<String, SwaggerSchemaOrRef>, // was &mut
    visiting: &HashSet<String>,
) -> Result<Parameter> {
    // body params are handled separately via lower_swagger_body_param
    let location = match param.location.as_str() {
        "query" => ParameterLocation::Query,
        "path" => ParameterLocation::Path,
        "header" => ParameterLocation::Header,
        "cookie" => ParameterLocation::Cookie,
        // body is handled via lower_swagger_body_param before this call site
        // is reached; if it arrives here it's a caller bug, not a spec bug.
        "body" => bail!(
            "body parameter `{}` reached lower_swagger_parameter — \
         this is a bug in the caller; body params must be extracted first",
            param.name
        ),
        // formData: treated as a regular query-style param for now.
        // A future pass could detect multipart consumes and emit is_multipart.
        "formData" => ParameterLocation::Query,
        other => bail!("unsupported parameter location `{other}`"),
    };

    let required = param.required || location == ParameterLocation::Path;
    let type_ref = if let Some(schema) = &param.schema {
        lower_swagger_schema_or_ref(schema, definitions, visiting)?
    } else {
        lower_swagger_inline_type(
            param.ty.as_deref(),
            param.format.as_deref(),
            param.items.as_deref(),
            &param.enum_values,
        )?
    };

    Ok(Parameter {
        name: param.name.clone(),
        location,
        type_ref,
        required,
    })
}

fn lower_swagger_body_param(
    param: &SwaggerParameter,
    definitions: &BTreeMap<String, SwaggerSchemaOrRef>, // was &mut
    visiting: &HashSet<String>,
) -> Result<RequestBody> {
    let schema = param
        .schema
        .as_ref()
        .ok_or_else(|| anyhow!("body param `{}` has no schema", param.name))?;
    let schema_ref = lower_swagger_schema_or_ref(schema, definitions, visiting)?;
    Ok(RequestBody {
        content_type: "application/json".to_string(),
        schema_ref,
        required: param.required,
        is_multipart: false,
    })
}

/// Exposed for unit tests.
pub fn load_str(text: &str) -> Result<Api> {
    check_openapi_version(text)?;
    let raw: RawSpec = serde_yaml::from_str(text).context("parsing OpenAPI YAML")?;
    lower(raw)
}

/// Fetch a remote OpenAPI / Swagger spec from `url` and lower it into an
/// `Api`, applying exactly the same detection and validation logic as
/// [`load`] does for local files.
pub fn load_url(url: &str) -> Result<Api> {
    let raw_text = ureq::get(url)
        .call()
        .with_context(|| format!("fetching remote spec from {url}"))?
        .into_string()
        .with_context(|| format!("reading response body from {url}"))?;

    // JSON detection — `reject_unsupported_version` scans for an `openapi:`
    // key which won't exist in a raw JSON document. Normalise to YAML first
    // so the rest of the pipeline is format-agnostic.
    let text = if raw_text.trim_start().starts_with('{') {
        let val: serde_yaml::Value = serde_json::from_str(&raw_text)
            .with_context(|| format!("parsing JSON spec from {url}"))?;
        serde_yaml::to_string(&val).context("re-serialising JSON spec as YAML")?
    } else {
        raw_text
    };

    // Reuse the same format-detection logic as `load`.
    let first_line = text.lines().next().unwrap_or("");
    if first_line.trim().starts_with("swagger:") {
        return load_swagger_str(&text).with_context(|| format!("in remote spec {url}"));
    }

    check_openapi_version(&text)?;
    let raw: RawSpec = serde_yaml::from_str(&text).context("parsing OpenAPI YAML")?;
    lower(raw)
}

/// Like [`load`] but accepts either a local filesystem path or an
/// `http://` / `https://` URL.
pub fn load_path_or_url(spec: &str) -> Result<Api> {
    if spec.starts_with("http://") || spec.starts_with("https://") {
        load_url(spec)
    } else {
        load(spec)
    }
}

// ── Version guard ────────────────────────────────────────────────────────────
fn check_openapi_version(text: &str) -> Result<()> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("openapi:") {
            let v = rest.trim().trim_matches(|c: char| c == '"' || c == '\'');
            if v.starts_with("3.") {
                return Ok(());
            }
            bail!("unsupported OpenAPI version `{v}` — flap supports OpenAPI 3.x");
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

#[derive(Debug, Default, Deserialize)]
struct RawOAuth2Flows {
    implicit: Option<RawOAuth2Flow>,
    password: Option<RawOAuth2Flow>,
    #[serde(rename = "clientCredentials")]
    client_credentials: Option<RawOAuth2Flow>,
    #[serde(rename = "authorizationCode")]
    authorization_code: Option<RawOAuth2Flow>,
}

#[derive(Debug, Deserialize)]
struct RawOAuth2Flow {
    #[serde(rename = "tokenUrl")]
    token_url: Option<String>,
    #[serde(rename = "authorizationUrl")]
    authorization_url: Option<String>,
    /// Keys are scope names; values are human-readable descriptions we discard.
    #[serde(default)]
    scopes: BTreeMap<String, String>,
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
    // ── OAuth2 ──
    flows: Option<RawOAuth2Flows>,
    // ── OpenID Connect ──
    #[serde(rename = "openIdConnectUrl")]
    open_id_connect_url: Option<String>,
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
    #[serde(default)]
    headers: BTreeMap<String, RawResponseHeader>,
}

/// One entry in a response's `headers:` block.
#[derive(Debug, Deserialize)]
struct RawResponseHeader {
    schema: Option<RawSchemaOrRef>,
    #[serde(default)]
    required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawSchemaOrRef {
    Ref {
        #[serde(rename = "$ref")]
        reference: String,
    },
    Inline(Box<RawSchema>),
}

#[derive(Debug, Default, Deserialize)]
struct RawSchema {
    #[serde(default, deserialize_with = "deserialize_openapi_type")]
    ty: Vec<String>,
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
    #[serde(default, rename = "anyOf")]
    any_of: Vec<RawSchemaOrRef>,
    #[serde(default, rename = "oneOf")]
    one_of: Vec<RawSchemaOrRef>,
    discriminator: Option<RawDiscriminator>,
    // AFTER
    /// OpenAPI 3.0 `nullable: true`. In 3.1 this keyword was removed in
    /// favour of `type: [T, "null"]`. Both forms are handled: this field
    /// captures the 3.0 flag, and `is_nullable(&raw.ty)` detects the 3.1
    /// array form. `collect_object_fields` ORs the two together.
    #[serde(default)]
    nullable: Option<bool>,
    #[serde(rename = "default")]
    default: Option<serde_yaml::Value>,
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
    synthetic_schemas: Vec<Schema>,
    extension_map: BTreeMap<String, Vec<String>>,
}

impl<'a> LoweringContext<'a> {
    fn new(components: &'a RawComponents, extension_map: BTreeMap<String, Vec<String>>) -> Self {
        Self {
            components,
            visiting: HashSet::new(),
            synthetic_schemas: Vec::new(),
            extension_map,
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

use serde::de;

use crate::swagger::{
    SwaggerAdditionalProperties, SwaggerContext, SwaggerItems, SwaggerOperation, SwaggerParameter,
    SwaggerPathItem, SwaggerSchema, SwaggerSchemaOrRef, SwaggerSecurityDefinition, SwaggerSpec,
};

fn build_allof_extension_map(
    schemas: &BTreeMap<String, RawSchemaOrRef>,
) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (child_name, sor) in schemas {
        let RawSchemaOrRef::Inline(raw) = sor else {
            continue;
        };
        let Some(RawSchemaOrRef::Ref { reference }) = raw.all_of.first() else {
            continue;
        };
        let Ok(parent_name) = parse_schema_ref_pointer(reference) else {
            continue;
        };
        map.entry(parent_name.to_string())
            .or_default()
            .push(child_name.clone());
    }
    map
}

fn deserialize_openapi_type<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: de::Deserializer<'de>,
{
    struct TypeVisitor;
    impl<'de> de::Visitor<'de> for TypeVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string or an array of strings")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            Ok(vec![value.to_string()])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut v = Vec::new();
            while let Some(s) = seq.next_element::<String>()? {
                v.push(s);
            }
            Ok(v)
        }
    }

    d.deserialize_any(TypeVisitor)
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

// ── 1. lower_enum_values ──────────────────────────────────────────────────────

fn lower_enum_values(field_name: &str, raw: &[serde_yaml::Value]) -> Result<Vec<EnumValue>> {
    raw.iter()
        .map(|v| match v {
            serde_yaml::Value::String(s) => Ok(EnumValue::Str(s.clone())),
            serde_yaml::Value::Number(n) => n.as_i64().map(EnumValue::Int).ok_or_else(|| {
                anyhow!(
                    "enum value `{n}` in `{field_name}` is not a 64-bit integer — \
                         only string and integer enum values are supported"
                )
            }),
            other => Err(anyhow!(
                "enum value `{other:?}` in `{field_name}` is not a string or integer"
            )),
        })
        .collect()
}

// ── Lowering pass (Raw* → IR) ─────────────────────────────────────────────────

fn lower(raw: RawSpec) -> Result<Api> {
    let title = raw.info.title;
    let base_urls: Vec<String> = raw.servers.into_iter().map(|s| s.url).collect();
    let extension_map = build_allof_extension_map(&raw.components.schemas);
    let mut ctx = LoweringContext::new(&raw.components, extension_map);
    let operations = lower_operations(&raw.paths, &mut ctx)?;
    let schemas = lower_schemas(&raw.components.schemas, &mut ctx)?;
    let security_schemes = lower_security_schemes(&raw.components.security_schemes)?;
    let security = flatten_security_requirements(&raw.security);
    Ok(Api {
        title,
        base_urls,
        operations,
        schemas,
        security_schemes,
        security,
    })
}

// New helper — placed near lower_type_ref:
fn lower_default_value(
    raw: &Option<serde_yaml::Value>,
    type_ref: &TypeRef,
) -> Option<flap_ir::DefaultValue> {
    use flap_ir::DefaultValue;
    let val = raw.as_ref()?;
    match type_ref {
        TypeRef::String => {
            if let serde_yaml::Value::String(s) = val {
                Some(DefaultValue::String(s.clone()))
            } else {
                None
            }
        }
        TypeRef::Integer { .. } => {
            if let serde_yaml::Value::Number(n) = val {
                n.as_i64().map(DefaultValue::Integer)
            } else {
                None
            }
        }
        TypeRef::Number { .. } => {
            if let serde_yaml::Value::Number(n) = val {
                n.as_f64().map(DefaultValue::Number)
            } else {
                None
            }
        }
        TypeRef::Boolean => {
            if let serde_yaml::Value::Bool(b) = val {
                Some(DefaultValue::Boolean(*b))
            } else {
                None
            }
        }
        // Arrays, objects, enums: cannot be expressed as Dart const literals.
        _ => None,
    }
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
        "oauth2" => {
            let raw_flows = raw.flows.as_ref().ok_or_else(|| {
                anyhow!("oauth2 security scheme `{name}` is missing the required `flows` block")
            })?;
            let flows = lower_oauth2_flows(name, raw_flows)
                .with_context(|| format!("in oauth2 scheme `{name}`"))?;
            Ok(SecurityScheme::OAuth2 {
                scheme_name: name.to_string(),
                flows,
            })
        }
        "openIdConnect" => {
            let openid_connect_url = raw.open_id_connect_url.clone().ok_or_else(|| {
                anyhow!(
                    "openIdConnect security scheme `{name}` is missing \
                     the required `openIdConnectUrl` field"
                )
            })?;
            Ok(SecurityScheme::OpenIdConnect {
                scheme_name: name.to_string(),
                openid_connect_url,
            })
        }
        other => bail!("unknown security scheme type `{other}`"),
    }
}

fn lower_oauth2_flows(scheme_name: &str, raw: &RawOAuth2Flows) -> Result<Vec<OAuth2Flow>> {
    let mut flows = Vec::new();

    if let Some(f) = &raw.implicit {
        let authorization_url = f.authorization_url.clone().ok_or_else(|| {
            anyhow!(
                "`implicit` flow in oauth2 scheme `{scheme_name}` \
                 requires `authorizationUrl`"
            )
        })?;
        flows.push(OAuth2Flow {
            flow_type: OAuth2FlowType::Implicit,
            token_url: None,
            authorization_url: Some(authorization_url),
            scopes: f.scopes.keys().cloned().collect(),
        });
    }

    if let Some(f) = &raw.password {
        let token_url = f.token_url.clone().ok_or_else(|| {
            anyhow!(
                "`password` flow in oauth2 scheme `{scheme_name}` \
                 requires `tokenUrl`"
            )
        })?;
        flows.push(OAuth2Flow {
            flow_type: OAuth2FlowType::Password,
            token_url: Some(token_url),
            authorization_url: None,
            scopes: f.scopes.keys().cloned().collect(),
        });
    }

    if let Some(f) = &raw.client_credentials {
        let token_url = f.token_url.clone().ok_or_else(|| {
            anyhow!(
                "`clientCredentials` flow in oauth2 scheme `{scheme_name}` \
                 requires `tokenUrl`"
            )
        })?;
        flows.push(OAuth2Flow {
            flow_type: OAuth2FlowType::ClientCredentials,
            token_url: Some(token_url),
            authorization_url: None,
            scopes: f.scopes.keys().cloned().collect(),
        });
    }

    if let Some(f) = &raw.authorization_code {
        let token_url = f.token_url.clone().ok_or_else(|| {
            anyhow!(
                "`authorizationCode` flow in oauth2 scheme `{scheme_name}` \
                 requires `tokenUrl`"
            )
        })?;
        let authorization_url = f.authorization_url.clone().ok_or_else(|| {
            anyhow!(
                "`authorizationCode` flow in oauth2 scheme `{scheme_name}` \
                 requires `authorizationUrl`"
            )
        })?;
        flows.push(OAuth2Flow {
            flow_type: OAuth2FlowType::AuthorizationCode,
            token_url: Some(token_url),
            authorization_url: Some(authorization_url),
            scopes: f.scopes.keys().cloned().collect(),
        });
    }

    if flows.is_empty() {
        bail!(
            "oauth2 scheme `{scheme_name}` defines no recognised flows \
             (expected at least one of: implicit, password, \
             clientCredentials, authorizationCode)"
        );
    }

    Ok(flows)
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

fn pick_request_body_content(
    content: &BTreeMap<String, RawMediaType>,
) -> Option<(String, &RawMediaType, bool)> {
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
    // ── Body schema (unchanged) ───────────────────────────────────────────────
    let schema_ref = if raw.content.is_empty() {
        None
    } else {
        let media_type = raw
            .content
            .get("application/json")
            .or_else(|| raw.content.values().next())
            .ok_or_else(|| anyhow!("response `{status_code}` has empty content map"))?;

        match &media_type.schema {
            Some(sor) => Some(
                lower_type_ref("<response>", sor, ctx)
                    .with_context(|| format!("schema of response `{status_code}`"))?,
            ),
            None => None,
        }
    };

    // ── Response headers ──────────────────────────────────────────────────────
    let mut headers: Vec<flap_ir::ResponseHeader> = Vec::new();
    for (header_name, raw_header) in &raw.headers {
        // The `Authorization` and `Content-*` family are consumed by the
        // HTTP layer; emitting them as typed fields would be misleading.
        if header_name.eq_ignore_ascii_case("authorization")
            || header_name.to_lowercase().starts_with("content-")
        {
            continue;
        }

        let schema = raw_header.schema.as_ref().ok_or_else(|| {
            anyhow!(
                "response header `{header_name}` of status `{status_code}` \
                 has no `schema` — cannot determine its type"
            )
        })?;

        let type_ref = lower_type_ref(header_name, schema, ctx).with_context(|| {
            format!(
                "schema of response header `{header_name}` \
                     of status `{status_code}`"
            )
        })?;

        // v0.1 only supports scalar and array-of-scalar headers.
        match &type_ref {
            TypeRef::Named(_) => bail!(
                "response header `{header_name}` of status `{status_code}` \
                 resolves to a named schema — only scalar types are \
                 supported for response headers in v0.1"
            ),
            _ => {}
        }

        headers.push(flap_ir::ResponseHeader {
            name: header_name.clone(),
            type_ref,
            required: raw_header.required,
        });
    }
    // Deterministic order so generated code is stable across runs.
    headers.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Response {
        status_code: status_code.to_string(),
        schema_ref,
        headers,
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

        // Record allOf-inheritance so emitters understand the hierarchy.
        let extends = if let RawSchemaOrRef::Inline(raw_schema) = schema_or_ref {
            raw_schema.all_of.first().and_then(|first| {
                if let RawSchemaOrRef::Ref { reference } = first {
                    parse_schema_ref_pointer(reference).ok().map(str::to_string)
                } else {
                    None
                }
            })
        } else {
            None
        };

        out.push(Schema {
            name: name.clone(),
            kind,
            internal: false,
            extends,
        });
    }
    out.append(&mut ctx.synthetic_schemas);
    Ok(out)
}

fn lower_allof_union(
    name: &str,
    discriminator: &RawDiscriminator,
    children: &[String],
    ctx: &LoweringContext,
) -> Result<SchemaKind> {
    let property_name = discriminator.property_name.trim();
    if property_name.is_empty() {
        bail!(
            "schema `{name}` has a `discriminator` with an empty \
             `propertyName` — set it to the wire-side field whose value \
             selects the variant."
        );
    }

    // Resolve explicit wire-tag overrides from the mapping block.
    let mut tag_by_schema: BTreeMap<String, String> = BTreeMap::new();
    for (wire_tag, schema_ref) in &discriminator.mapping {
        let bare = parse_mapping_target(schema_ref)
            .with_context(|| format!("discriminator mapping entry `{wire_tag}` of `{name}`"))?;
        tag_by_schema.insert(bare.to_string(), wire_tag.clone());
    }

    let mut variants = Vec::with_capacity(children.len());
    let mut variant_tags = Vec::with_capacity(children.len());

    // children come from BTreeMap iteration order — deterministic.
    for child_name in children {
        if !ctx.components.schemas.contains_key(child_name) {
            bail!(
                "schema `{name}` has discriminator child `{child_name}` \
                 that is not present in components.schemas"
            );
        }
        let wire_tag = tag_by_schema
            .get(child_name)
            .cloned()
            .unwrap_or_else(|| child_name.clone());
        variants.push(TypeRef::Named(child_name.clone()));
        variant_tags.push(wire_tag);
    }

    if variants.is_empty() {
        bail!(
            "schema `{name}` declares a `discriminator` but no child schemas \
             extend it via `allOf` and no `oneOf` is present — cannot build \
             a union. Either add `oneOf` or have at least one schema extend \
             `{name}` via `allOf`."
        );
    }

    Ok(SchemaKind::Union {
        variants,
        discriminator: property_name.to_string(),
        variant_tags,
    })
}

fn lower_schema_kind(
    name: &str,
    sor: &RawSchemaOrRef,
    ctx: &mut LoweringContext,
) -> Result<SchemaKind> {
    match sor {
        RawSchemaOrRef::Ref { reference } => {
            let target = parse_schema_ref_pointer(reference)
                .with_context(|| format!("top-level schema `{name}` is a bare $ref"))?;
            if !ctx.components.schemas.contains_key(target) {
                bail!(
                    "top-level schema `{name}` aliases `{target}` \
                     which is not defined in components.schemas"
                );
            }
            Ok(SchemaKind::Alias {
                target: target.to_string(),
            })
        }
        RawSchemaOrRef::Inline(raw) => lower_inline_schema(name, raw, ctx),
    }
}

fn lower_inline_schema(
    name: &str,
    raw: &RawSchema,
    ctx: &mut LoweringContext,
) -> Result<SchemaKind> {
    if !raw.any_of.is_empty() {
        return lower_any_of(name, raw, ctx);
    }

    if !raw.one_of.is_empty() {
        return lower_one_of(name, raw, ctx);
    }

    // allOf-based discriminated union: the discriminator lives on the parent
    // schema and children self-register by extending via allOf. This is the
    // OpenAPI 3.0 alternative to the explicit oneOf+discriminator form.
    if let Some(discriminator) = &raw.discriminator {
        if raw.one_of.is_empty() && raw.any_of.is_empty() {
            if let Some(children) = ctx.extension_map.get(name).cloned() {
                return lower_allof_union(name, discriminator, &children, ctx);
            }
        }
    }

    if !raw.all_of.is_empty() {
        let fields = collect_object_fields(raw, ctx)?;
        return Ok(SchemaKind::Object { fields });
    }

    match primary_type(&raw.ty) {
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

fn lower_any_of(
    parent_name: &str,
    raw: &RawSchema,
    ctx: &mut LoweringContext,
) -> Result<SchemaKind> {
    let mut variants = Vec::with_capacity(raw.any_of.len());
    let mut wrapper_schemas = Vec::new(); // to register synthetic schemas later

    for (i, sor) in raw.any_of.iter().enumerate() {
        let type_ref = lower_type_ref(&format!("{parent_name}_variant_{i}"), sor, ctx)?;
        match type_ref {
            // Named references are already valid union variants
            TypeRef::Named(n) => {
                variants.push(TypeRef::Named(n));
            }
            // Primitives (including enums, maps, arrays) need to be wrapped
            // in a named schema so Freezed can generate a class for them.
            other => {
                let wrapper_name = format!("{parent_name}Variant{i}"); // e.g., "MyFieldVariant0"
                // Create a simple wrapper schema: a single field called "value"
                let wrapper = Schema {
                    name: wrapper_name.clone(),
                    kind: SchemaKind::Object {
                        fields: vec![Field {
                            name: "value".to_string(),
                            type_ref: other,
                            required: true,
                            nullable: false,
                            is_recursive: false,
                            default_value: None,
                        }],
                    },
                    internal: true,
                    extends: None,
                };
                wrapper_schemas.push(wrapper);
                variants.push(TypeRef::Named(wrapper_name));
            }
        }
    }

    // Register the wrapper schemas in the lowering context so they are
    // included in the final Api.schemas list. We'll inject them into the
    // final schemas vector after the main loop.
    // For now, add them to a temporary list stored on the context:
    ctx.synthetic_schemas.extend(wrapper_schemas);

    Ok(SchemaKind::UntaggedUnion { variants })
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
            RawSchemaOrRef::Inline(raw) => {
                // 3.0: `nullable: true` keyword.
                // 3.1: `type: ["string", "null"]` — is_nullable scans the array.
                raw.nullable.unwrap_or(false) | is_nullable(&raw.ty)
            }
            RawSchemaOrRef::Ref { .. } => false,
        };

        let is_recursive = type_ref_is_recursive(&type_ref, &ctx.visiting);
        // Extract the spec-declared default, if any and if expressible.
        let default_value = match sor {
            RawSchemaOrRef::Inline(raw) => lower_default_value(&raw.default, &type_ref),
            RawSchemaOrRef::Ref { .. } => None,
        };
        fields.push(Field {
            name: field_name.clone(),
            type_ref,
            required: is_required,
            nullable: is_nullable,
            is_recursive,
            default_value,
        });
    }

    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut deduped: Vec<Field> = Vec::with_capacity(fields.len());
    for field in fields {
        if let Some(&idx) = seen.get(&field.name) {
            let merged_required = deduped[idx].required || field.required;
            let merged_nullable = deduped[idx].nullable || field.nullable;
            let merged_recursive = deduped[idx].is_recursive || field.is_recursive;
            let merged_default = field
                .default_value
                .clone()
                .or_else(|| deduped[idx].default_value.clone());
            deduped[idx] = Field {
                required: merged_required,
                nullable: merged_nullable,
                is_recursive: merged_recursive,
                default_value: merged_default,
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
    if raw.discriminator.is_none() {
        // No discriminator: use untagged-union semantics, same as anyOf.
        // The emitter tries each variant's fromJson in order.
        let mut variants = Vec::with_capacity(raw.one_of.len());
        let mut wrapper_schemas = Vec::new();
        for (i, sor) in raw.one_of.iter().enumerate() {
            let type_ref = lower_type_ref(&format!("{name}_variant_{i}"), sor, ctx)?;
            match type_ref {
                TypeRef::Named(n) => variants.push(TypeRef::Named(n)),
                other => {
                    let wrapper_name = format!("{name}Variant{i}");
                    wrapper_schemas.push(Schema {
                        name: wrapper_name.clone(),
                        kind: SchemaKind::Object {
                            fields: vec![Field {
                                name: "value".to_string(),
                                type_ref: other,
                                required: true,
                                nullable: false,
                                is_recursive: false,
                                default_value: None,
                            }],
                        },
                        internal: true,
                        extends: None,
                    });
                    variants.push(TypeRef::Named(wrapper_name));
                }
            }
        }
        ctx.synthetic_schemas.extend(wrapper_schemas);
        return Ok(SchemaKind::UntaggedUnion { variants });
    }

    let discriminator = raw.discriminator.as_ref().unwrap();
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
                 `components.schemas` so each variant has a stable class name."
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
            ctx.visiting.insert(bare.to_string());
            let result = match target {
                RawSchemaOrRef::Inline(target_raw) => collect_object_fields(target_raw, ctx)
                    .with_context(|| format!("flattening `{bare}` for allOf")),
                // Follow the chain — A extends B extends C now works.
                // `visiting` already contains `bare`, so cycles are caught next iteration.
                RawSchemaOrRef::Ref { .. } => collect_member_fields(target, ctx)
                    .with_context(|| format!("following ref chain through `{bare}`")),
            };
            ctx.visiting.remove(bare);
            result
        }
        RawSchemaOrRef::Inline(raw) => collect_object_fields(raw, ctx),
    }
}

fn primary_type(types: &[String]) -> Option<&str> {
    types.iter().find(|t| *t != "null").map(String::as_str)
}

fn is_nullable(types: &[String]) -> bool {
    types.iter().any(|t| t == "null")
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
                let values = lower_enum_values(field_name, &raw.enum_values)?;
                return Ok(TypeRef::Enum(values));
            }
            if let Some(RawAdditionalProperties::Schema(inner)) = &raw.additional_properties {
                let value = lower_type_ref("<additionalProperties>", inner, ctx)
                    .with_context(|| format!("additionalProperties of `{field_name}`"))?;
                return Ok(TypeRef::Map(Box::new(value)));
            }
            match primary_type(&raw.ty) {
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

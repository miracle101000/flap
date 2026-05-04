//! OpenAPI 3.0 loader and lowering pass.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use flap_ir::{
    Api, ApiKeyLocation, EnumValue, ExtensionValue, Extensions, Field, HttpMethod, OAuth2Flow,
    OAuth2FlowType, Operation, Parameter, ParameterLocation, RequestBody, Response, Schema,
    SchemaKind, SecurityScheme, TypeRef,
};
use serde::Deserialize;

pub mod swagger;

// ── Extension helpers ─────────────────────────────────────────────────────────

fn yaml_to_extension(v: &serde_yaml::Value) -> ExtensionValue {
    match v {
        serde_yaml::Value::Null => ExtensionValue::Null,
        serde_yaml::Value::Bool(b) => ExtensionValue::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ExtensionValue::Integer(i)
            } else {
                ExtensionValue::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_yaml::Value::String(s) => ExtensionValue::String(s.clone()),
        serde_yaml::Value::Sequence(seq) => {
            ExtensionValue::Sequence(seq.iter().map(yaml_to_extension).collect())
        }
        serde_yaml::Value::Mapping(map) => ExtensionValue::Mapping(
            map.iter()
                .filter_map(|(k, v)| k.as_str().map(|s| (s.to_string(), yaml_to_extension(v))))
                .collect(),
        ),
        _ => ExtensionValue::Null,
    }
}

fn collect_extensions(extra: &BTreeMap<String, serde_yaml::Value>) -> Extensions {
    extra
        .iter()
        .filter(|(k, _)| k.starts_with("x-"))
        .map(|(k, v)| (k.clone(), yaml_to_extension(v)))
        .collect()
}

// ── Public entry point ───────────────────────────────────────────────────────

pub fn load(path: impl AsRef<Path>) -> Result<Api> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading spec file {}", path.display()))?;

    let first_line = text.lines().next().unwrap_or("");
    if first_line.trim().starts_with("swagger:") {
        return load_swagger_str(&text).with_context(|| format!("in spec file {}", path.display()));
    }

    check_openapi_version(&text)?;
    let raw: RawSpec = serde_yaml::from_str(&text).context("parsing OpenAPI YAML")?;
    lower(raw)
}

pub fn load_str(text: &str) -> Result<Api> {
    check_openapi_version(text)?;
    let raw: RawSpec = serde_yaml::from_str(text).context("parsing OpenAPI YAML")?;
    lower(raw)
}

pub fn load_url(url: &str) -> Result<Api> {
    let raw_text = ureq::get(url)
        .call()
        .with_context(|| format!("fetching remote spec from {url}"))?
        .into_string()
        .with_context(|| format!("reading response body from {url}"))?;

    let text = if raw_text.trim_start().starts_with('{') {
        let val: serde_yaml::Value = serde_json::from_str(&raw_text)
            .with_context(|| format!("parsing JSON spec from {url}"))?;
        serde_yaml::to_string(&val).context("re-serialising JSON spec as YAML")?
    } else {
        raw_text
    };

    let first_line = text.lines().next().unwrap_or("");
    if first_line.trim().starts_with("swagger:") {
        return load_swagger_str(&text).with_context(|| format!("in remote spec {url}"));
    }

    check_openapi_version(&text)?;
    let raw: RawSpec = serde_yaml::from_str(&text).context("parsing OpenAPI YAML")?;
    lower(raw)
}

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

// ── Raw serde types ───────────────────────────────────────────────────────────

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
    #[serde(flatten)]
    extensions: BTreeMap<String, serde_yaml::Value>,
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
    flows: Option<RawOAuth2Flows>,
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
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
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
    #[serde(flatten)]
    extensions: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
struct RawParameter {
    name: String,
    #[serde(rename = "in")]
    location: String,
    #[serde(default)]
    required: bool,
    schema: Option<RawSchemaOrRef>,
    #[serde(flatten)]
    extensions: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
struct RawRequestBody {
    content: BTreeMap<String, RawMediaType>,
    #[serde(default)]
    required: bool,
    #[serde(flatten)]
    extensions: BTreeMap<String, serde_yaml::Value>,
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
    #[serde(flatten)]
    extensions: BTreeMap<String, serde_yaml::Value>,
}

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
    #[serde(
        default,
        rename = "type",
        deserialize_with = "deserialize_openapi_type"
    )]
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
    #[serde(default)]
    nullable: Option<bool>,
    #[serde(rename = "default")]
    default: Option<serde_yaml::Value>,
    #[serde(flatten)]
    extensions: BTreeMap<String, serde_yaml::Value>,
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

// ── Validation ────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Diagnostics {
    errors: Vec<String>,
}

impl Diagnostics {
    fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    fn into_result(self) -> Result<()> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            let joined = self.errors.join("\n  - ");
            bail!("spec validation failed:\n  - {joined}");
        }
    }
}

fn validate_raw_spec(raw: &RawSpec) -> Result<()> {
    let mut d = Diagnostics::default();
    let schemas = &raw.components.schemas;

    // ── $ref integrity ────────────────────────────────────────────────────────

    // Collect every $ref that appears anywhere in the spec and verify it
    // resolves to a known component schema. We walk schemas, path parameters,
    // request bodies, and responses.

    for (schema_name, sor) in schemas {
        validate_schema_or_ref(sor, schema_name, schemas, &mut d);
    }

    for (path, item) in &raw.paths {
        let ops: [Option<&RawOperation>; 8] = [
            item.get.as_ref(),
            item.post.as_ref(),
            item.put.as_ref(),
            item.delete.as_ref(),
            item.patch.as_ref(),
            item.options.as_ref(),
            item.head.as_ref(),
            item.trace.as_ref(),
        ];
        for op in ops.into_iter().flatten() {
            let ctx = op.operation_id.as_deref().unwrap_or(path.as_str());

            for (i, param) in op.parameters.iter().enumerate() {
                if let Some(schema) = &param.schema {
                    validate_schema_or_ref(
                        schema,
                        &format!("{ctx} parameter[{i}] `{}`", param.name),
                        schemas,
                        &mut d,
                    );
                } else if param.location != "body" {
                    // OpenAPI 3.x requires every non-body param to have a schema.
                    d.error(format!(
                        "{ctx} parameter[{i}] `{}` (in: {}) has no `schema`",
                        param.name, param.location
                    ));
                }
                if !["query", "path", "header", "cookie"].contains(&param.location.as_str()) {
                    d.error(format!(
                        "{ctx} parameter[{i}] `{}` has unsupported `in: {}`",
                        param.name, param.location
                    ));
                }
            }

            if let Some(rb) = &op.request_body {
                for (ct, media) in &rb.content {
                    if let Some(s) = &media.schema {
                        validate_schema_or_ref(
                            s,
                            &format!("{ctx} requestBody[{ct}]"),
                            schemas,
                            &mut d,
                        );
                    }
                }
            }

            for (code, resp) in &op.responses {
                for (ct, media) in &resp.content {
                    if let Some(s) = &media.schema {
                        validate_schema_or_ref(
                            s,
                            &format!("{ctx} response[{code}][{ct}]"),
                            schemas,
                            &mut d,
                        );
                    }
                }
            }

            // operationId uniqueness is checked separately below.
        }
    }

    // ── operationId uniqueness ────────────────────────────────────────────────
    let mut seen_ids: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for (path, item) in &raw.paths {
        let ops: [Option<&RawOperation>; 8] = [
            item.get.as_ref(),
            item.post.as_ref(),
            item.put.as_ref(),
            item.delete.as_ref(),
            item.patch.as_ref(),
            item.options.as_ref(),
            item.head.as_ref(),
            item.trace.as_ref(),
        ];
        for op in ops.into_iter().flatten() {
            if let Some(id) = &op.operation_id {
                if let Some(prev) = seen_ids.insert(id.as_str(), path.as_str()) {
                    d.error(format!(
                        "operationId `{id}` is used by both `{prev}` and `{path}`"
                    ));
                }
            }
        }
    }

    // ── security scheme references ────────────────────────────────────────────
    let defined_schemes: std::collections::HashSet<&str> = raw
        .components
        .security_schemes
        .keys()
        .map(String::as_str)
        .collect();

    let check_security_refs =
        |reqs: &[BTreeMap<String, Vec<String>>], location: &str, d: &mut Diagnostics| {
            for req in reqs {
                for name in req.keys() {
                    if !defined_schemes.contains(name.as_str()) {
                        d.error(format!(
                            "security requirement `{name}` at {location} references an \
                         undefined security scheme"
                        ));
                    }
                }
            }
        };

    check_security_refs(&raw.security, "top-level", &mut d);
    for (path, item) in &raw.paths {
        let ops: [Option<&RawOperation>; 8] = [
            item.get.as_ref(),
            item.post.as_ref(),
            item.put.as_ref(),
            item.delete.as_ref(),
            item.patch.as_ref(),
            item.options.as_ref(),
            item.head.as_ref(),
            item.trace.as_ref(),
        ];
        for op in ops.into_iter().flatten() {
            if let Some(reqs) = &op.security {
                let ctx = op.operation_id.as_deref().unwrap_or(path.as_str());
                check_security_refs(reqs, ctx, &mut d);
            }
        }
    }

    d.into_result()
}

/// Recursively validate every $ref inside a schema tree.
fn validate_schema_or_ref(
    sor: &RawSchemaOrRef,
    location: &str,
    schemas: &BTreeMap<String, RawSchemaOrRef>,
    d: &mut Diagnostics,
) {
    match sor {
        RawSchemaOrRef::Ref { reference } => match parse_schema_ref_pointer(reference) {
            Ok(name) => {
                if !schemas.contains_key(name) {
                    d.error(format!(
                        "{location}: $ref `{reference}` points to undefined schema `{name}`"
                    ));
                }
            }
            Err(e) => d.error(format!("{location}: malformed $ref `{reference}`: {e}")),
        },
        RawSchemaOrRef::Inline(raw) => {
            validate_inline_schema(raw, location, schemas, d);
        }
    }
}

fn validate_inline_schema(
    raw: &RawSchema,
    location: &str,
    schemas: &BTreeMap<String, RawSchemaOrRef>,
    d: &mut Diagnostics,
) {
    for (field_name, sor) in &raw.properties {
        validate_schema_or_ref(sor, &format!("{location}.{field_name}"), schemas, d);
    }
    if let Some(items) = &raw.items {
        validate_schema_or_ref(items, &format!("{location}[items]"), schemas, d);
    }
    for (i, member) in raw.all_of.iter().enumerate() {
        validate_schema_or_ref(member, &format!("{location} allOf[{i}]"), schemas, d);
    }
    for (i, member) in raw.any_of.iter().enumerate() {
        validate_schema_or_ref(member, &format!("{location} anyOf[{i}]"), schemas, d);
    }
    for (i, member) in raw.one_of.iter().enumerate() {
        validate_schema_or_ref(member, &format!("{location} oneOf[{i}]"), schemas, d);
    }
    if let Some(RawAdditionalProperties::Schema(inner)) = &raw.additional_properties {
        validate_schema_or_ref(
            inner,
            &format!("{location}[additionalProperties]"),
            schemas,
            d,
        );
    }
    // discriminator mapping refs
    if let Some(disc) = &raw.discriminator {
        for (tag, target) in &disc.mapping {
            if target.starts_with("#/") {
                match parse_schema_ref_pointer(target) {
                    Ok(name) if !schemas.contains_key(name) => d.error(format!(
                        "{location} discriminator mapping `{tag}` → `{target}` \
                         points to undefined schema `{name}`"
                    )),
                    Err(e) => d.error(format!(
                        "{location} discriminator mapping `{tag}`: malformed $ref: {e}"
                    )),
                    _ => {}
                }
            }
        }
    }
}

fn validate_swagger_spec(spec: &SwaggerSpec) -> Result<()> {
    let mut d = Diagnostics::default();
    let definitions = &spec.definitions;

    // ── $ref integrity ────────────────────────────────────────────────────────
    for (name, sor) in definitions {
        validate_swagger_sor(sor, name, definitions, &mut d);
    }

    for (path, item) in &spec.paths {
        let ops: [Option<&SwaggerOperation>; 7] = [
            item.get.as_ref(),
            item.post.as_ref(),
            item.put.as_ref(),
            item.delete.as_ref(),
            item.patch.as_ref(),
            item.options.as_ref(),
            item.head.as_ref(),
        ];
        for op in ops.into_iter().flatten() {
            let ctx = op.operation_id.as_deref().unwrap_or(path.as_str());
            for (i, param) in op.parameters.iter().enumerate() {
                if let Some(s) = &param.schema {
                    validate_swagger_sor(
                        s,
                        &format!("{ctx} parameter[{i}] `{}`", param.name),
                        definitions,
                        &mut d,
                    );
                }
            }
            for (code, resp) in &op.responses {
                if let Some(s) = &resp.schema {
                    validate_swagger_sor(
                        s,
                        &format!("{ctx} response[{code}]"),
                        definitions,
                        &mut d,
                    );
                }
            }
        }
    }

    // ── operationId uniqueness ────────────────────────────────────────────────
    let mut seen_ids: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for (path, item) in &spec.paths {
        let ops: [Option<&SwaggerOperation>; 7] = [
            item.get.as_ref(),
            item.post.as_ref(),
            item.put.as_ref(),
            item.delete.as_ref(),
            item.patch.as_ref(),
            item.options.as_ref(),
            item.head.as_ref(),
        ];
        for op in ops.into_iter().flatten() {
            if let Some(id) = &op.operation_id {
                if let Some(prev) = seen_ids.insert(id.as_str(), path.as_str()) {
                    d.error(format!(
                        "operationId `{id}` is used by both `{prev}` and `{path}`"
                    ));
                }
            }
        }
    }

    d.into_result()
}

fn validate_swagger_sor(
    sor: &SwaggerSchemaOrRef,
    location: &str,
    definitions: &BTreeMap<String, SwaggerSchemaOrRef>,
    d: &mut Diagnostics,
) {
    match sor {
        SwaggerSchemaOrRef::Ref { reference } => match parse_swagger_ref(reference) {
            Ok(name) => {
                if !definitions.contains_key(name) {
                    d.error(format!(
                        "{location}: $ref `{reference}` points to undefined definition `{name}`"
                    ));
                }
            }
            Err(e) => d.error(format!("{location}: malformed $ref `{reference}`: {e}")),
        },
        SwaggerSchemaOrRef::Inline(raw) => {
            for (field, sor) in &raw.properties {
                validate_swagger_sor(sor, &format!("{location}.{field}"), definitions, d);
            }
            if let Some(items) = &raw.items {
                validate_swagger_sor(items, &format!("{location}[items]"), definitions, d);
            }
            for (i, member) in raw.all_of.iter().enumerate() {
                validate_swagger_sor(member, &format!("{location} allOf[{i}]"), definitions, d);
            }
            if let Some(SwaggerAdditionalProperties::Schema(inner)) = &raw.additional_properties {
                validate_swagger_sor(
                    inner,
                    &format!("{location}[additionalProperties]"),
                    definitions,
                    d,
                );
            }
        }
    }
}

// ── Lowering context ──────────────────────────────────────────────────────────

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

// ── Deserializer helpers ──────────────────────────────────────────────────────

use serde::de;

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

// ── Enum values ───────────────────────────────────────────────────────────────

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

// ── Default values ────────────────────────────────────────────────────────────

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
        _ => None,
    }
}

// ── Top-level lowering ────────────────────────────────────────────────────────

fn lower(raw: RawSpec) -> Result<Api> {
    validate_raw_spec(&raw)?;
    let extensions = collect_extensions(&raw.extensions);
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
        extensions,
    })
}

// ── Security ──────────────────────────────────────────────────────────────────

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
            anyhow!("`implicit` flow in oauth2 scheme `{scheme_name}` requires `authorizationUrl`")
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
            anyhow!("`password` flow in oauth2 scheme `{scheme_name}` requires `tokenUrl`")
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
            anyhow!("`clientCredentials` flow in oauth2 scheme `{scheme_name}` requires `tokenUrl`")
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
            anyhow!("`authorizationCode` flow in oauth2 scheme `{scheme_name}` requires `tokenUrl`")
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

// ── Operation lowering ────────────────────────────────────────────────────────

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
                    extensions: collect_extensions(&raw_op.extensions),
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
            "parameter `{}` in {path} has no `schema` — cannot determine its type",
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
        extensions: collect_extensions(&raw.extensions),
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
        anyhow!("content type `{content_type}` in requestBody of {method} {path} has no schema")
    })?;

    let schema_ref = lower_type_ref("<requestBody>", schema, ctx)?;

    Ok(RequestBody {
        content_type,
        schema_ref,
        required: raw.required,
        is_multipart,
        extensions: collect_extensions(&raw.extensions),
    })
}

// ── Response lowering ─────────────────────────────────────────────────────────

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

    let mut headers: Vec<flap_ir::ResponseHeader> = Vec::new();
    for (header_name, raw_header) in &raw.headers {
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
            format!("schema of response header `{header_name}` of status `{status_code}`")
        })?;

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
    headers.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Response {
        status_code: status_code.to_string(),
        schema_ref,
        headers,
        extensions: collect_extensions(&raw.extensions),
    })
}

// ── Schema lowering ───────────────────────────────────────────────────────────

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

        let extensions = match schema_or_ref {
            RawSchemaOrRef::Inline(raw) => collect_extensions(&raw.extensions),
            RawSchemaOrRef::Ref { .. } => BTreeMap::new(),
        };

        out.push(Schema {
            name: name.clone(),
            kind,
            internal: false,
            extends,
            extensions,
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

    let mut tag_by_schema: BTreeMap<String, String> = BTreeMap::new();
    for (wire_tag, schema_ref) in &discriminator.mapping {
        let bare = parse_mapping_target(schema_ref)
            .with_context(|| format!("discriminator mapping entry `{wire_tag}` of `{name}`"))?;
        tag_by_schema.insert(bare.to_string(), wire_tag.clone());
    }

    let mut variants = Vec::with_capacity(children.len());
    let mut variant_tags = Vec::with_capacity(children.len());

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
                unreachable!()
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
            "schema `{name}` has no `type` and no `properties` — cannot determine kind"
        )),
    }
}

fn lower_any_of(
    parent_name: &str,
    raw: &RawSchema,
    ctx: &mut LoweringContext,
) -> Result<SchemaKind> {
    let mut variants = Vec::with_capacity(raw.any_of.len());
    let mut wrapper_schemas = Vec::new();

    for (i, sor) in raw.any_of.iter().enumerate() {
        let type_ref = lower_type_ref(&format!("{parent_name}_variant_{i}"), sor, ctx)?;
        match type_ref {
            TypeRef::Named(n) => {
                variants.push(TypeRef::Named(n));
            }
            other => {
                let wrapper_name = format!("{parent_name}Variant{i}");
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
                            extensions: BTreeMap::new(),
                        }],
                    },
                    internal: true,
                    extends: None,
                    extensions: BTreeMap::new(),
                });
                variants.push(TypeRef::Named(wrapper_name));
            }
        }
    }

    ctx.synthetic_schemas.extend(wrapper_schemas);
    Ok(SchemaKind::UntaggedUnion { variants })
}

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

        let is_nullable = match sor {
            RawSchemaOrRef::Inline(raw) => raw.nullable.unwrap_or(false) | is_nullable(&raw.ty),
            RawSchemaOrRef::Ref { .. } => false,
        };

        let is_recursive = type_ref_is_recursive(&type_ref, &ctx.visiting);

        let default_value = match sor {
            RawSchemaOrRef::Inline(raw) => lower_default_value(&raw.default, &type_ref),
            RawSchemaOrRef::Ref { .. } => None,
        };

        let field_extensions = match sor {
            RawSchemaOrRef::Inline(raw) => collect_extensions(&raw.extensions),
            RawSchemaOrRef::Ref { .. } => BTreeMap::new(),
        };

        fields.push(Field {
            name: field_name.clone(),
            type_ref,
            required: is_required,
            nullable: is_nullable,
            is_recursive,
            default_value,
            extensions: field_extensions,
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
            let merged_extensions = {
                let mut m = deduped[idx].extensions.clone();
                m.extend(field.extensions.clone());
                m
            };
            deduped[idx] = Field {
                required: merged_required,
                nullable: merged_nullable,
                is_recursive: merged_recursive,
                default_value: merged_default,
                extensions: merged_extensions,
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
                                extensions: BTreeMap::new(),
                            }],
                        },
                        internal: true,
                        extends: None,
                        extensions: BTreeMap::new(),
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

fn type_ref_is_recursive(t: &TypeRef, visiting: &HashSet<String>) -> bool {
    match t {
        TypeRef::Named(n) => visiting.contains(n),
        TypeRef::Array(inner) | TypeRef::Map(inner) => type_ref_is_recursive(inner, visiting),
        _ => false,
    }
}

// ── Swagger 2.0 lowering ──────────────────────────────────────────────────────

use crate::swagger::{
    SwaggerAdditionalProperties, SwaggerContext, SwaggerItems, SwaggerOperation, SwaggerParameter,
    SwaggerPathItem, SwaggerSchema, SwaggerSchemaOrRef, SwaggerSecurityDefinition, SwaggerSpec,
};

pub fn load_swagger_str(text: &str) -> Result<Api> {
    let raw: SwaggerSpec = serde_yaml::from_str(text).context("parsing Swagger 2.0 YAML")?;
    lower_swagger(raw)
}

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
    validate_swagger_spec(&spec)?;
    let extensions = collect_extensions(&spec.extensions);
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
        extensions,
    })
}

fn build_swagger_base_url(host: &Option<String>, base_path: &Option<String>) -> Option<String> {
    match (host.as_deref(), base_path.as_deref()) {
        (Some(h), Some(bp)) => Some(format!("https://{h}{bp}")),
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
        out.push(lower_one_swagger_security(name, def)?);
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
        "basic" => Ok(SecurityScheme::HttpBasic {
            scheme_name: name.to_string(),
        }),
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

        let extensions = if let SwaggerSchemaOrRef::Inline(raw) = sor {
            collect_extensions(&raw.extensions)
        } else {
            BTreeMap::new()
        };

        out.push(Schema {
            name: name.clone(),
            kind,
            internal: false,
            extends,
            extensions,
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

    for member in &raw.all_of {
        fields.extend(collect_allof_fields_swagger(member, definitions, ctx)?);
    }

    let own_required: HashSet<&str> = raw.required.iter().map(String::as_str).collect();
    for (field_name, sor) in &raw.properties {
        let type_ref = lower_swagger_schema_or_ref(sor, definitions, &ctx.visiting)?;
        let is_required = own_required.contains(field_name.as_str());
        let is_recursive = type_ref_is_recursive(&type_ref, &ctx.visiting);
        let extensions = if let SwaggerSchemaOrRef::Inline(s) = sor {
            collect_extensions(&s.extensions)
        } else {
            BTreeMap::new()
        };
        fields.push(Field {
            name: field_name.clone(),
            type_ref,
            required: is_required,
            nullable: false,
            is_recursive,
            default_value: None,
            extensions,
        });
    }

    // Deduplication
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut deduped: Vec<Field> = Vec::with_capacity(fields.len());
    for field in fields {
        if let Some(&idx) = seen.get(&field.name) {
            let merged_required = deduped[idx].required || field.required;
            let merged_recursive = deduped[idx].is_recursive || field.is_recursive;
            let merged_extensions = {
                let mut m = deduped[idx].extensions.clone();
                m.extend(field.extensions.clone());
                m
            };
            deduped[idx] = Field {
                name: field.name.clone(),
                type_ref: field.type_ref.clone(),
                required: merged_required,
                nullable: false,
                is_recursive: merged_recursive,
                default_value: None,
                extensions: merged_extensions,
            };
        } else {
            seen.insert(field.name.clone(), deduped.len());
            deduped.push(field);
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
                            extensions: collect_extensions(&resp.extensions),
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
                    extensions: collect_extensions(&raw_op.extensions),
                });
            }
        }
    }
    Ok(ops)
}

fn lower_swagger_parameter(
    param: &SwaggerParameter,
    definitions: &BTreeMap<String, SwaggerSchemaOrRef>,
    visiting: &HashSet<String>,
) -> Result<Parameter> {
    let location = match param.location.as_str() {
        "query" => ParameterLocation::Query,
        "path" => ParameterLocation::Path,
        "header" => ParameterLocation::Header,
        "cookie" => ParameterLocation::Cookie,
        "body" => bail!(
            "body parameter `{}` reached lower_swagger_parameter — \
             this is a bug in the caller; body params must be extracted first",
            param.name
        ),
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
        extensions: collect_extensions(&param.extensions),
    })
}

fn lower_swagger_body_param(
    param: &SwaggerParameter,
    definitions: &BTreeMap<String, SwaggerSchemaOrRef>,
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
        extensions: collect_extensions(&param.extensions),
    })
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
            let name = parse_swagger_ref(reference)?;
            if !definitions.contains_key(name) {
                bail!("unknown definition {name}");
            }
            Ok(TypeRef::Named(name.to_string()))
        }
        SwaggerSchemaOrRef::Inline(raw) => {
            if let Some(ty) = &raw.ty {
                return lower_swagger_inline_type(
                    Some(ty),
                    raw.format.as_deref(),
                    None,
                    &raw.enum_values,
                );
            }
            bail!("inline schema without type not supported")
        }
    }
}

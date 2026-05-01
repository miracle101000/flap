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
//!   request bodies, responses — uniformly via `LoweringContext::resolve_schema`.
//!
//! Phase 2 additions:
//! - `lower_type_ref` recognises three new shapes:
//!   - `enum: [...]` → `TypeRef::Enum`
//!   - `additionalProperties: { ... }` → `TypeRef::Map`
//!   - `type: string, format: date-time` → `TypeRef::DateTime`
//! - `lower_inline_schema` handles `allOf` (see `collect_object_fields`).
//!
//! Phase 3 additions:
//! - `lower_response` populates the new `Operation.responses` IR field. The
//!   `responses` map is keyed by status code (or `"default"`); each entry's
//!   schema is taken from `content[application/json].schema` if present,
//!   otherwise the response is recorded with `schema_ref: None`. Response
//!   ordering is deterministic: numeric codes sorted ascending first, then
//!   `"default"` last, so the IR is stable across runs regardless of how
//!   YAML mappings happen to iterate.
//! - `lower_request_body` now accepts `multipart/form-data` as a fallback
//!   when `application/json` is not present. The chosen content type is
//!   reflected in `RequestBody.content_type`, and `is_multipart` is set
//!   when `multipart/form-data` is selected so the Dart emitter can later
//!   build a Dio `FormData` instead of a JSON body.
//!
//! Design notes:
//! - `Raw*` types mirror the OpenAPI YAML structure and are only used for parsing.
//! - `lower_*` functions convert `&Raw*` → IR. Errors are propagated with context.
//! - Per DECISIONS D5, OpenAPI 3.1 is rejected up front with a clear message.
//! - Per DECISIONS D6, oneOf/anyOf without a discriminator will be a hard error
//!   once the emitter needs to handle them; for now they're not in PetStore.
//! - v0.1 only supports `#/components/schemas/*` `$ref` pointers. Pointers into
//!   `components/parameters` or `components/responses` are deferred — the IR
//!   doesn't yet model those component groups.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use flap_ir::{
    ApiKeyLocation, Api, Field, HttpMethod, Operation, Parameter, ParameterLocation, RequestBody,
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
    /// Top-level `security` block — a list of OR-ed security requirements.
    /// Each requirement is an AND-of-schemes map (scheme name → scope list).
    /// In v0.1 we collapse the structure to a deduplicated flat list of
    /// scheme names; see `flatten_security_requirements`.
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
    /// `components.securitySchemes` — name → scheme definition. BTreeMap
    /// keeps iteration alphabetical so emitter output is reproducible.
    #[serde(default, rename = "securitySchemes")]
    security_schemes: BTreeMap<String, RawSecurityScheme>,
}

/// A single entry from `components.securitySchemes`.
///
/// The shape is intentionally permissive at parse time: every field is
/// optional except `type`, and `lower_security_schemes` validates the
/// combinations actually permitted by OpenAPI per scheme type. This keeps
/// the error messages co-located with the lowering logic rather than
/// scattered across serde's `#[serde(deny_unknown_fields)]` plumbing.
#[derive(Debug, Deserialize)]
struct RawSecurityScheme {
    #[serde(rename = "type")]
    ty: String,
    /// `apiKey`: the wire-side parameter name, e.g. `"X-API-Key"`.
    name: Option<String>,
    /// `apiKey`: where the key is sent — `"header"`, `"query"`, or
    /// `"cookie"`.
    #[serde(rename = "in")]
    location: Option<String>,
    /// `http`: the auth scheme name (`"bearer"`, `"basic"`, ...). v0.1
    /// only honours `"bearer"`; other schemes produce a clear error
    /// during lowering rather than silently emitting incorrect code.
    scheme: Option<String>,
    /// `http` + `scheme: bearer`: optional format hint, e.g. `"JWT"`.
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
    /// Phase 3: the `responses` block. Keyed by status code (or `"default"`).
    /// Default to an empty map if absent — OpenAPI requires at least one
    /// response, but we don't enforce that at lowering time.
    #[serde(default)]
    responses: BTreeMap<String, RawResponse>,
    /// Per-operation `security` override. `None` means "use API default".
    /// `Some([])` means "explicitly no security on this endpoint" — the
    /// OpenAPI sentinel for marking a single endpoint public when the
    /// rest of the API is authenticated.
    security: Option<Vec<BTreeMap<String, Vec<String>>>>,
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
    /// We prefer `application/json`, fall back to `multipart/form-data`,
    /// then to the first entry.
    content: BTreeMap<String, RawMediaType>,
    /// Defaults to false per the OpenAPI spec.
    #[serde(default)]
    required: bool,
}

/// A single entry in either `requestBody.content` or `responses[*].content`.
#[derive(Debug, Deserialize)]
struct RawMediaType {
    /// The schema of this media type. Bodies bail if absent; responses
    /// merely omit the schema.
    schema: Option<RawSchemaOrRef>,
}

/// Phase 3: a single entry in the `responses` map, e.g. `"200"` or `"default"`.
///
/// OpenAPI permits the entire response to be a `$ref` into
/// `components/responses`, but that component group is deferred to a future
/// IR expansion. For v0.1 we accept inline response objects only.
#[derive(Debug, Deserialize)]
struct RawResponse {
    /// `description` is required by the spec — we don't carry it into IR
    /// but accept it during parsing so unknown-field warnings don't fire on
    /// real specs. Marked `dead_code` since the emitter doesn't consume it.
    #[allow(dead_code)]
    description: Option<String>,
    /// Map of content-type → media type object. Optional — many responses
    /// (204, 201 created with no body) declare no content.
    #[serde(default)]
    content: BTreeMap<String, RawMediaType>,
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
/// `components/parameters` or `components/responses` will be added when the
/// IR grows to model those.
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

    // Security schemes are validated structurally and turned into IR before
    // any operation-level security can reference them. We do *not* enforce
    // that every `security` reference points at a declared scheme — real
    // specs occasionally cite implicit/global schemes the OpenAPI document
    // forgets to define, and the emitter is the right place to surface
    // that mismatch (it's the layer that needs the scheme to exist).
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

/// Lowers `components.securitySchemes` into IR variants.
///
/// v0.1 supports two scheme shapes:
/// - `type: apiKey` with `name` + `in: {header|query|cookie}`.
/// - `type: http` with `scheme: bearer` (case-insensitive) and an optional
///   `bearerFormat`.
///
/// Anything else (`oauth2`, `openIdConnect`, `http` with `basic`/`digest`,
/// etc.) produces a clear hard error rather than being silently dropped —
/// dropping would mean the generated client compiles but cannot authenticate
/// against an endpoint that requires it, which is the worst kind of bug.
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

/// Collapses OpenAPI's list-of-AND-of-OR security requirement structure into
/// a flat, deduplicated list of scheme names in spec order.
///
/// OpenAPI semantically says "any one of these requirements is sufficient,
/// and within each requirement all named schemes must be presented". The
/// generated Dart client doesn't gate calls on this structure — it sends
/// every credential the caller provided, regardless of whether the spec
/// listed them as alternatives or together — so we simply collect the
/// union of every scheme name referenced anywhere. A future phase that
/// adds proper requirement-set enforcement can re-derive the structure
/// from the YAML; nothing about this collapse is lossy from the
/// emitter's current point of view.
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

                // Phase 3: lower responses, deterministically ordered.
                let responses = lower_responses(path, method, &raw_op.responses, ctx)?;

                // Phase 5 (auth): per-op security override. `Some(empty)` is
                // OpenAPI's "explicitly public" sentinel and is preserved as
                // an empty Vec rather than being collapsed to `None`.
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

/// Selects the preferred content type from a `requestBody.content` map.
///
/// Preference order:
/// 1. `application/json` — the default for modern REST APIs.
/// 2. `multipart/form-data` — file uploads, the common second case. We
///    surface `is_multipart=true` so the emitter knows to build `FormData`.
/// 3. The first entry in BTreeMap order — last-resort fallback for specs
///    that only expose a single non-standard media type. `is_multipart`
///    stays false; the emitter will treat it as JSON-ish until a future
///    phase adds richer codec selection.
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

// ── Phase 3: response lowering ───────────────────────────────────────────────

/// Lowers an operation's `responses` map into a deterministic Vec.
///
/// Status codes are emitted in this order:
/// 1. Numeric codes ascending (so "200" before "404" before "500").
/// 2. The `"default"` sentinel last.
/// 3. Anything else (extension keys, etc.) sorted lexicographically before
///    `"default"`.
///
/// The status-code key is preserved verbatim in `Response.status_code` so
/// emitters can reproduce it (e.g. for switch arms or doc comments) without
/// having to pretty-print a numeric type back into a string.
fn lower_responses(
    path: &str,
    method: HttpMethod,
    raw: &BTreeMap<String, RawResponse>,
    ctx: &mut LoweringContext,
) -> Result<Vec<Response>> {
    // Sort keys: numeric codes first (ascending by integer value), then
    // non-numeric / non-default keys lexicographically, then "default" last.
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

/// Lowers a single response entry.
///
/// Schema selection from `content`: prefer `application/json`; fall back to
/// the first entry (BTreeMap-ordered, so deterministic). When `content` is
/// absent or the chosen entry has no `schema`, `schema_ref` is `None` —
/// callers like the Dart emitter will turn that into `Future<void>`-shaped
/// returns.
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
        // Unreachable given the is_empty guard above, but cheap defensiveness.
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
                Some("array") => {
                    // Inline arrays — common in response schemas like
                    // `application/json: { schema: { type: array, items: ... } }`.
                    // We can't synthesise a top-level Schema here, so treat
                    // the array as a Map-like-but-list... actually, no: the IR
                    // doesn't yet model inline list TypeRefs. Reject with a
                    // clear pointer to the workaround (declare a named schema).
                    Err(anyhow!(
                        "field `{field_name}` is an inline array — declare it \
                         as a named schema in components.schemas and $ref to it"
                    ))
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
        assert_eq!(api.operations[0].method, HttpMethod::Get);
        assert_eq!(api.operations[0].path, "/pets");
        assert_eq!(api.operations[0].operation_id.as_deref(), Some("listPets"));
        assert_eq!(api.operations[1].method, HttpMethod::Post);
        assert_eq!(api.operations[2].method, HttpMethod::Get);
        assert_eq!(api.operations[2].path, "/pets/{petId}");
    }

    // ── Phase 5: auth lowering ───────────────────────────────────────────────

    #[test]
    fn no_security_block_yields_empty_ir_fields() {
        // The bare PetStore fixture declares no auth; the IR should reflect
        // that with empty (not missing) collections, so emitters can do
        // straight `.is_empty()` checks without ceremony.
        let yaml = include_str!("../../../tests/fixtures/petstore.yaml");
        let api = load_str(yaml).expect("petstore should parse");
        assert!(api.security_schemes.is_empty());
        assert!(api.security.is_empty());
        for op in &api.operations {
            assert!(op.security.is_none(), "no per-op security expected");
        }
    }

    #[test]
    fn lowers_bearer_and_api_key_schemes() {
        let yaml = "
openapi: 3.0.0
info:
  title: secured
  version: '1'
paths: {}
components:
  securitySchemes:
    bearerAuth:
      type: http
      scheme: bearer
      bearerFormat: JWT
    apiKeyAuth:
      type: apiKey
      in: header
      name: X-API-Key
security:
  - bearerAuth: []
  - apiKeyAuth: []
";
        let api = load_str(yaml).expect("should parse");
        assert_eq!(api.security_schemes.len(), 2);

        // BTreeMap iteration → alphabetical: apiKeyAuth, bearerAuth.
        match &api.security_schemes[0] {
            SecurityScheme::ApiKey {
                scheme_name,
                parameter_name,
                location,
            } => {
                assert_eq!(scheme_name, "apiKeyAuth");
                assert_eq!(parameter_name, "X-API-Key");
                assert_eq!(*location, ApiKeyLocation::Header);
            }
            other => panic!("expected ApiKey first, got {other:?}"),
        }
        match &api.security_schemes[1] {
            SecurityScheme::HttpBearer {
                scheme_name,
                bearer_format,
            } => {
                assert_eq!(scheme_name, "bearerAuth");
                assert_eq!(bearer_format.as_deref(), Some("JWT"));
            }
            other => panic!("expected HttpBearer second, got {other:?}"),
        }

        // Top-level requirements flatten to the union of names.
        assert_eq!(
            api.security,
            vec!["bearerAuth".to_string(), "apiKeyAuth".to_string()]
        );
    }

    #[test]
    fn api_key_supports_query_and_cookie_locations() {
        let yaml = "
openapi: 3.0.0
info:
  title: secured
  version: '1'
paths: {}
components:
  securitySchemes:
    qKey:
      type: apiKey
      in: query
      name: api_key
    cKey:
      type: apiKey
      in: cookie
      name: SESSION
";
        let api = load_str(yaml).expect("should parse");
        let by_name: std::collections::HashMap<&str, &SecurityScheme> = api
            .security_schemes
            .iter()
            .map(|s| (s.scheme_name(), s))
            .collect();
        match by_name["qKey"] {
            SecurityScheme::ApiKey { location, .. } => assert_eq!(*location, ApiKeyLocation::Query),
            _ => panic!("qKey should be ApiKey"),
        }
        match by_name["cKey"] {
            SecurityScheme::ApiKey { location, .. } => {
                assert_eq!(*location, ApiKeyLocation::Cookie)
            }
            _ => panic!("cKey should be ApiKey"),
        }
    }

    #[test]
    fn rejects_unsupported_scheme_types() {
        // oauth2 → clear error rather than silent drop.
        let yaml = "
openapi: 3.0.0
info:
  title: x
  version: '1'
paths: {}
components:
  securitySchemes:
    oauth:
      type: oauth2
      flows:
        implicit:
          authorizationUrl: https://example.com/auth
          scopes: {}
";
        let err = load_str(yaml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("oauth2") && msg.contains("not supported"),
            "expected unsupported-scheme error, got: {msg}"
        );
    }

    #[test]
    fn rejects_http_basic() {
        // We only support `scheme: bearer` under `type: http` for now.
        let yaml = "
openapi: 3.0.0
info:
  title: x
  version: '1'
paths: {}
components:
  securitySchemes:
    basic:
      type: http
      scheme: basic
";
        let err = load_str(yaml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("basic") && msg.contains("not supported"),
            "expected basic-scheme rejection, got: {msg}"
        );
    }

    #[test]
    fn rejects_api_key_missing_name_or_location() {
        let yaml = "
openapi: 3.0.0
info:
  title: x
  version: '1'
paths: {}
components:
  securitySchemes:
    bad:
      type: apiKey
      in: header
";
        let err = load_str(yaml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("name"),
            "expected missing-name error, got: {msg}"
        );
    }

    #[test]
    fn per_operation_security_overrides_default() {
        // Top-level `security: [bearerAuth]`, but `/health` opts out with
        // an explicit empty list — this is OpenAPI's "make this endpoint
        // public" sentinel and we preserve it as Some(empty), distinct
        // from None ("inherit default").
        let yaml = "
openapi: 3.0.0
info:
  title: x
  version: '1'
paths:
  /health:
    get:
      operationId: health
      security: []
      responses:
        '204':
          description: ok
  /me:
    get:
      operationId: getMe
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema:
                type: string
security:
  - bearerAuth: []
components:
  securitySchemes:
    bearerAuth:
      type: http
      scheme: bearer
";
        let api = load_str(yaml).expect("should parse");
        let by_id: std::collections::HashMap<&str, &Operation> = api
            .operations
            .iter()
            .map(|o| (o.operation_id.as_deref().unwrap_or(""), o))
            .collect();

        let health = by_id["health"];
        assert_eq!(
            health.security.as_deref(),
            Some(&[][..]),
            "Some(empty) preserves the explicit-public sentinel"
        );

        let me = by_id["getMe"];
        assert!(
            me.security.is_none(),
            "absence of per-op security should remain None"
        );

        assert_eq!(api.security, vec!["bearerAuth".to_string()]);
    }
}
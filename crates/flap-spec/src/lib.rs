//! OpenAPI 3.0 loader and lowering pass.
//!
//! Public API: one function, `load`, which reads a YAML file and returns a
//! fully-populated `flap_ir::Api`. Everything inside is private serde plumbing.
//!
//! Design notes:
//! - Raw* types mirror the OpenAPI YAML structure and are only used for parsing.
//! - `lower_*` functions convert Raw* → IR. Errors are propagated with context.
//! - Per DECISIONS D5, OpenAPI 3.1 is rejected up front with a clear message.
//! - Per DECISIONS D6, oneOf/anyOf without a discriminator will be a hard error
//!   once the emitter needs to handle them; for now they're not in PetStore.

use std::collections::BTreeMap;
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
    /// not yet supported — v0.1 covers only inline schemas.
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
}

// ── Lowering pass (Raw* → IR) ─────────────────────────────────────────────────

fn lower(raw: RawSpec) -> Result<Api> {
    let title = raw.info.title;
    let base_url = raw.servers.into_iter().next().map(|s| s.url);

    let operations = lower_operations(raw.paths)?;
    let schemas = lower_schemas(raw.components.schemas)?;

    Ok(Api {
        title,
        base_url,
        operations,
        schemas,
    })
}

fn lower_operations(paths: BTreeMap<String, RawPathItem>) -> Result<Vec<Operation>> {
    // BTreeMap iteration is sorted by path — deterministic.
    let mut ops = Vec::new();
    for (path, item) in paths {
        let pairs: [(HttpMethod, Option<RawOperation>); 8] = [
            (HttpMethod::Delete, item.delete),
            (HttpMethod::Get, item.get),
            (HttpMethod::Head, item.head),
            (HttpMethod::Options, item.options),
            (HttpMethod::Patch, item.patch),
            (HttpMethod::Post, item.post),
            (HttpMethod::Put, item.put),
            (HttpMethod::Trace, item.trace),
        ];
        for (method, maybe_op) in pairs {
            if let Some(raw_op) = maybe_op {
                let parameters = raw_op
                    .parameters
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| {
                        lower_parameter(&path, p)
                            .with_context(|| format!("parameter[{i}] of {method} {path}"))
                    })
                    .collect::<Result<Vec<_>>>()?;

                // Option<Result<T>> → Result<Option<T>>
                let request_body = raw_op
                    .request_body
                    .map(|rb| {
                        lower_request_body(&path, method, rb)
                            .with_context(|| format!("requestBody of {method} {path}"))
                    })
                    .transpose()?;

                ops.push(Operation {
                    method,
                    path: path.clone(),
                    operation_id: raw_op.operation_id,
                    summary: raw_op.summary,
                    parameters,
                    request_body,
                });
            }
        }
    }
    Ok(ops)
}

fn lower_parameter(path: &str, raw: RawParameter) -> Result<Parameter> {
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

    let schema = raw.schema.ok_or_else(|| {
        anyhow!(
            "parameter `{}` in {path} has no `schema` — \
             cannot determine its type",
            raw.name
        )
    })?;

    let type_ref = lower_type_ref(&raw.name, schema)
        .with_context(|| format!("schema of parameter `{}`", raw.name))?;

    Ok(Parameter {
        name: raw.name,
        location,
        type_ref,
        required,
    })
}

fn lower_request_body(path: &str, method: HttpMethod, raw: RawRequestBody) -> Result<RequestBody> {
    let mut content = raw.content;

    // Prefer application/json; fall back to the first entry in BTreeMap order.
    // Using if/else to avoid the borrow-then-move conflict on `content`.
    let (content_type, media_type) = if let Some(mt) = content.remove("application/json") {
        ("application/json".to_string(), mt)
    } else {
        content
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("requestBody of {method} {path} has no content entries"))?
    };

    let schema = media_type.schema.ok_or_else(|| {
        anyhow!(
            "content type `{content_type}` in requestBody of {method} {path} \
             has no schema"
        )
    })?;

    let schema_ref = lower_type_ref("<requestBody>", schema)?;

    Ok(RequestBody {
        content_type,
        schema_ref,
        required: raw.required,
    })
}

fn lower_schemas(raw: BTreeMap<String, RawSchemaOrRef>) -> Result<Vec<Schema>> {
    // BTreeMap iteration is alphabetically sorted — deterministic.
    raw.into_iter()
        .map(|(name, schema_or_ref)| {
            let kind = lower_schema_kind(&name, schema_or_ref)
                .with_context(|| format!("in schema `{name}`"))?;
            Ok(Schema { name, kind })
        })
        .collect()
}

fn lower_schema_kind(name: &str, sor: RawSchemaOrRef) -> Result<SchemaKind> {
    match sor {
        RawSchemaOrRef::Ref { reference } => Err(anyhow!(
            "top-level schema `{name}` is a bare $ref (`{reference}`); \
             aliases are not yet supported in v0.1"
        )),
        RawSchemaOrRef::Inline(raw) => lower_inline_schema(name, raw),
    }
}

fn lower_inline_schema(name: &str, raw: RawSchema) -> Result<SchemaKind> {
    match raw.ty.as_deref() {
        Some("object") | None if !raw.properties.is_empty() => {
            let required: std::collections::HashSet<&str> =
                raw.required.iter().map(String::as_str).collect();

            let fields = raw
                .properties
                .into_iter()
                .map(|(field_name, sor)| {
                    let type_ref = lower_type_ref(&field_name, sor)
                        .with_context(|| format!("field `{field_name}`"))?;
                    let is_required = required.contains(field_name.as_str());
                    Ok(Field {
                        name: field_name,
                        type_ref,
                        required: is_required,
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            Ok(SchemaKind::Object { fields })
        }

        Some("array") => {
            let items = raw
                .items
                .ok_or_else(|| anyhow!("array schema `{name}` is missing `items`"))?;
            let item =
                lower_type_ref("<items>", *items).with_context(|| format!("in `{name}.items`"))?;
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

fn lower_type_ref(field_name: &str, sor: RawSchemaOrRef) -> Result<TypeRef> {
    match sor {
        RawSchemaOrRef::Ref { reference } => {
            // "$ref": "#/components/schemas/Pet" → "Pet"
            let bare = reference
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("malformed $ref `{reference}`"))?;
            Ok(TypeRef::Named(bare.to_string()))
        }
        RawSchemaOrRef::Inline(raw) => match raw.ty.as_deref() {
            Some("string") => Ok(TypeRef::String),
            Some("integer") => Ok(TypeRef::Integer { format: raw.format }),
            Some("number") => Ok(TypeRef::Number { format: raw.format }),
            Some("boolean") => Ok(TypeRef::Boolean),
            Some(other) => Err(anyhow!(
                "field `{field_name}` has unsupported inline type `{other}`"
            )),
            None => Err(anyhow!(
                "field `{field_name}` has no `type` and is not a $ref"
            )),
        },
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

        // POST /pets — no parameters (has requestBody, not modelled yet)
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
}

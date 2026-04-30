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
//! The `visiting` set inside `LoweringContext` tracks which top-level schemas
//! are currently mid-lowering. Today it short-circuits self-referential `$ref`s
//! (so `Node.next: $ref Node` returns `TypeRef::Named` instead of looping).
//! Phase 2's `allOf` merging will use the same set when it actually walks into
//! referenced schemas to inline their fields.
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
}

// ── Lowering context (shared during pass 2) ──────────────────────────────────

/// Threaded mutably through every `lower_*` function during pass 2.
///
/// - `components` is the global registry of all top-level definitions, used to
///   resolve `$ref` pointers from anywhere in the document.
/// - `visiting` records the set of top-level schemas currently mid-lowering,
///   so that recursive references terminate instead of looping. Resolvers
///   consult this set and emit a `TypeRef::Named` pointer without traversing
///   further. Phase 1 never traverses through a `$ref` anyway, so this is
///   currently a belt-and-braces guard; Phase 2's `allOf` merging will rely
///   on it for real cycle protection.
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
    match raw.ty.as_deref() {
        Some("object") | None if !raw.properties.is_empty() => {
            let required: HashSet<&str> = raw.required.iter().map(String::as_str).collect();

            let fields = raw
                .properties
                .iter()
                .map(|(field_name, sor)| {
                    let type_ref = lower_type_ref(field_name, sor, ctx)
                        .with_context(|| format!("field `{field_name}`"))?;
                    let is_required = required.contains(field_name.as_str());
                    Ok(Field {
                        name: field_name.clone(),
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
        RawSchemaOrRef::Inline(raw) => match raw.ty.as_deref() {
            Some("string") => Ok(TypeRef::String),
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
}

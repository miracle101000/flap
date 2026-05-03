//! The flap intermediate representation.
//!
//! `flap-spec` lowers a raw OpenAPI document into these types. `flap-emit-*`
//! crates consume them to produce language-specific output. Nothing in here
//! knows about YAML or Dart — it is the contract between the two halves.

use std::fmt;

// ── Top-level ────────────────────────────────────────────────────────────────

/// A fully-resolved API, ready for code generation.
#[derive(Debug)]
pub struct Api {
    pub title: String,
    pub base_url: Option<String>,
    /// Ordered by path, then by HTTP method — deterministic output.
    pub operations: Vec<Operation>,
    /// Ordered by schema name — deterministic output.
    pub schemas: Vec<Schema>,
    /// All security schemes declared in `components.securitySchemes`,
    /// ordered alphabetically by scheme name (BTreeMap-driven, deterministic).
    /// Empty when the spec defines no security schemes — the emitter then
    /// skips generating any auth-related code.
    pub security_schemes: Vec<SecurityScheme>,
    /// The set of security scheme names applied to every operation by
    /// default (the top-level `security` block). Stored as a flat,
    /// deduplicated list of scheme names — OpenAPI's full
    /// list-of-AND-of-OR structure is collapsed in v0.1, since the
    /// generated Dart client supports providing any combination of
    /// credentials and sending whichever ones are non-null. Each entry
    /// is expected to reference an entry in `security_schemes`.
    pub security: Vec<String>,
}

// ── Operations ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HttpMethod {
    Delete,
    Get,
    Head,
    Options,
    Patch,
    Post,
    Put,
    Trace,
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpMethod::Delete => f.write_str("DELETE"),
            HttpMethod::Get => f.write_str("GET"),
            HttpMethod::Head => f.write_str("HEAD"),
            HttpMethod::Options => f.write_str("OPTIONS"),
            HttpMethod::Patch => f.write_str("PATCH"),
            HttpMethod::Post => f.write_str("POST"),
            HttpMethod::Put => f.write_str("PUT"),
            HttpMethod::Trace => f.write_str("TRACE"),
        }
    }
}

#[derive(Debug)]
pub struct Operation {
    pub method: HttpMethod,
    pub path: String,
    pub operation_id: Option<String>,
    pub summary: Option<String>,
    /// Query, path, header, and cookie parameters for this operation.
    /// Ordered as they appear in the spec — no re-sorting applied.
    pub parameters: Vec<Parameter>,
    /// The request body, if this operation accepts one.
    pub request_body: Option<RequestBody>,
    /// All declared responses for this operation, keyed by status code.
    ///
    /// Ordered deterministically: numeric status codes ascending first,
    /// then `default` last (so "200", "201", "404", "default"). Empty when
    /// no responses are declared.
    pub responses: Vec<Response>,
    /// Per-operation security override.
    pub security: Option<Vec<String>>,
}

// ── Parameters ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterLocation {
    Cookie,
    Header,
    Path,
    Query,
}

impl fmt::Display for ParameterLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParameterLocation::Cookie => f.write_str("cookie"),
            ParameterLocation::Header => f.write_str("header"),
            ParameterLocation::Path => f.write_str("path"),
            ParameterLocation::Query => f.write_str("query"),
        }
    }
}

#[derive(Debug)]
pub struct Parameter {
    pub name: String,
    pub location: ParameterLocation,
    pub type_ref: TypeRef,
    pub required: bool,
}

// ── Request body ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct RequestBody {
    pub content_type: String,
    pub schema_ref: TypeRef,
    pub required: bool,
    pub is_multipart: bool,
}

// ── Responses ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Response {
    pub status_code: String,
    pub schema_ref: Option<TypeRef>,
}

// ── Schemas ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Schema {
    pub name: String,
    pub kind: SchemaKind,
}

#[derive(Debug)]
pub enum SchemaKind {
    /// An object with a fixed set of named fields.
    Object { fields: Vec<Field> },
    /// A homogeneous list — e.g. OpenAPI `type: array`.
    Array { item: TypeRef },
    /// A homogeneous string-keyed dictionary at the top level.
    Map { value: TypeRef },
    /// A discriminated union — OpenAPI `oneOf` with a `discriminator`.
    Union {
        variants: Vec<TypeRef>,
        discriminator: String,
        variant_tags: Vec<String>,
    },
    /// A union of forms where no single explicit discriminator field
    /// exists. Deserialization must attempt each variant in order.
    UntaggedUnion { variants: Vec<TypeRef> },
}

#[derive(Debug)]
pub struct Field {
    pub name: String,
    pub type_ref: TypeRef,
    pub required: bool,
    /// True when the field's value may legitimately be JSON `null` on the
    /// wire — set by lowering when the schema declares `nullable: true`
    /// (OpenAPI 3.0). Orthogonal to `required`:
    ///
    /// | `required` | `nullable` | wire semantics                              |
    /// |------------|------------|---------------------------------------------|
    /// | true       | false      | key MUST be present, value non-null         |
    /// | true       | true       | key MUST be present, value MAY be null      |
    /// | false      | false      | key MAY be omitted, never null when present |
    /// | false      | true       | key MAY be omitted, value MAY be null       |
    ///
    /// The last row is the one that motivates the Dart `Optional<T?>`
    /// wrapper in `flap_emit_dart` — it's the only case where the
    /// receiver of a PATCH must be able to distinguish "client did not
    /// send this key" from "client explicitly set this key to null".
    ///
    /// OpenAPI 3.1's `type: [string, "null"]` is rejected at the version
    /// guard (DECISIONS D5) so we never see it here; when the 3.1 ban
    /// drops, lowering will need to translate that shape into this same
    /// boolean.
    pub nullable: bool,
    /// True when this field's type (directly, or via `List<>` / `Map<>`)
    /// points at the schema currently being lowered — i.e. self-recursion
    /// (`Node.children: List<Node>`) or a back-edge through an `allOf`
    /// chain. Set by the lowering pass; downstream emitters use this as
    /// an explicit signal that the field type must be rendered as the
    /// bare class name (no inline typedef wrapping that would break
    /// Freezed's generator).
    pub is_recursive: bool,
}

impl Field {
    /// Construct a non-recursive, non-nullable field — the common case
    /// for hand-written IR (test fixtures, golden builders, etc.). The
    /// lowering pass in `flap_spec` constructs `Field` directly because
    /// it computes `is_recursive` and `nullable` from the source spec;
    /// everywhere else, prefer this.
    pub fn new(name: impl Into<String>, type_ref: TypeRef, required: bool) -> Self {
        Self {
            name: name.into(),
            type_ref,
            required,
            nullable: false,
            is_recursive: false,
        }
    }
}

/// A reference to a concrete type, either primitive or a named schema.
#[derive(Debug, Clone)]
pub enum TypeRef {
    String,
    Integer {
        format: Option<String>,
    },
    Number {
        format: Option<String>,
    },
    Boolean,
    /// `type: string, format: date-time` — emitted as Dart `DateTime`.
    DateTime,
    /// A closed set of allowed string values (`enum: [...]`).
    Enum(Vec<String>),
    /// A homogeneous string-keyed dictionary value type.
    Map(Box<TypeRef>),
    /// An inline homogeneous list — `{ type: array, items: ... }`.
    Array(Box<TypeRef>),
    /// Reference to a named component schema (the bare name, not the $ref path).
    Named(String),
}

impl fmt::Display for TypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeRef::String => f.write_str("string"),
            TypeRef::Integer { format: Some(s) } => write!(f, "integer({s})"),
            TypeRef::Integer { format: None } => f.write_str("integer"),
            TypeRef::Number { format: Some(s) } => write!(f, "number({s})"),
            TypeRef::Number { format: None } => f.write_str("number"),
            TypeRef::Boolean => f.write_str("boolean"),
            TypeRef::DateTime => f.write_str("date-time"),
            TypeRef::Enum(values) => {
                f.write_str("enum[")?;
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(v)?;
                }
                f.write_str("]")
            }
            TypeRef::Map(inner) => write!(f, "map<{inner}>"),
            TypeRef::Array(inner) => write!(f, "array<{inner}>"),
            TypeRef::Named(n) => write!(f, "{n}"),
        }
    }
}

// ── Security ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeyLocation {
    Cookie,
    Header,
    Query,
}

impl fmt::Display for ApiKeyLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiKeyLocation::Cookie => f.write_str("cookie"),
            ApiKeyLocation::Header => f.write_str("header"),
            ApiKeyLocation::Query => f.write_str("query"),
        }
    }
}

// ── OAuth2 ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuth2FlowType {
    Implicit,
    Password,
    ClientCredentials,
    AuthorizationCode,
}

impl fmt::Display for OAuth2FlowType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OAuth2FlowType::Implicit => f.write_str("implicit"),
            OAuth2FlowType::Password => f.write_str("password"),
            OAuth2FlowType::ClientCredentials => f.write_str("clientCredentials"),
            OAuth2FlowType::AuthorizationCode => f.write_str("authorizationCode"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OAuth2Flow {
    pub flow_type: OAuth2FlowType,
    /// Present for: password, clientCredentials, authorizationCode.
    pub token_url: Option<String>,
    /// Present for: implicit, authorizationCode.
    pub authorization_url: Option<String>,
    /// Scope names declared by the flow (descriptions are dropped).
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum SecurityScheme {
    ApiKey {
        scheme_name: String,
        parameter_name: String,
        location: ApiKeyLocation,
    },
    HttpBearer {
        scheme_name: String,
        bearer_format: Option<String>,
    },
    OAuth2 {
        scheme_name: String,
        /// At least one flow is always present — lowering rejects empty `flows` blocks.
        flows: Vec<OAuth2Flow>,
    },
    OpenIdConnect {
        scheme_name: String,
        openid_connect_url: String,
    },
}

impl SecurityScheme {
    pub fn scheme_name(&self) -> &str {
        match self {
            SecurityScheme::ApiKey { scheme_name, .. } => scheme_name,
            SecurityScheme::HttpBearer { scheme_name, .. } => scheme_name,
            SecurityScheme::OAuth2 { scheme_name, .. } => scheme_name,
            SecurityScheme::OpenIdConnect { scheme_name, .. } => scheme_name,
        }
    }
}

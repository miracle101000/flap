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
    /// no responses are declared (rare — OpenAPI requires at least one,
    /// but we don't enforce that here).
    pub responses: Vec<Response>,
    /// Per-operation security override.
    ///
    /// - `None` means "use the API-level default" (`Api.security`).
    /// - `Some(empty)` means an explicit override to *no* security —
    ///   the OpenAPI sentinel for marking an endpoint public even when
    ///   the rest of the API requires auth.
    /// - `Some(non-empty)` means use these specific scheme names instead
    ///   of the API default.
    ///
    /// The Dart emitter currently treats credentials globally (any
    /// configured credential gets sent on every request via the Dio
    /// interceptor), so this field is captured for fidelity but not yet
    /// used to gate per-operation injection. A future phase can refine
    /// the interceptor to consult per-operation requirements.
    pub security: Option<Vec<String>>,
}

// ── Parameters ────────────────────────────────────────────────────────────────

/// Where a parameter is transmitted in the HTTP request.
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
    /// Path parameters are always required (OpenAPI §4.7.12).
    /// Query/header/cookie parameters use the value from the spec.
    pub required: bool,
}

// ── Request body ─────────────────────────────────────────────────────────────

/// The body sent with a POST / PUT / PATCH request.
///
/// Only the first (preferred) content type is modelled — typically
/// `application/json`, with `multipart/form-data` accepted as a second
/// preference for file-upload endpoints. Multi-body operations beyond
/// the chosen pair are a post-v0.1 concern.
#[derive(Debug)]
pub struct RequestBody {
    /// The MIME type of the body, e.g. `"application/json"` or
    /// `"multipart/form-data"`.
    pub content_type: String,
    /// The schema of the body payload.
    pub schema_ref: TypeRef,
    /// Whether the caller must supply this body. OpenAPI defaults to false,
    /// but `required: true` is strongly encouraged and common in real specs.
    pub required: bool,
    /// True when `content_type` is `multipart/form-data`. The Dart emitter
    /// uses this to choose between sending a JSON-encoded body and building
    /// a Dio `FormData` (which is also what enables file uploads).
    pub is_multipart: bool,
}

// ── Responses ────────────────────────────────────────────────────────────────

/// A single response entry of an operation, keyed by status code.
///
/// `status_code` is kept as a `String` so we can carry both numeric codes
/// (`"200"`, `"404"`) and the OpenAPI sentinel `"default"` without adding
/// an enum that the emitter would just have to map back to a string. The
/// upstream parser is the authority on what counts as a valid key.
///
/// `schema_ref` is `None` when the response declares no body — for example
/// `204 No Content`, or PetStore's `POST /pets` returning `201` with just a
/// `description`. Emitters use this to decide between `Future<T>` and
/// `Future<void>`-shaped return types.
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
}

#[derive(Debug)]
pub struct Field {
    pub name: String,
    pub type_ref: TypeRef,
    pub required: bool,
}

/// A reference to a concrete type, either primitive or a named schema.
///
/// Phase 2 additions: `DateTime`, `Enum`, and `Map`. These extend the set
/// of in-place types that may appear as a field type, parameter schema, or
/// request-body schema. Top-level schemas continue to be either `Object`
/// or `Array` (`SchemaKind`); a top-level enum or map is currently lowered
/// as an inline `TypeRef` only when it appears nested inside a property.
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
    /// Kept as its own variant rather than `String { format: "date-time" }`
    /// because almost every emitter wants to special-case it (parsing,
    /// `toIso8601String()`, JSON converters, etc.).
    DateTime,
    /// A closed set of allowed string values (`enum: [...]`).
    /// Values are stored in spec order — emitters typically preserve that
    /// order in the generated language-level enum so existing serialised
    /// payloads keep working when new values are appended.
    ///
    /// v0.1 only models string enums; non-string enum entries are rejected
    /// during lowering.
    Enum(Vec<String>),
    /// A homogeneous string-keyed dictionary, e.g. an object schema with
    /// `additionalProperties: { type: string }`. The boxed inner `TypeRef`
    /// is the value type; the key is always `String` per JSON semantics.
    Map(Box<TypeRef>),
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
            TypeRef::Named(n) => write!(f, "{n}"),
        }
    }
}

// ── Security ─────────────────────────────────────────────────────────────────

/// Where an API key parameter is sent on the wire.
///
/// Distinct from [`ParameterLocation`] because OpenAPI does not permit
/// `apiKey` schemes in path position — only header, query, or cookie. Keeping
/// this as its own enum stops emitters from having to handle a `Path` arm
/// that can never legitimately occur.
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

/// A security scheme declared in `components.securitySchemes`.
///
/// v0.1 supports the two most common shapes:
///
/// - `apiKey` — a credential transmitted as a header, query parameter, or
///   cookie under a fixed wire-side name.
/// - `http` with `scheme: bearer` — RFC 6750 bearer tokens, sent as
///   `Authorization: Bearer <token>`.
///
/// `oauth2` and `openIdConnect` flows are out of scope for v0.1 — the
/// generated client would need a token-acquisition step that doesn't
/// belong in a single-method interceptor. They will be modelled in a
/// future phase that adds a real auth-flow IR.
#[derive(Debug, Clone)]
pub enum SecurityScheme {
    /// `type: apiKey` — a static credential the caller supplies once.
    ApiKey {
        /// The key under which this scheme is registered in
        /// `components.securitySchemes`. Used as a stable handle by both
        /// `Api.security` / `Operation.security` references and as the
        /// basis for the emitted Dart constructor parameter name.
        scheme_name: String,
        /// The wire-side name of the header / query parameter / cookie,
        /// e.g. `"X-API-Key"` or `"api_key"`.
        parameter_name: String,
        location: ApiKeyLocation,
    },
    /// `type: http`, `scheme: bearer` — a bearer token sent in the
    /// `Authorization` header.
    HttpBearer {
        scheme_name: String,
        /// Optional hint about the token format (e.g. `"JWT"`). Carried
        /// through unchanged for documentation; v0.1 emitters do not vary
        /// behaviour on it.
        bearer_format: Option<String>,
    },
}

impl SecurityScheme {
    /// Returns the registry key under `components.securitySchemes`.
    /// Common to every variant — handy for cross-referencing with
    /// `Api.security` / `Operation.security`.
    pub fn scheme_name(&self) -> &str {
        match self {
            SecurityScheme::ApiKey { scheme_name, .. } => scheme_name,
            SecurityScheme::HttpBearer { scheme_name, .. } => scheme_name,
        }
    }
}

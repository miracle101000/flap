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
            TypeRef::Named(n) => write!(f, "{n}"),
        }
    }
}

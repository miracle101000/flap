use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

// The top‑level document
#[derive(Debug, Deserialize)]
pub struct SwaggerSpec {
    #[allow(dead_code)]
    pub swagger: String, // "2.0"
    pub(crate) info: SwaggerInfo,
    pub host: Option<String>,
    #[serde(rename = "basePath")]
    pub base_path: Option<String>,
    #[serde(default)]
    pub paths: BTreeMap<String, SwaggerPathItem>,
    #[serde(default)]
    pub definitions: BTreeMap<String, SwaggerSchemaOrRef>,
    #[serde(default, rename = "securityDefinitions")]
    pub security_definitions: BTreeMap<String, SwaggerSecurityDefinition>,
    #[serde(default)]
    pub security: Vec<BTreeMap<String, Vec<String>>>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Clone)]
pub struct SwaggerContext<'a> {
    pub definitions: &'a BTreeMap<String, SwaggerSchemaOrRef>,
    pub visiting: HashSet<String>,
}

impl<'a> SwaggerContext<'a> {
    pub fn new(definitions: &'a BTreeMap<String, SwaggerSchemaOrRef>) -> Self {
        Self {
            definitions,
            visiting: HashSet::new(),
        }
    }
    pub fn with_visiting(&self, name: &str) -> Self {
        let mut child = self.clone();
        child.visiting.insert(name.to_string());
        child
    }
}

#[derive(Debug, Deserialize)]
pub struct SwaggerInfo {
    pub(crate) title: String,
}

// A path item
#[derive(Debug, Default, Deserialize)]
pub struct SwaggerPathItem {
    pub get: Option<SwaggerOperation>,
    pub put: Option<SwaggerOperation>,
    pub post: Option<SwaggerOperation>,
    pub delete: Option<SwaggerOperation>,
    pub options: Option<SwaggerOperation>,
    pub head: Option<SwaggerOperation>,
    pub patch: Option<SwaggerOperation>,
    // Top‑level parameters (inherited by all operations)
    #[serde(default)]
    pub parameters: Vec<SwaggerParameter>,
}

// Operation
#[derive(Debug, Deserialize)]
pub struct SwaggerOperation {
    #[serde(rename = "operationId")]
    pub operation_id: Option<String>,
    pub summary: Option<String>,
    #[serde(default)]
    pub parameters: Vec<SwaggerParameter>,
    #[serde(default)]
    pub responses: BTreeMap<String, SwaggerResponse>,
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub security: Option<Vec<BTreeMap<String, Vec<String>>>>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

// Parameter (2.0 style: type lives at top level, not in a schema)
#[derive(Debug, Deserialize)]
pub struct SwaggerParameter {
    pub name: String,
    #[serde(rename = "in")]
    pub location: String, // query, header, path, formData, body
    #[serde(default)]
    pub required: bool,
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub format: Option<String>,
    #[serde(default)]
    pub items: Option<Box<SwaggerItems>>,
    #[serde(default, rename = "enum")]
    pub enum_values: Vec<serde_yaml::Value>,
    // For 'body' parameter
    pub schema: Option<SwaggerSchemaOrRef>,
    // For 'in: formData' the type is inline, we'll treat it like a query param
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

// Items for arrays inside parameters
#[derive(Debug, Deserialize)]
pub struct SwaggerItems {
    #[serde(rename = "type")]
    pub ty: String,
    pub format: Option<String>,
    // ... could nest, but we'll keep simple for now
}

// Response
#[derive(Debug, Deserialize)]
pub struct SwaggerResponse {
    pub description: Option<String>,
    pub schema: Option<SwaggerSchemaOrRef>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
    // Could also have headers, etc.
}

// Schema or $ref (reused from existing pattern, but adapted)
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SwaggerSchemaOrRef {
    Ref {
        #[serde(rename = "$ref")]
        reference: String,
    },
    Inline(Box<SwaggerSchema>),
}

#[derive(Debug, Default, Deserialize)]
pub struct SwaggerSchema {
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub format: Option<String>,
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, SwaggerSchemaOrRef>,
    pub items: Option<Box<SwaggerSchemaOrRef>>,
    #[serde(default, rename = "enum")]
    pub enum_values: Vec<serde_yaml::Value>,
    #[serde(rename = "additionalProperties")]
    pub additional_properties: Option<SwaggerAdditionalProperties>,
    #[serde(default, rename = "allOf")]
    pub all_of: Vec<SwaggerSchemaOrRef>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SwaggerAdditionalProperties {
    Bool(bool),
    Schema(Box<SwaggerSchemaOrRef>),
}

// Security definitions
#[derive(Debug, Deserialize)]
pub struct SwaggerSecurityDefinition {
    #[serde(rename = "type")]
    pub ty: String, // "apiKey", "basic", "oauth2"
    pub name: Option<String>,
    #[serde(rename = "in")]
    pub location: Option<String>,
    // oauth2 flows
    #[serde(rename = "flow")]
    pub flow: Option<String>,
    #[serde(rename = "authorizationUrl")]
    pub authorization_url: Option<String>,
    #[serde(rename = "tokenUrl")]
    pub token_url: Option<String>,
    #[serde(default)]
    pub scopes: BTreeMap<String, String>,
}

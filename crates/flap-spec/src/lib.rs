//! Minimal OpenAPI 3.0 spec loader.
//!
//! Models only what's needed to count operations and schemas. Everything
//! else in the spec is preserved as opaque `serde_yaml::Value` or ignored.
//! Per DECISIONS.md D5, 3.1 is out of scope for v0.1.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Spec {
    #[serde(default)]
    pub paths: BTreeMap<String, PathItem>,
    #[serde(default)]
    pub components: Components,
}

#[derive(Debug, Default, Deserialize)]
pub struct Components {
    #[serde(default)]
    pub schemas: BTreeMap<String, serde_yaml::Value>,
}

/// A path item. Each HTTP method is opaque for now — we only count presence.
#[derive(Debug, Default, Deserialize)]
pub struct PathItem {
    pub get: Option<serde_yaml::Value>,
    pub put: Option<serde_yaml::Value>,
    pub post: Option<serde_yaml::Value>,
    pub delete: Option<serde_yaml::Value>,
    pub patch: Option<serde_yaml::Value>,
    pub options: Option<serde_yaml::Value>,
    pub head: Option<serde_yaml::Value>,
    pub trace: Option<serde_yaml::Value>,
}

impl PathItem {
    pub fn operation_count(&self) -> usize {
        [
            &self.get,
            &self.put,
            &self.post,
            &self.delete,
            &self.patch,
            &self.options,
            &self.head,
            &self.trace,
        ]
        .iter()
        .filter(|op| op.is_some())
        .count()
    }
}

impl Spec {
    pub fn operation_count(&self) -> usize {
        self.paths.values().map(PathItem::operation_count).sum()
    }

    pub fn schema_count(&self) -> usize {
        self.components.schemas.len()
    }
}

pub fn load(path: impl AsRef<Path>) -> Result<Spec> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading spec file {}", path.display()))?;
    let spec: Spec = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing OpenAPI YAML from {}", path.display()))?;
    Ok(spec)
}

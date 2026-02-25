use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::validate::cached_regex;

#[derive(Clone, Debug)]
enum Node {
    Object(BTreeMap<String, ObjectTree>),
    Array(Vec<ObjectTree>),
    Scalar(Value),
}

/// Schema-defined object value. Data + schema are one identity.
#[derive(Clone, Debug)]
pub struct ObjectTree {
    node: Node,
    schema: Option<Arc<Value>>,
    root: Option<Arc<Value>>,
}

impl ObjectTree {
    /// Create with data and schema.
    pub fn new(data: Value, schema: Value) -> Self {
        let schema = Arc::new(schema);
        let root = Some(schema.clone());
        Self::from_value(data, Some(schema), root)
    }

    /// Create without schema (no validation).
    pub fn without_schema(data: Value) -> Self {
        Self::from_value(data, None, None)
    }

    fn from_value(data: Value, schema: Option<Arc<Value>>, root: Option<Arc<Value>>) -> Self {
        let node = match data {
            Value::Object(map) => {
                let mut out = BTreeMap::new();
                for (k, v) in map {
                    let sub_schema = sub_schema_for_key_from(schema.as_deref(), root.as_deref(), &k);
                    out.insert(k, ObjectTree::from_value(v, sub_schema, root.clone()));
                }
                Node::Object(out)
            }
            Value::Array(arr) => {
                let mut out = Vec::with_capacity(arr.len());
                for (idx, v) in arr.into_iter().enumerate() {
                    let sub_schema = sub_schema_for_index_from(schema.as_deref(), root.as_deref(), idx);
                    out.push(ObjectTree::from_value(v, sub_schema, root.clone()));
                }
                Node::Array(out)
            }
            v => Node::Scalar(v),
        };
        Self { node, schema, root }
    }

    /// Borrow the schema if present.
    pub fn schema(&self) -> Option<&Value> {
        self.schema.as_deref()
    }

    pub(crate) fn root_schema(&self) -> Option<&Value> {
        self.root.as_deref()
    }

    /// Return schema as plain JSON (full schema).
    pub fn get_schema(&self, path: Option<&str>) -> Option<Value> {
        let schema = self.schema.as_deref()?;
        let schema = super::validate::resolve_ref_if_needed(schema, self.root_schema())?;
        if path.is_none() || path == Some("") {
            return Some(schema.clone());
        }
        let mut node = schema;
        for part in path.unwrap().split('.') {
            if part.is_empty() {
                continue;
            }
            let props = node.get("properties")?.as_object()?;
            node = props.get(part)?;
        }
        Some(node.clone())
    }

    /// Return x-* extensions from schema (optionally by dot path).
    pub fn get_extensions(&self, path: Option<&str>) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        let Some(node) = self.get_schema(path) else { return out };
        let Some(obj) = node.as_object() else { return out };
        for (k, v) in obj {
            if k.starts_with("x-") {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// True if the inner value is null.
    pub fn is_null(&self) -> bool {
        matches!(self.node, Node::Scalar(Value::Null))
    }

    /// True if the inner value is a boolean.
    pub fn is_boolean(&self) -> bool {
        matches!(self.node, Node::Scalar(Value::Bool(_)))
    }

    /// True if the inner value is a number.
    pub fn is_number(&self) -> bool {
        matches!(self.node, Node::Scalar(Value::Number(_)))
    }

    /// True if the inner value is a string.
    pub fn is_string(&self) -> bool {
        matches!(self.node, Node::Scalar(Value::String(_)))
    }

    /// True if the inner value is an array.
    pub fn is_array(&self) -> bool {
        matches!(self.node, Node::Array(_))
    }

    /// True if the inner value is an object.
    pub fn is_object(&self) -> bool {
        matches!(self.node, Node::Object(_))
    }

    /// Extract as bool.
    pub fn as_bool(&self) -> Option<bool> {
        match &self.node {
            Node::Scalar(Value::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    /// Extract as i64.
    pub fn as_i64(&self) -> Option<i64> {
        match &self.node {
            Node::Scalar(Value::Number(n)) => n.as_i64(),
            _ => None,
        }
    }

    /// Extract as u64.
    pub fn as_u64(&self) -> Option<u64> {
        match &self.node {
            Node::Scalar(Value::Number(n)) => n.as_u64(),
            _ => None,
        }
    }

    /// Extract as f64.
    pub fn as_f64(&self) -> Option<f64> {
        match &self.node {
            Node::Scalar(Value::Number(n)) => n.as_f64(),
            _ => None,
        }
    }

    /// Extract as string slice.
    pub fn as_str(&self) -> Option<&str> {
        match &self.node {
            Node::Scalar(Value::String(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    // --- Mutation with validation ---

    /// Set a value by key. Validates against schema before writing.
    pub fn set(&mut self, key: &str, value: Value) -> Result<(), super::ValidationError> {
        let schema = self.schema.clone();
        let root = self.root.clone();
        let Node::Object(map) = &mut self.node else {
            return Err(super::ValidationError::root(
                super::ErrorKind::NotAnObject,
                "cannot set key on non-object value",
            ));
        };

        if let Some(sub) = sub_schema_for_key_from(schema.as_deref(), root.as_deref(), key) {
            super::validate::check_field(key, &sub, &value, root.as_deref())?;
            map.insert(key.to_string(), ObjectTree::from_value(value, Some(sub), root.clone()));
        } else {
            map.insert(key.to_string(), ObjectTree::from_value(value, None, root.clone()));
        }
        Ok(())
    }

    /// Set array element by index. Validates against items schema if present.
    pub fn set_index(&mut self, idx: usize, value: Value) -> Result<(), super::ValidationError> {
        let schema = self.schema.clone();
        let root = self.root.clone();
        let Node::Array(arr) = &mut self.node else {
            return Err(super::ValidationError::root(
                super::ErrorKind::NotAnObject,
                "cannot set index on non-array value",
            ));
        };
        if let Some(sub) = sub_schema_for_index_from(schema.as_deref(), root.as_deref(), idx) {
            super::validate::check_field(&format!("[{idx}]"), &sub, &value, root.as_deref())?;
            if idx < arr.len() {
                arr[idx] = ObjectTree::from_value(value, Some(sub), root.clone());
            } else {
                arr.push(ObjectTree::from_value(value, Some(sub), root.clone()));
            }
        } else if idx < arr.len() {
            arr[idx] = ObjectTree::from_value(value, None, root.clone());
        } else {
            arr.push(ObjectTree::from_value(value, None, root.clone()));
        }
        Ok(())
    }

    /// Apply default values from schema, returning a new ObjectTree.
    pub fn with_defaults(self) -> Self {
        let Some(schema) = &self.schema else { return self };
        let mut data = self.to_value();
        super::defaults::apply_defaults(&mut data, schema.as_ref());
        ObjectTree::from_value(data, Some(schema.clone()), self.root.clone())
    }

    /// Validate the entire data against the attached schema.
    pub fn validate(&self) -> Result<(), Vec<super::ValidationError>> {
        let schema = match &self.schema {
            Some(s) => s,
            None => return Ok(()),
        };
        super::validate::check_value_with_root(schema, &self.to_value(), self.root_schema()).map_err(|e| vec![e])
    }

    // --- Access ---

    /// Get a child by object key.
    pub fn get(&self, key: &str) -> Option<&ObjectTree> {
        match &self.node {
            Node::Object(map) => map.get(key),
            _ => None,
        }
    }

    /// Get a child by object key (mutable).
    pub fn get_mut(&mut self, key: &str) -> Option<&mut ObjectTree> {
        match &mut self.node {
            Node::Object(map) => map.get_mut(key),
            _ => None,
        }
    }

    /// Get a child by array index.
    pub fn get_index(&self, idx: usize) -> Option<&ObjectTree> {
        match &self.node {
            Node::Array(arr) => arr.get(idx),
            _ => None,
        }
    }

    /// Navigate a dot-separated path (e.g., "user.profile.name").
    pub fn path(&self, dot_path: &str) -> Option<&ObjectTree> {
        let mut current = self;
        for segment in dot_path.split('.') {
            if let Ok(idx) = segment.parse::<usize>() {
                current = current.get_index(idx)?;
            } else {
                current = current.get(segment)?;
            }
        }
        Some(current)
    }

    /// Number of entries (object) or elements (array). Returns 0 for scalars.
    pub fn len(&self) -> usize {
        match &self.node {
            Node::Object(map) => map.len(),
            Node::Array(arr) => arr.len(),
            _ => 0,
        }
    }

    /// True if null, empty object, or empty array.
    pub fn is_empty(&self) -> bool {
        match &self.node {
            Node::Scalar(Value::Null) => true,
            Node::Object(map) => map.is_empty(),
            Node::Array(arr) => arr.is_empty(),
            _ => false,
        }
    }

    /// Iterate over object entries as `(key, ObjectTree)` pairs.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &ObjectTree)> {
        match &self.node {
            Node::Object(map) => map.iter().map(|(k, v)| (k.as_str(), v)).collect::<Vec<_>>().into_iter(),
            _ => Vec::new().into_iter(),
        }
    }

    /// Iterate over array elements as `ObjectTree`.
    pub fn elements(&self) -> impl Iterator<Item = &ObjectTree> {
        match &self.node {
            Node::Array(arr) => arr.iter().collect::<Vec<_>>().into_iter(),
            _ => Vec::new().into_iter(),
        }
    }

    /// Full data (includes unknown fields).
    pub fn to_value(&self) -> Value {
        match &self.node {
            Node::Scalar(v) => v.clone(),
            Node::Array(arr) => Value::Array(arr.iter().map(|v| v.to_value()).collect()),
            Node::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, v) in map {
                    out.insert(k.clone(), v.to_value());
                }
                Value::Object(out)
            }
        }
    }

    /// Schema-defined data only (unknown fields excluded).
    pub fn to_dict(&self) -> Value {
        match &self.node {
            Node::Scalar(v) => v.clone(),
            Node::Array(arr) => Value::Array(arr.iter().map(|v| v.to_dict()).collect()),
            Node::Object(map) => {
                let Some(schema) = &self.schema else {
                    return self.to_value();
                };
                let props = match schema.get("properties").and_then(|p| p.as_object()) {
                    Some(p) => p,
                    None => return self.to_value(),
                };
                let mut out = serde_json::Map::new();
                for key in props.keys() {
                    if let Some(v) = map.get(key) {
                        out.insert(key.clone(), v.to_dict());
                    }
                }
                Value::Object(out)
            }
        }
    }

}

/// Free function for sub-schema extraction (used by iterators that can't borrow self).
fn sub_schema_for_key_from(schema: Option<&Value>, root: Option<&Value>, key: &str) -> Option<Arc<Value>> {
    let schema = schema?;
    let schema = super::validate::resolve_ref_if_needed(schema, root)?;
    if let Some(prop_schema) = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .and_then(|p| p.get(key))
    {
        return Some(Arc::new(prop_schema.clone()));
    }
    if let Some(patterns) = schema.get("patternProperties").and_then(|p| p.as_object()) {
        for (pattern, pat_schema) in patterns {
            if let Ok(re) = cached_regex(pattern) {
                if re.is_match(key) {
                    return Some(Arc::new(pat_schema.clone()));
                }
            }
        }
    }
    if let Some(additional) = schema.get("additionalProperties") {
        if additional.is_object() {
            return Some(Arc::new(additional.clone()));
        }
    }
    None
}

fn sub_schema_for_index_from(schema: Option<&Value>, root: Option<&Value>, idx: usize) -> Option<Arc<Value>> {
    let schema = schema?;
    let schema = super::validate::resolve_ref_if_needed(schema, root)?;
    let items = schema.get("items")?;
    if let Some(arr) = items.as_array() {
        if let Some(item_schema) = arr.get(idx) {
            return Some(Arc::new(item_schema.clone()));
        }
        if let Some(additional) = schema.get("additionalItems") {
            if additional.is_object() {
                return Some(Arc::new(additional.clone()));
            }
        }
        return None;
    }
    if items.is_object() {
        Some(Arc::new(items.clone()))
    } else {
        None
    }
}

// --- From / Into ---

impl From<Value> for ObjectTree {
    fn from(value: Value) -> Self {
        Self::without_schema(value)
    }
}

impl From<ObjectTree> for Value {
    fn from(sv: ObjectTree) -> Self {
        sv.to_value()
    }
}

// --- Display ---

impl fmt::Display for ObjectTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_value())
    }
}

// --- PartialEq (compare data only, ignore schema) ---

impl PartialEq for ObjectTree {
    fn eq(&self, other: &Self) -> bool {
        self.to_value() == other.to_value()
    }
}

impl PartialEq<Value> for ObjectTree {
    fn eq(&self, other: &Value) -> bool {
        &self.to_value() == other
    }
}

// --- Serialize / Deserialize (delegates to data) ---

impl Serialize for ObjectTree {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ObjectTree {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let data = Value::deserialize(deserializer)?;
        Ok(Self::without_schema(data))
    }
}

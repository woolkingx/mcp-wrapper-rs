use serde_json::Value;

use super::error::{ErrorKind, ValidationError};
use super::ObjectTree;

/// Check if data matches a sub-schema (no error details needed).
fn matches(schema: &Value, data: &Value, root: Option<&Value>) -> bool {
    super::validate::check_value_with_root(schema, data, root).is_ok()
}

impl ObjectTree {
    /// `oneOf` (XOR): select the unique matching branch from `schema.oneOf`.
    pub fn one_of(&self) -> Result<ObjectTree, ValidationError> {
        let subs = self.schema_array_keyword("oneOf");
        if subs.is_empty() {
            return Ok(self.clone());
        }

        let data = self.to_value();
        let matched: Vec<&Value> = subs.iter().filter(|s| matches(s, &data, self.root_schema())).collect();
        match matched.len() {
            0 => Err(ValidationError::root(
                ErrorKind::OneOfNoneMatch,
                "oneOf: no branch matches",
            )),
            1 => Ok(ObjectTree::new(data, matched[0].clone())),
            n => Err(ValidationError::root(
                ErrorKind::OneOfMultipleMatch,
                format!("oneOf: expected 1 match, got {n}"),
            )),
        }
    }

    /// `anyOf` (OR): return all matching branches from `schema.anyOf`.
    pub fn any_of(&self) -> Result<Vec<ObjectTree>, ValidationError> {
        let subs = self.schema_array_keyword("anyOf");
        if subs.is_empty() {
            return Ok(vec![self.clone()]);
        }

        let data = self.to_value();
        let results: Vec<ObjectTree> = subs
            .iter()
            .filter(|s| matches(s, &data, self.root_schema()))
            .map(|s| ObjectTree::new(data.clone(), s.clone()))
            .collect();

        if results.is_empty() {
            Err(ValidationError::root(
                ErrorKind::AnyOfNoneMatch,
                "anyOf: no branch matches",
            ))
        } else {
            Ok(results)
        }
    }

    /// `allOf` (AND): deep-merge all sub-schemas from `schema.allOf`.
    pub fn all_of(&self) -> ObjectTree {
        let schema = match self.schema() {
            Some(s) => s,
            None => return self.clone(),
        };

        let subs = match schema.get("allOf").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return self.clone(),
        };

        let sources: Vec<&Value> = std::iter::once(schema).chain(subs.iter()).collect();
        let merged = sources.into_iter().fold(Value::Object(serde_json::Map::new()), deep_merge);
        ObjectTree::new(self.to_value(), merged)
    }

    /// `not` (EXCEPT): true if data does NOT match `schema.not`.
    pub fn not_of(&self) -> bool {
        let target = self.schema().and_then(|s| s.get("not"));
        match target {
            Some(s) => !matches(s, &self.to_value(), self.root_schema()),
            None => true,
        }
    }

    /// `if/then/else` (CASE WHEN): conditional branching.
    pub fn if_then(&self) -> ObjectTree {
        let schema = match self.schema() {
            Some(s) => s,
            None => return self.clone(),
        };

        let if_schema = match schema.get("if") {
            Some(s) => s,
            None => return self.clone(),
        };

        let branch = if matches(if_schema, &self.to_value(), self.root_schema()) {
            schema.get("then")
        } else {
            schema.get("else")
        };

        match branch {
            Some(b) => ObjectTree::new(self.to_value(), b.clone()),
            None => self.clone(),
        }
    }

    /// `project` (SELECT): keep only schema-defined properties.
    pub fn project(&self) -> Result<ObjectTree, ValidationError> {
        let schema = match self.schema() {
            Some(s) => s,
            None => return Ok(self.clone()),
        };

        let resolved = self.resolve_single_branch(schema)?;
        let resolved_ref = resolved.as_ref().unwrap_or(schema);

        let props = match resolved_ref.get("properties").and_then(|v| v.as_object()) {
            Some(p) => p,
            None => return Ok(self.clone()),
        };

        let data = self.to_value();
        let data_obj = match data.as_object() {
            Some(o) => o,
            None => return Ok(self.clone()),
        };

        let filtered: serde_json::Map<String, Value> = props
            .keys()
            .filter_map(|k| data_obj.get(k).map(|v| (k.clone(), v.clone())))
            .collect();

        Ok(ObjectTree::new(Value::Object(filtered), resolved_ref.clone()))
    }

    /// `contains` (EXISTS): true if any array element matches the schema.
    pub fn contains(&self, schema: Option<&Value>) -> bool {
        let target = schema.or_else(|| self.schema().and_then(|s| s.get("contains")));
        let target = match target {
            Some(s) => s,
            None => return false,
        };
        let data = self.to_value();
        let arr = match data.as_array() {
            Some(a) => a,
            None => return false,
        };
        arr.iter().any(|item| matches(target, item, self.root_schema()))
    }

    // --- Helpers ---

    fn schema_array_keyword(&self, keyword: &str) -> Vec<Value> {
        self.schema()
            .and_then(|s| s.get(keyword))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    fn resolve_single_branch(&self, schema: &Value) -> Result<Option<Value>, ValidationError> {
        let data = self.to_value();
        for keyword in ["oneOf", "anyOf"] {
            if let Some(subs) = schema.get(keyword).and_then(|v| v.as_array()) {
                let matched: Vec<&Value> = subs.iter().filter(|s| matches(s, &data, self.root_schema())).collect();
                match matched.len() {
                    0 => {}
                    1 => return Ok(Some(matched[0].clone())),
                    n => {
                        let method = keyword.replace("Of", "_of");
                        return Err(ValidationError::root(
                            ErrorKind::OneOfMultipleMatch,
                            format!(
                                "project: {keyword} has {n} matches, use {method}() first"
                            ),
                        ));
                    }
                }
            }
        }
        Ok(None)
    }
}

fn deep_merge(a: Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Object(mut ao), Value::Object(bo)) => {
            for (k, bv) in bo {
                match k.as_str() {
                    "properties" => {
                        let merged = match (ao.remove(k), bv) {
                            (Some(Value::Object(mut ap)), Value::Object(bp)) => {
                                for (pk, pv) in bp {
                                    let next = if let Some(existing) = ap.remove(pk) {
                                        deep_merge(existing, pv)
                                    } else {
                                        pv.clone()
                                    };
                                    ap.insert(pk.clone(), next);
                                }
                                Value::Object(ap)
                            }
                            (_, v) => v.clone(),
                        };
                        ao.insert(k.clone(), merged);
                    }
                    "required" => {
                        let mut set = std::collections::BTreeSet::new();
                        if let Some(Value::Array(ar)) = ao.remove(k) {
                            for v in ar {
                                if let Some(s) = v.as_str() {
                                    set.insert(s.to_string());
                                }
                            }
                        }
                        if let Value::Array(br) = bv {
                            for v in br {
                                if let Some(s) = v.as_str() {
                                    set.insert(s.to_string());
                                }
                            }
                        }
                        let merged = Value::Array(set.into_iter().map(Value::String).collect());
                        ao.insert(k.clone(), merged);
                    }
                    _ => {
                        let merged = match ao.remove(k) {
                            Some(existing) => deep_merge(existing, bv),
                            None => bv.clone(),
                        };
                        ao.insert(k.clone(), merged);
                    }
                }
            }
            Value::Object(ao)
        }
        (_, v) => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_one_of_unique() {
        let schema = json!({
            "oneOf": [
                {"properties": {"type": {"const": "user"}, "name": {"type": "string"}}},
                {"properties": {"type": {"const": "bot"}, "id": {"type": "integer"}}}
            ]
        });
        let sv = ObjectTree::new(json!({"type": "user", "name": "Alice"}), schema);
        let branch = sv.one_of().unwrap();
        assert!(branch.schema().is_some());
    }
}

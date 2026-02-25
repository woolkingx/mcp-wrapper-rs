use serde_json::Value;

/// Recursively apply default values from schema to data.
///
/// For each property in `schema.properties` that has a `default` keyword
/// and is missing from `data`, inserts the default value.
/// Recurses into nested objects.
pub fn apply_defaults(data: &mut Value, schema: &Value) {
    let props = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return,
    };

    let obj = match data.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    for (key, prop_schema) in props {
        if obj.contains_key(key) {
            // Recurse into existing nested objects
            if let Some(child) = obj.get_mut(key) {
                if child.is_object() && prop_schema.is_object() {
                    apply_defaults(child, prop_schema);
                }
            }
        } else if let Some(default) = prop_schema.get("default") {
            obj.insert(key.clone(), default.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_fill_simple_defaults() {
        let schema = json!({
            "properties": {
                "status": {"type": "string", "default": "pending"},
                "priority": {"type": "integer", "default": 0}
            }
        });
        let mut data = json!({});
        apply_defaults(&mut data, &schema);
        assert_eq!(data["status"], json!("pending"));
        assert_eq!(data["priority"], json!(0));
    }

    #[test]
    fn test_existing_values_not_overwritten() {
        let schema = json!({
            "properties": {
                "status": {"type": "string", "default": "pending"}
            }
        });
        let mut data = json!({"status": "active"});
        apply_defaults(&mut data, &schema);
        assert_eq!(data["status"], json!("active"));
    }

    #[test]
    fn test_nested_defaults() {
        let schema = json!({
            "properties": {
                "config": {
                    "type": "object",
                    "properties": {
                        "retries": {"type": "integer", "default": 3}
                    }
                }
            }
        });
        let mut data = json!({"config": {}});
        apply_defaults(&mut data, &schema);
        assert_eq!(data["config"]["retries"], json!(3));
    }

    #[test]
    fn test_no_schema_properties() {
        let schema = json!({"type": "object"});
        let mut data = json!({"a": 1});
        apply_defaults(&mut data, &schema);
        assert_eq!(data, json!({"a": 1})); // unchanged
    }

    #[test]
    fn test_non_object_data() {
        let schema = json!({"properties": {"x": {"default": 1}}});
        let mut data = json!(42);
        apply_defaults(&mut data, &schema);
        assert_eq!(data, json!(42)); // unchanged
    }

    #[test]
    fn test_mixed_present_and_missing() {
        let schema = json!({
            "properties": {
                "a": {"type": "integer", "default": 10},
                "b": {"type": "string", "default": "hello"},
                "c": {"type": "boolean"}
            }
        });
        let mut data = json!({"b": "world"});
        apply_defaults(&mut data, &schema);
        assert_eq!(data["a"], json!(10));       // filled
        assert_eq!(data["b"], json!("world"));  // kept
        assert_eq!(data["c"], Value::Null);     // no default in schema
    }
}

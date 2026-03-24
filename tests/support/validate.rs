use regex::Regex;
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

use super::error::{ErrorKind, ValidationError};

/// Resolve $ref against root schema (JSON Pointer only).
pub fn resolve_ref_if_needed<'a>(schema: &'a Value, root: Option<&'a Value>) -> Option<&'a Value> {
    if let Some(r) = schema.get("$ref").and_then(|v| v.as_str()) {
        if let Some(root) = root {
            if let Ok(resolved) = resolve_ref_value(r, root) {
                return Some(resolved);
            }
        }
        return None;
    }
    Some(schema)
}

fn resolve_ref_value<'a>(r: &str, root: &'a Value) -> Result<&'a Value, ValidationError> {
    if !r.starts_with("#/") {
        return Err(ValidationError::root(
            ErrorKind::TypeError,
            format!("unsupported $ref: {r}"),
        ));
    }
    let mut node = root;
    for part in r[2..].split('/') {
        let part = part.replace("~1", "/").replace("~0", "~");
        node = node.get(&part).ok_or_else(|| {
            ValidationError::root(ErrorKind::TypeError, format!("$ref not found: {r}"))
        })?;
    }
    Ok(node)
}

/// Check a value against a JSON Schema Draft-07 schema.
///
/// Returns `Ok(())` on success or `Err(ValidationError)` on first failure.
pub fn check_value(schema: &Value, value: &Value) -> Result<(), ValidationError> {
    check_value_with_root(schema, value, None)
}

pub fn check_value_with_root(schema: &Value, value: &Value, root: Option<&Value>) -> Result<(), ValidationError> {
    // Boolean schema: true accepts everything, false rejects everything
    if let Some(b) = schema.as_bool() {
        return if b {
            Ok(())
        } else {
            Err(ValidationError::root(
                ErrorKind::TypeError,
                "schema is false — all values are rejected",
            ))
        };
    }

    let schema = resolve_ref_if_needed(schema, root).unwrap_or(schema);
    let obj = match schema.as_object() {
        Some(o) => o,
        None => return Ok(()), // non-object schema = no constraints
    };

    // --- type ---
    if let Some(type_spec) = obj.get("type") {
        check_type(type_spec, value)?;
    }

    // --- const ---
    if let Some(c) = obj.get("const") {
        if value != c {
            return Err(ValidationError::root(
                ErrorKind::ConstMismatch,
                format!("expected {c}, got {value}"),
            ));
        }
    }

    // --- enum ---
    if let Some(Value::Array(variants)) = obj.get("enum") {
        if !variants.contains(value) {
            return Err(ValidationError::root(
                ErrorKind::EnumMismatch,
                format!("{value} not in enum"),
            ));
        }
    }

    // --- numeric constraints ---
    if let Some(n) = value_as_f64(value) {
        check_numeric(obj, n)?;
    }

    // --- string constraints ---
    if let Some(s) = value.as_str() {
        check_string(obj, s)?;
    }

    // --- array constraints ---
    if let Some(arr) = value.as_array() {
        check_array(obj, arr, root)?;
    }

    // --- object constraints ---
    if let Some(map) = value.as_object() {
        check_object(obj, map, value, root)?;
    }

    // --- not ---
    if let Some(not_schema) = obj.get("not") {
        if check_value_with_root(not_schema, value, root).is_ok() {
            return Err(ValidationError::root(
                ErrorKind::TypeError,
                "value must NOT match 'not' schema",
            ));
        }
    }

    // --- oneOf ---
    if let Some(Value::Array(subs)) = obj.get("oneOf") {
        let match_count = subs.iter().filter(|s| check_value_with_root(s, value, root).is_ok()).count();
        if match_count == 0 {
            return Err(ValidationError::root(
                ErrorKind::OneOfNoneMatch,
                "oneOf: no branch matches",
            ));
        }
        if match_count > 1 {
            return Err(ValidationError::root(
                ErrorKind::OneOfMultipleMatch,
                format!("oneOf: expected 1 match, got {match_count}"),
            ));
        }
    }

    // --- anyOf ---
    if let Some(Value::Array(subs)) = obj.get("anyOf") {
        let any_match = subs.iter().any(|s| check_value_with_root(s, value, root).is_ok());
        if !any_match {
            return Err(ValidationError::root(
                ErrorKind::AnyOfNoneMatch,
                "anyOf: no branch matches",
            ));
        }
    }

    // --- allOf ---
    if let Some(Value::Array(subs)) = obj.get("allOf") {
        for (i, sub) in subs.iter().enumerate() {
            check_value_with_root(sub, value, root).map_err(|e| {
                ValidationError::root(
                    e.kind,
                    format!("allOf[{i}]: {}", e.message),
                )
            })?;
        }
    }

    // --- if/then/else ---
    if let Some(if_schema) = obj.get("if") {
        if check_value_with_root(if_schema, value, root).is_ok() {
            if let Some(then_schema) = obj.get("then") {
                check_value_with_root(then_schema, value, root).map_err(|e| {
                    ValidationError::root(
                        ErrorKind::IfThenFailed,
                        format!("if/then: {}", e.message),
                    )
                })?;
            }
        } else if let Some(else_schema) = obj.get("else") {
            check_value_with_root(else_schema, value, root).map_err(|e| {
                ValidationError::root(
                    ErrorKind::IfThenFailed,
                    format!("if/else: {}", e.message),
                )
            })?;
        }
    }

    Ok(())
}

/// Validate a single field value against its sub-schema.
/// Returns descriptive error with field name context.
pub fn check_field(field: &str, schema: &Value, value: &Value, root: Option<&Value>) -> Result<(), ValidationError> {
    check_value_with_root(schema, value, root).map_err(|e| e.at_field(field))
}

// --- Type checking ---

fn check_type(type_spec: &Value, value: &Value) -> Result<(), ValidationError> {
    let types: Vec<&str> = match type_spec {
        Value::String(s) => vec![s.as_str()],
        Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect(),
        _ => return Ok(()),
    };

    // Draft-07: bool is NOT integer/number
    if value.is_boolean() && !types.contains(&"boolean") {
        if types.contains(&"integer") || types.contains(&"number") {
            return Err(ValidationError::root(
                ErrorKind::TypeError,
                format!("expected type {type_spec}, got boolean"),
            ));
        }
    }

    let matched = types.iter().any(|t| match *t {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64() || is_whole_number(value),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    });

    if !matched {
        return Err(ValidationError::root(
            ErrorKind::TypeError,
            format!("expected type {type_spec}, got {}", value_type_name(value)),
        ));
    }

    Ok(())
}

// --- Numeric ---

fn check_numeric(
    schema: &serde_json::Map<String, Value>,
    n: f64,
) -> Result<(), ValidationError> {
    if let Some(min) = schema.get("minimum").and_then(|v| v.as_f64()) {
        if n < min {
            return Err(ValidationError::root(
                ErrorKind::Minimum,
                format!("{n} < minimum {min}"),
            ));
        }
    }

    if let Some(max) = schema.get("maximum").and_then(|v| v.as_f64()) {
        if n > max {
            return Err(ValidationError::root(
                ErrorKind::Maximum,
                format!("{n} > maximum {max}"),
            ));
        }
    }

    if let Some(emin) = schema.get("exclusiveMinimum").and_then(|v| v.as_f64()) {
        if n <= emin {
            return Err(ValidationError::root(
                ErrorKind::ExclusiveMinimum,
                format!("{n} <= exclusiveMinimum {emin}"),
            ));
        }
    }

    if let Some(emax) = schema.get("exclusiveMaximum").and_then(|v| v.as_f64()) {
        if n >= emax {
            return Err(ValidationError::root(
                ErrorKind::ExclusiveMaximum,
                format!("{n} >= exclusiveMaximum {emax}"),
            ));
        }
    }

    if let Some(mo) = schema.get("multipleOf").and_then(|v| v.as_f64()) {
        if mo != 0.0 {
            let remainder = n % mo;
            if remainder.abs() > 1e-9 && (remainder - mo).abs() > 1e-9 {
                return Err(ValidationError::root(
                    ErrorKind::MultipleOf,
                    format!("{n} is not a multiple of {mo}"),
                ));
            }
        }
    }

    Ok(())
}

// --- String ---

fn check_string(
    schema: &serde_json::Map<String, Value>,
    s: &str,
) -> Result<(), ValidationError> {
    let len = s.graphemes(true).count();

    if let Some(min) = schema.get("minLength").and_then(as_usize) {
        if len < min {
            return Err(ValidationError::root(
                ErrorKind::MinLength,
                format!("length {len} < minLength {min}"),
            ));
        }
    }

    if let Some(max) = schema.get("maxLength").and_then(as_usize) {
        if len > max {
            return Err(ValidationError::root(
                ErrorKind::MaxLength,
                format!("length {len} > maxLength {max}"),
            ));
        }
    }

    if let Some(pat) = schema.get("pattern").and_then(|v| v.as_str()) {
        if let Ok(re) = cached_regex(pat) {
            if !re.is_match(s) {
                return Err(ValidationError::root(
                    ErrorKind::Pattern,
                    format!("{s:?} does not match pattern {pat:?}"),
                ));
            }
        }
    }

    Ok(())
}

// --- Array ---

fn check_array(
    schema: &serde_json::Map<String, Value>,
    arr: &[Value],
    root: Option<&Value>,
) -> Result<(), ValidationError> {
    if let Some(min) = schema.get("minItems").and_then(as_usize) {
        if arr.len() < min {
            return Err(ValidationError::root(
                ErrorKind::MinItems,
                format!("array length {} < minItems {min}", arr.len()),
            ));
        }
    }

    if let Some(max) = schema.get("maxItems").and_then(as_usize) {
        if arr.len() > max {
            return Err(ValidationError::root(
                ErrorKind::MaxItems,
                format!("array length {} > maxItems {max}", arr.len()),
            ));
        }
    }

    if schema.get("uniqueItems").and_then(|v| v.as_bool()) == Some(true) {
        let mut seen = std::collections::HashSet::new();
        for item in arr {
            let key = serde_json::to_string(item).unwrap_or_default();
            if !seen.insert(key) {
                return Err(ValidationError::root(
                    ErrorKind::UniqueItems,
                    format!("duplicate item {item} in array"),
                ));
            }
        }
    }

    // items — validate each element against item schema
    if let Some(items) = schema.get("items") {
        // Boolean schema: false rejects all items, true accepts all
        if let Some(b) = items.as_bool() {
            if !b && !arr.is_empty() {
                return Err(ValidationError::root(
                    ErrorKind::TypeError,
                    "items schema is false — no items allowed",
                ));
            }
        } else if let Some(item_schema) = items.as_object() {
            // Single schema for all items
            let item_schema_val = Value::Object(item_schema.clone());
            for (i, item) in arr.iter().enumerate() {
                check_value_with_root(&item_schema_val, item, root).map_err(|e| e.at_index(i))?;
            }
        } else if let Some(item_schemas) = items.as_array() {
            // Tuple validation: array of schemas
            for (i, item) in arr.iter().enumerate() {
                if let Some(item_schema) = item_schemas.get(i) {
                    check_value_with_root(item_schema, item, root).map_err(|e| e.at_index(i))?;
                }
            }
            // additionalItems for extra elements beyond tuple schemas
            if arr.len() > item_schemas.len() {
                if let Some(additional) = schema.get("additionalItems") {
                    if additional == &Value::Bool(false) {
                        return Err(ValidationError::root(
                            ErrorKind::MaxItems,
                            format!(
                                "array has {} items but tuple schema only allows {}",
                                arr.len(),
                                item_schemas.len()
                            ),
                        ));
                    }
                    if additional.is_object() {
                        for (i, item) in arr.iter().enumerate().skip(item_schemas.len()) {
                            check_value_with_root(additional, item, root).map_err(|e| e.at_index(i))?;
                        }
                    }
                }
            }
        }
    }

    // contains — at least one element must match
    if let Some(contains_schema) = schema.get("contains") {
        if let Some(b) = contains_schema.as_bool() {
            // contains: true — need at least one element; contains: false — always fails
            if !b {
                return Err(ValidationError::root(
                    ErrorKind::Contains,
                    "contains schema is false — no element can match",
                ));
            } else if arr.is_empty() {
                return Err(ValidationError::root(
                    ErrorKind::Contains,
                    "contains requires at least one element but array is empty",
                ));
            }
        } else {
            let any_match = arr.iter().any(|item| check_value_with_root(contains_schema, item, root).is_ok());
            if !any_match {
                return Err(ValidationError::root(
                    ErrorKind::Contains,
                    "no array element matches 'contains' schema",
                ));
            }
        }
    }

    Ok(())
}

// --- Object ---

fn check_object(
    schema: &serde_json::Map<String, Value>,
    map: &serde_json::Map<String, Value>,
    full_value: &Value,
    root: Option<&Value>,
) -> Result<(), ValidationError> {
    // required
    if let Some(Value::Array(req)) = schema.get("required") {
        for r in req {
            if let Some(field) = r.as_str() {
                if !map.contains_key(field) {
                    return Err(ValidationError::root(
                        ErrorKind::Required,
                        format!("missing required field '{field}'"),
                    ));
                }
            }
        }
    }

    // minProperties / maxProperties
    if let Some(min) = schema.get("minProperties").and_then(as_usize) {
        if map.len() < min {
            return Err(ValidationError::root(
                ErrorKind::MinProperties,
                format!("object has {} properties, minProperties is {min}", map.len()),
            ));
        }
    }
    if let Some(max) = schema.get("maxProperties").and_then(as_usize) {
        if map.len() > max {
            return Err(ValidationError::root(
                ErrorKind::MaxProperties,
                format!("object has {} properties, maxProperties is {max}", map.len()),
            ));
        }
    }

    // properties — validate each declared property
    let props = schema.get("properties").and_then(|v| v.as_object());
    if let Some(props) = props {
        for (pk, ps) in props {
            if let Some(pv) = map.get(pk) {
                check_value_with_root(ps, pv, root).map_err(|e| e.at_field(pk))?;
            }
        }
    }

    // patternProperties
    let pattern_props = schema.get("patternProperties").and_then(|v| v.as_object());
    if let Some(pp) = pattern_props {
        for (pat, pat_schema) in pp {
            if let Ok(re) = cached_regex(pat) {
                for (vk, vv) in map {
                    if re.is_match(vk) {
                        check_value_with_root(pat_schema, vv, root)
                            .map_err(|e| e.at_field(vk))?;
                    }
                }
            }
        }
    }

    // additionalProperties
    if let Some(ap) = schema.get("additionalProperties") {
        if ap != &Value::Bool(true) {
            let defined: std::collections::HashSet<&str> = props
                .map(|p| p.keys().map(|k| k.as_str()).collect())
                .unwrap_or_default();

            let pp_patterns: Vec<Regex> = pattern_props
                .map(|pp| {
                    pp.keys()
                        .filter_map(|k| cached_regex(k).ok())
                        .collect()
                })
                .unwrap_or_default();

            for (vk, vv) in map {
                if defined.contains(vk.as_str()) {
                    continue;
                }
                if pp_patterns.iter().any(|re| re.is_match(vk)) {
                    continue;
                }
                // extra property
                if ap == &Value::Bool(false) {
                    return Err(ValidationError::root(
                        ErrorKind::AdditionalProperties,
                        format!("additional property '{vk}' not allowed"),
                    ));
                }
                if ap.is_object() {
                    check_value_with_root(ap, vv, root)
                        .map_err(|e| e.at_field(vk))?;
                }
            }
        }
    }

    // dependencies / dependentRequired
    for dep_kw in ["dependencies", "dependentRequired"] {
        if let Some(deps_obj) = schema.get(dep_kw).and_then(|v| v.as_object()) {
            for (dk, deps) in deps_obj {
                if !map.contains_key(dk) {
                    continue;
                }
                match deps {
                    Value::Array(required) => {
                        for dep in required {
                            if let Some(dep_key) = dep.as_str() {
                                if !map.contains_key(dep_key) {
                                    return Err(ValidationError::root(
                                        ErrorKind::DependentRequired,
                                        format!("'{dk}' requires '{dep_key}' to be present"),
                                    ));
                                }
                            }
                        }
                    }
                    dep_schema if dep_schema.is_object() || dep_schema.is_boolean() => {
                        check_value_with_root(dep_schema, full_value, root).map_err(|e| {
                            ValidationError::root(
                                ErrorKind::DependencySchema,
                                format!("dependency '{dk}': {e}"),
                            )
                        })?;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

// --- Helpers ---

/// Extract a usize from a Value — handles both integer (2) and decimal (2.0) forms.
fn as_usize(v: &Value) -> Option<usize> {
    if let Some(n) = v.as_u64() {
        return Some(n as usize);
    }
    // Handle decimal form like 2.0
    v.as_f64()
        .filter(|f| f.fract() == 0.0 && *f >= 0.0)
        .map(|f| f as usize)
}

pub(crate) fn cached_regex(pattern: &str) -> Result<Regex, regex::Error> {
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static CACHE: RefCell<HashMap<String, Regex>> = RefCell::new(HashMap::new());
    }

    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(re) = cache.get(pattern) {
            return Ok(re.clone());
        }
        let re = Regex::new(pattern)?;
        cache.insert(pattern.to_string(), re.clone());
        Ok(re)
    })
}

/// Extract f64 from a Value, but NOT from booleans.
fn value_as_f64(v: &Value) -> Option<f64> {
    if v.is_boolean() {
        return None;
    }
    v.as_f64()
}

/// Check if a number Value is a whole number (e.g., 3.0 counts as integer).
fn is_whole_number(v: &Value) -> bool {
    v.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false)
}

/// Human-readable type name for error messages.
fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- Type ---

    #[test]
    fn test_type_string_pass() {
        let s = json!({"type": "string"});
        assert!(check_value(&s, &json!("hello")).is_ok());
    }

    #[test]
    fn test_type_string_fail() {
        let s = json!({"type": "string"});
        assert!(check_value(&s, &json!(42)).is_err());
    }

    #[test]
    fn test_type_integer_pass() {
        let s = json!({"type": "integer"});
        assert!(check_value(&s, &json!(42)).is_ok());
    }

    #[test]
    fn test_type_integer_fail_float() {
        let s = json!({"type": "integer"});
        assert!(check_value(&s, &json!(3.5)).is_err());
    }

    #[test]
    fn test_type_bool_not_integer() {
        let s = json!({"type": "integer"});
        assert!(check_value(&s, &json!(true)).is_err());
    }

    #[test]
    fn test_type_bool_not_number() {
        let s = json!({"type": "number"});
        assert!(check_value(&s, &json!(true)).is_err());
    }

    #[test]
    fn test_type_union() {
        let s = json!({"type": ["string", "null"]});
        assert!(check_value(&s, &json!("hi")).is_ok());
        assert!(check_value(&s, &Value::Null).is_ok());
        assert!(check_value(&s, &json!(42)).is_err());
    }

    // --- Const / Enum ---

    #[test]
    fn test_const_pass() {
        let s = json!({"const": 42});
        assert!(check_value(&s, &json!(42)).is_ok());
    }

    #[test]
    fn test_const_fail() {
        let s = json!({"const": 42});
        assert!(check_value(&s, &json!(43)).is_err());
    }

    #[test]
    fn test_enum_pass() {
        let s = json!({"enum": ["a", "b", "c"]});
        assert!(check_value(&s, &json!("b")).is_ok());
    }

    #[test]
    fn test_enum_fail() {
        let s = json!({"enum": ["a", "b"]});
        assert!(check_value(&s, &json!("z")).is_err());
    }

    // --- Numeric ---

    #[test]
    fn test_minimum() {
        let s = json!({"type": "integer", "minimum": 0});
        assert!(check_value(&s, &json!(0)).is_ok());
        assert!(check_value(&s, &json!(-1)).is_err());
    }

    #[test]
    fn test_maximum() {
        let s = json!({"type": "integer", "maximum": 100});
        assert!(check_value(&s, &json!(100)).is_ok());
        assert!(check_value(&s, &json!(101)).is_err());
    }

    #[test]
    fn test_exclusive_minimum() {
        let s = json!({"exclusiveMinimum": 0});
        assert!(check_value(&s, &json!(1)).is_ok());
        assert!(check_value(&s, &json!(0)).is_err());
    }

    #[test]
    fn test_exclusive_maximum() {
        let s = json!({"exclusiveMaximum": 10});
        assert!(check_value(&s, &json!(9)).is_ok());
        assert!(check_value(&s, &json!(10)).is_err());
    }

    #[test]
    fn test_multiple_of() {
        let s = json!({"multipleOf": 5});
        assert!(check_value(&s, &json!(15)).is_ok());
        assert!(check_value(&s, &json!(7)).is_err());
    }

    #[test]
    fn test_multiple_of_float() {
        let s = json!({"multipleOf": 0.1});
        assert!(check_value(&s, &json!(0.3)).is_ok());
    }

    // --- String ---

    #[test]
    fn test_min_length() {
        let s = json!({"type": "string", "minLength": 3});
        assert!(check_value(&s, &json!("abc")).is_ok());
        assert!(check_value(&s, &json!("ab")).is_err());
    }

    #[test]
    fn test_max_length() {
        let s = json!({"type": "string", "maxLength": 5});
        assert!(check_value(&s, &json!("hello")).is_ok());
        assert!(check_value(&s, &json!("toolong")).is_err());
    }

    #[test]
    fn test_pattern() {
        let s = json!({"type": "string", "pattern": "^[A-Z]"});
        assert!(check_value(&s, &json!("Hello")).is_ok());
        assert!(check_value(&s, &json!("hello")).is_err());
    }

    // --- Array ---

    #[test]
    fn test_min_items() {
        let s = json!({"type": "array", "minItems": 2});
        assert!(check_value(&s, &json!([1, 2])).is_ok());
        assert!(check_value(&s, &json!([1])).is_err());
    }

    #[test]
    fn test_max_items() {
        let s = json!({"type": "array", "maxItems": 2});
        assert!(check_value(&s, &json!([1, 2])).is_ok());
        assert!(check_value(&s, &json!([1, 2, 3])).is_err());
    }

    #[test]
    fn test_unique_items() {
        let s = json!({"type": "array", "uniqueItems": true});
        assert!(check_value(&s, &json!([1, 2, 3])).is_ok());
        assert!(check_value(&s, &json!([1, 2, 1])).is_err());
    }

    // --- Object ---

    #[test]
    fn test_required() {
        let s = json!({"type": "object", "required": ["name"]});
        assert!(check_value(&s, &json!({"name": "Alice"})).is_ok());
        assert!(check_value(&s, &json!({"age": 30})).is_err());
    }

    #[test]
    fn test_min_properties() {
        let s = json!({"minProperties": 2});
        assert!(check_value(&s, &json!({"a": 1, "b": 2})).is_ok());
        assert!(check_value(&s, &json!({"a": 1})).is_err());
    }

    #[test]
    fn test_max_properties() {
        let s = json!({"maxProperties": 1});
        assert!(check_value(&s, &json!({"a": 1})).is_ok());
        assert!(check_value(&s, &json!({"a": 1, "b": 2})).is_err());
    }

    #[test]
    fn test_properties_validate_children() {
        let s = json!({
            "type": "object",
            "properties": {
                "age": {"type": "integer", "minimum": 0}
            }
        });
        assert!(check_value(&s, &json!({"age": 25})).is_ok());
        assert!(check_value(&s, &json!({"age": -1})).is_err());
    }

    #[test]
    fn test_additional_properties_false() {
        let s = json!({
            "properties": {"name": {"type": "string"}},
            "additionalProperties": false
        });
        assert!(check_value(&s, &json!({"name": "Alice"})).is_ok());
        assert!(check_value(&s, &json!({"name": "Alice", "extra": 1})).is_err());
    }

    #[test]
    fn test_additional_properties_schema() {
        let s = json!({
            "properties": {"name": {"type": "string"}},
            "additionalProperties": {"type": "integer"}
        });
        assert!(check_value(&s, &json!({"name": "Alice", "x": 1})).is_ok());
        assert!(check_value(&s, &json!({"name": "Alice", "x": "bad"})).is_err());
    }

    #[test]
    fn test_pattern_properties() {
        let s = json!({
            "patternProperties": {
                "^S_": {"type": "string"},
                "^I_": {"type": "integer"}
            }
        });
        assert!(check_value(&s, &json!({"S_name": "Alice", "I_count": 5})).is_ok());
        assert!(check_value(&s, &json!({"S_name": 123})).is_err());
    }

    #[test]
    fn test_dependent_required() {
        let s = json!({
            "dependencies": {
                "credit_card": ["billing_address"]
            }
        });
        assert!(check_value(&s, &json!({"credit_card": "123", "billing_address": "x"})).is_ok());
        assert!(check_value(&s, &json!({"credit_card": "123"})).is_err());
        assert!(check_value(&s, &json!({"name": "Alice"})).is_ok()); // no trigger
    }

    #[test]
    fn test_dependency_schema() {
        let s = json!({
            "dependencies": {
                "bar": {
                    "properties": {
                        "foo": {"type": "integer"}
                    }
                }
            }
        });
        assert!(check_value(&s, &json!({"bar": 1, "foo": 42})).is_ok());
        assert!(check_value(&s, &json!({"bar": 1, "foo": "bad"})).is_err());
    }

    // --- Error context ---

    #[test]
    fn test_nested_error_has_path() {
        let s = json!({
            "properties": {
                "user": {
                    "type": "object",
                    "properties": {
                        "age": {"type": "integer"}
                    }
                }
            }
        });
        let err = check_value(&s, &json!({"user": {"age": "not_int"}})).unwrap_err();
        assert!(err.path.contains("user"), "path should contain 'user': {}", err.path);
        assert!(err.path.contains("age"), "path should contain 'age': {}", err.path);
    }

    // --- Non-object schema ---

    #[test]
    fn test_non_object_schema_passes() {
        assert!(check_value(&json!(true), &json!("anything")).is_ok());
        assert!(check_value(&Value::Null, &json!(42)).is_ok());
    }
}

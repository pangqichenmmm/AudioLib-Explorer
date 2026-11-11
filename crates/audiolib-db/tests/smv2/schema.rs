// crates/audiolib-db/tests/smv2/schema.rs
//! Minimal JSON Schema subset validator for `doc/schemas/migration-report.schema.json`
//! (type, enum, required, properties, additionalProperties, items, minimum), so the
//! harness needs no new dependency.
use serde_json::Value;
use std::sync::OnceLock;

pub fn report_schema() -> &'static Value {
    static S: OnceLock<Value> = OnceLock::new();
    S.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../../../../doc/schemas/migration-report.schema.json"
        ))
        .expect("migration-report.schema.json is valid JSON")
    })
}

pub fn http_schema() -> &'static Value {
    static S: OnceLock<Value> = OnceLock::new();
    S.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../../../../doc/schemas/migration-http.schema.json"
        ))
        .expect("migration-http.schema.json is valid JSON")
    })
}

/// Errors as `path: message`; empty when `value` conforms.
pub fn validate(schema: &Value, value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    check(schema, value, "$", &mut errors);
    errors
}

/// Search actions emit only `ids` / `next_cursor`: check those properties alone.
pub fn validate_search(value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let props = &report_schema()["properties"];
    match value.get("ids") {
        Some(ids) => check(&props["ids"], ids, "$.ids", &mut errors),
        None => errors.push("$: missing ids".into()),
    }
    if let Some(c) = value.get("next_cursor") {
        check(&props["next_cursor"], c, "$.next_cursor", &mut errors);
    }
    errors
}

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => false,
    }
}

fn check(schema: &Value, v: &Value, path: &str, errors: &mut Vec<String>) {
    if let Some(t) = schema.get("type") {
        let ok = match t {
            Value::String(t) => type_ok(t, v),
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).any(|t| type_ok(t, v)),
            _ => true,
        };
        if !ok {
            errors.push(format!("{path}: expected type {t}, got {v}"));
            return;
        }
    }
    if let Some(Value::Array(options)) = schema.get("enum")
        && !options.contains(v)
    {
        errors.push(format!("{path}: {v} not in enum {options:?}"));
    }
    if let (Some(min), Some(n)) = (schema.get("minimum").and_then(Value::as_f64), v.as_f64())
        && n < min
    {
        errors.push(format!("{path}: {n} < minimum {min}"));
    }
    if let Some(obj) = v.as_object() {
        if let Some(Value::Array(req)) = schema.get("required") {
            for k in req.iter().filter_map(Value::as_str) {
                if !obj.contains_key(k) {
                    errors.push(format!("{path}: missing required {k}"));
                }
            }
        }
        let props = schema.get("properties").and_then(Value::as_object);
        for (k, child) in obj {
            let sub = format!("{path}.{k}");
            match props.and_then(|p| p.get(k)) {
                Some(s) => check(s, child, &sub, errors),
                None => match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => errors.push(format!("{sub}: not allowed")),
                    Some(s @ Value::Object(_)) => check(s, child, &sub, errors),
                    _ => {}
                },
            }
        }
    }
    if let (Some(items), Some(arr)) = (schema.get("items"), v.as_array()) {
        for (i, child) in arr.iter().enumerate() {
            check(items, child, &format!("{path}[{i}]"), errors);
        }
    }
}

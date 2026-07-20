use serde::Deserialize;
use serde_json::Value;

pub fn validate_args(schema: &Value, args: &Value) -> Result<(), String> {
    let validator =
        jsonschema::validator_for(schema).map_err(|e| format!("invalid schema: {e}"))?;
    validator.validate(args).map_err(|e| e.to_string())
}

/// Normalize weak-model tool arguments before coerce/validate.
///
/// Handles common failure modes:
/// - top-level JSON string instead of object
/// - trailing junk after a valid object (`{...} sure!`)
/// - concatenated objects (`{...}{...}`) — keeps the first
/// - markdown fences around JSON
pub fn repair_args(args: &Value) -> Value {
    match args {
        Value::Object(_) => args.clone(),
        Value::String(s) => parse_args_string(s).unwrap_or_else(|| args.clone()),
        Value::Null => Value::Object(serde_json::Map::new()),
        other => other.clone(),
    }
}

fn parse_args_string(raw: &str) -> Option<Value> {
    let s = strip_md_fence(raw.trim());
    if s.is_empty() {
        return Some(Value::Object(serde_json::Map::new()));
    }
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return match v {
            Value::Object(_) => Some(v),
            // Some models wrap args in a one-element array.
            Value::Array(mut a) if a.len() == 1 && a[0].is_object() => Some(a.remove(0)),
            _ => Some(v),
        };
    }
    // Trailing junk or concatenated objects: take the first complete value.
    extract_first_json_value(s)
}

fn strip_md_fence(s: &str) -> &str {
    let t = s.trim();
    if !t.starts_with("```") {
        return t;
    }
    // Best effort: find first `{` / `[` after the fence line.
    t.find(['{', '[']).map(|i| &t[i..]).unwrap_or(t)
}

/// Scan `s` for the first JSON value (object or array) using `serde_json`'s
/// streaming deserializer so trailing text and concatenated values are ok.
fn extract_first_json_value(s: &str) -> Option<Value> {
    let start = s.find(['{', '['])?;
    let slice = &s[start..];
    let mut de = serde_json::Deserializer::from_str(slice);
    match Value::deserialize(&mut de) {
        Ok(v) => Some(v),
        Err(_) => extract_balanced_object(slice),
    }
}

fn extract_balanced_object(s: &str) -> Option<Value> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let chunk = &s[start..=i];
                    return serde_json::from_str(chunk).ok();
                }
            }
            _ => {}
        }
    }
    None
}

pub fn coerce_args(schema: &Value, args: &Value) -> Value {
    let args = repair_args(args);
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return args;
    };
    let Some(obj) = args.as_object() else {
        return args;
    };
    let mut out = obj.clone();
    for (key, value) in obj.iter() {
        let Some(prop_schema) = props.get(key) else {
            continue;
        };
        let expected = match prop_schema.get("type") {
            Some(Value::String(t)) => Some(t.clone()),
            Some(Value::Array(types)) => types
                .iter()
                .filter_map(|t| t.as_str())
                .find(|t| *t != "null")
                .map(|t| t.to_string()),
            _ => None,
        };
        let Some(expected) = expected else {
            continue;
        };
        let coerced = match (expected.as_str(), value) {
            ("integer", Value::String(s)) => s
                .trim()
                .parse::<i64>()
                .map(Value::from)
                .unwrap_or_else(|_| value.clone()),
            ("number", Value::String(s)) => s
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .unwrap_or_else(|| value.clone()),
            ("boolean", Value::String(s)) => match s.trim() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => value.clone(),
            },
            ("object" | "array", Value::String(s)) => {
                serde_json::from_str(s).unwrap_or_else(|_| value.clone())
            }
            _ => value.clone(),
        };
        out.insert(key.clone(), coerced);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coerce_string_to_integer() {
        let schema = json!({"type":"object","properties":{"timeout":{"type":"integer"}}});
        let args = json!({"timeout": "30"});
        assert_eq!(coerce_args(&schema, &args), json!({"timeout": 30}));
    }

    #[test]
    fn coerce_nullable_type_arrays() {
        let schema = json!({"type":"object","properties":{
            "limit":{"type":["integer","null"]},
            "path":{"type":"string"}
        }});
        let args = json!({"limit": "60", "path": "f.rs"});
        assert_eq!(
            coerce_args(&schema, &args),
            json!({"limit": 60, "path": "f.rs"})
        );
    }

    #[test]
    fn coerce_stringified_object() {
        let schema = json!({"type":"object","properties":{"edits":{"type":"array"}}});
        let args = json!({"edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]"});
        let out = coerce_args(&schema, &args);
        assert!(out["edits"].is_array());
    }

    #[test]
    fn validate_ok() {
        let schema = json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]});
        assert!(validate_args(&schema, &json!({"command":"ls"})).is_ok());
    }

    #[test]
    fn validate_missing_required() {
        let schema = json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]});
        assert!(validate_args(&schema, &json!({})).is_err());
    }

    #[test]
    fn validate_wrong_type() {
        let schema = json!({"type":"object","properties":{"n":{"type":"integer"}}});
        assert!(validate_args(&schema, &json!({"n":"abc"})).is_err());
    }

    #[test]
    fn repair_top_level_string_object() {
        let raw = json!("{\"command\": \"ls\"}");
        assert_eq!(repair_args(&raw), json!({"command": "ls"}));
    }

    #[test]
    fn repair_trailing_junk() {
        let raw = json!("{\"path\": \"a.rs\"} thanks!");
        assert_eq!(repair_args(&raw), json!({"path": "a.rs"}));
    }

    #[test]
    fn repair_concatenated_objects_keeps_first() {
        let raw = Value::String(r#"{"path":"a"}{"path":"b"}"#.into());
        assert_eq!(repair_args(&raw), json!({"path": "a"}));
    }

    #[test]
    fn repair_null_becomes_empty_object() {
        assert_eq!(repair_args(&Value::Null), json!({}));
    }

    #[test]
    fn coerce_repairs_then_coerces() {
        let schema = json!({"type":"object","properties":{"timeout":{"type":"integer"}}});
        let args = json!("{\"timeout\": \"30\"}");
        assert_eq!(coerce_args(&schema, &args), json!({"timeout": 30}));
    }
}

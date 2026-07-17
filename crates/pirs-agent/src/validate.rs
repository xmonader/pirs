use serde_json::Value;

pub fn validate_args(schema: &Value, args: &Value) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema).map_err(|e| format!("invalid schema: {e}"))?;
    validator
        .validate(args)
        .map_err(|e| e.to_string())
}

pub fn coerce_args(schema: &Value, args: &Value) -> Value {
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return args.clone();
    };
    let Some(obj) = args.as_object() else {
        return args.clone();
    };
    let mut out = obj.clone();
    for (key, value) in obj.iter() {
        let Some(prop_schema) = props.get(key) else { continue };
        let Some(expected) = prop_schema.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        let coerced = match (expected, value) {
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
}

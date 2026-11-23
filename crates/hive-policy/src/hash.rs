use serde_json::Value;
use sha2::{Digest, Sha256};

pub struct HashRequest<'a> {
    pub tool_name: &'a str,
    pub tool_version: u32,
    pub arguments: &'a Value,
    pub mission_id: &'a str,
    pub run_id: Option<&'a str>,
    pub node_id: &'a str,
    pub session_id: Option<&'a str>,
    pub attempt_no: u32,
    pub lease_token: &'a str,
    pub policy_version: &'a str,
}

pub fn request_hash(request: &HashRequest<'_>) -> String {
    let mut canonical = String::new();
    canonical.push('{');
    push_field(
        &mut canonical,
        "arguments",
        &canonical_json(request.arguments),
        true,
    );
    push_field(
        &mut canonical,
        "attempt_no",
        &request.attempt_no.to_string(),
        false,
    );
    push_field(
        &mut canonical,
        "lease_token",
        &string_json(request.lease_token),
        false,
    );
    push_field(
        &mut canonical,
        "mission_id",
        &string_json(request.mission_id),
        false,
    );
    push_field(
        &mut canonical,
        "node_id",
        &string_json(request.node_id),
        false,
    );
    push_field(
        &mut canonical,
        "policy_version",
        &string_json(request.policy_version),
        false,
    );
    push_optional_field(&mut canonical, "run_id", request.run_id);
    push_optional_field(&mut canonical, "session_id", request.session_id);
    push_field(
        &mut canonical,
        "tool_name",
        &string_json(request.tool_name),
        false,
    );
    push_field(
        &mut canonical,
        "tool_version",
        &request.tool_version.to_string(),
        false,
    );
    canonical.push('}');

    let digest = Sha256::digest(canonical.as_bytes());
    hex::encode(digest)
}

pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn push_field(out: &mut String, key: &str, value: &str, first: bool) {
    if !first {
        out.push(',');
    }
    out.push_str(&string_json(key));
    out.push(':');
    out.push_str(value);
}

fn push_optional_field(out: &mut String, key: &str, value: Option<&str>) {
    out.push(',');
    out.push_str(&string_json(key));
    out.push(':');
    match value {
        Some(value) => out.push_str(&string_json(value)),
        None => out.push_str("null"),
    }
}

fn string_json(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization cannot fail")
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => out.push_str(&value.to_string()),
        Value::String(value) => out.push_str(&string_json(value)),
        Value::Array(values) => {
            out.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(value, out);
            }
            out.push(']');
        }
        Value::Object(values) => {
            out.push('{');
            let mut keys: Vec<&String> = values.keys().collect();
            keys.sort();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&string_json(key));
                out.push(':');
                write_canonical(&values[key], out);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Map, Value};

    use super::*;

    fn hash(arguments: &Value) -> String {
        request_hash(&HashRequest {
            tool_name: "fs.read",
            tool_version: 1,
            arguments,
            mission_id: "m_00000000-0000-4000-8000-000000000001",
            run_id: None,
            node_id: "n_00000000-0000-4000-8000-000000000002",
            session_id: None,
            attempt_no: 1,
            lease_token: "lease",
            policy_version: "v1",
        })
    }

    #[test]
    fn request_hash_is_stable_and_key_order_independent() {
        let first = json!({ "b": 2, "a": 1 });
        let mut reversed = Map::new();
        reversed.insert("b".to_owned(), json!(2));
        reversed.insert("a".to_owned(), json!(1));
        let second = Value::Object(reversed);

        assert_eq!(hash(&first), hash(&second));
        assert_eq!(hash(&first).len(), 64);
        assert_ne!(hash(&first), hash(&json!({ "a": 1, "b": 3 })));
    }

    #[test]
    fn canonical_json_sorts_object_keys() {
        let mut map = Map::new();
        map.insert("z".to_owned(), json!(1));
        map.insert("a".to_owned(), json!(2));
        assert_eq!(canonical_json(&Value::Object(map)), r#"{"a":2,"z":1}"#);
    }
}

//! Canonical JSON and hashing. Object keys are sorted regardless of how the value was built,
//! so hashes do not depend on map insertion order or serde_json features.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&Value::String(s.clone()).to_string()),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write(&map[key], out);
            }
            out.push('}');
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Converts a domain value to JSON. Soil's domain types only use string-keyed maps, so this
/// conversion cannot fail for them; a failure indicates a programming error.
pub fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("soil domain types always serialize to JSON")
}

/// SHA-256 of the canonical JSON form of `value`.
pub fn hash_of<T: Serialize>(value: &T) -> String {
    sha256_hex(canonical_json(&to_value(value)).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_order_does_not_change_hash() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"y":2,"x":[1,"s"]}}"#).unwrap();
        let b = json!({"a": {"x": [1, "s"], "y": 2}, "b": 1});
        assert_eq!(canonical_json(&a), r#"{"a":{"x":[1,"s"],"y":2},"b":1}"#);
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    #[test]
    fn escapes_strings() {
        assert_eq!(canonical_json(&json!("a\"b")), r#""a\"b""#);
    }
}

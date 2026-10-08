//! `${path}` placeholders in params, resources and intent text.
//!
//! A string that is exactly one placeholder is replaced by the referenced value (keeping its
//! JSON type); placeholders embedded in longer strings are interpolated as text. Objects with a
//! `secretRef` key are references to secrets and are never touched; the core never resolves
//! secrets.

use crate::canonical::canonical_json;
use crate::expr::{parse_path, Path};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("unresolved placeholder ${{{0}}}")]
    Unresolved(String),
    #[error("invalid placeholder ${{{0}}}: {1}")]
    Invalid(String, String),
    #[error("unterminated placeholder in '{0}'")]
    Unterminated(String),
}

/// Splits a string into literal text and placeholder bodies.
fn pieces(s: &str) -> Result<Vec<(bool, &str)>, TemplateError> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        if start > 0 {
            out.push((false, &rest[..start]));
        }
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| TemplateError::Unterminated(s.to_string()))?;
        out.push((true, after[..end].trim()));
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        out.push((false, rest));
    }
    Ok(out)
}

fn path(body: &str) -> Result<Path, TemplateError> {
    parse_path(body).map_err(|e| TemplateError::Invalid(body.to_string(), e.message))
}

pub fn render_str(s: &str, ctx: &Value) -> Result<String, TemplateError> {
    match render_string(s, ctx)? {
        Value::String(s) => Ok(s),
        other => Ok(canonical_json(&other)),
    }
}

fn render_string(s: &str, ctx: &Value) -> Result<Value, TemplateError> {
    let parts = pieces(s)?;
    if let [(true, body)] = parts.as_slice() {
        return path(body)?
            .lookup(ctx)
            .cloned()
            .ok_or_else(|| TemplateError::Unresolved(body.to_string()));
    }
    let mut out = String::new();
    for (is_placeholder, text) in parts {
        if !is_placeholder {
            out.push_str(text);
            continue;
        }
        match path(text)?.lookup(ctx) {
            Some(Value::String(v)) => out.push_str(v),
            Some(v) => out.push_str(&canonical_json(v)),
            None => return Err(TemplateError::Unresolved(text.to_string())),
        }
    }
    Ok(Value::String(out))
}

pub fn render(value: &Value, ctx: &Value) -> Result<Value, TemplateError> {
    match value {
        Value::String(s) => render_string(s, ctx),
        Value::Array(items) => items
            .iter()
            .map(|v| render(v, ctx))
            .collect::<Result<_, _>>()
            .map(Value::Array),
        Value::Object(map) if map.contains_key("secretRef") => Ok(value.clone()),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| Ok((k.clone(), render(v, ctx)?)))
            .collect::<Result<_, _>>()
            .map(Value::Object),
        other => Ok(other.clone()),
    }
}

/// Placeholder bodies found in a string, for static validation.
pub fn placeholders_in_str(s: &str) -> Result<Vec<Path>, TemplateError> {
    pieces(s)?
        .into_iter()
        .filter(|(is_placeholder, _)| *is_placeholder)
        .map(|(_, body)| path(body))
        .collect()
}

/// Placeholder bodies found anywhere in a JSON value (secret references excluded).
pub fn placeholders(value: &Value) -> Result<Vec<Path>, TemplateError> {
    let mut out = Vec::new();
    collect(value, &mut out)?;
    Ok(out)
}

fn collect(value: &Value, out: &mut Vec<Path>) -> Result<(), TemplateError> {
    match value {
        Value::String(s) => out.extend(placeholders_in_str(s)?),
        Value::Array(items) => {
            for v in items {
                collect(v, out)?;
            }
        }
        Value::Object(map) if map.contains_key("secretRef") => {}
        Value::Object(map) => {
            for v in map.values() {
                collect(v, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn whole_placeholder_keeps_type() {
        let ctx = json!({"inputs": {"weight": 100, "image": "app:2"}});
        assert_eq!(render(&json!("${inputs.weight}"), &ctx).unwrap(), json!(100));
        assert_eq!(
            render(&json!({"w": "${ inputs.weight }", "l": ["x-${inputs.image}"]}), &ctx).unwrap(),
            json!({"w": 100, "l": ["x-app:2"]})
        );
    }

    #[test]
    fn missing_is_an_error() {
        let err = render(&json!("${facts.nope}"), &json!({})).unwrap_err();
        assert_eq!(err, TemplateError::Unresolved("facts.nope".into()));
        assert!(matches!(
            render_str("a ${b", &json!({})),
            Err(TemplateError::Unterminated(_))
        ));
    }

    #[test]
    fn secret_refs_are_untouched() {
        let v = json!({"token": {"secretRef": "env:${X}"}});
        assert_eq!(render(&v, &json!({})).unwrap(), v);
        assert!(placeholders(&v).unwrap().is_empty());
    }
}

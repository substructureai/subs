use std::fmt;

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer};

const WINDOW: usize = 160;

/// A float that reads inside a tagged enum. With `arbitrary_precision`, serde
/// buffers a number there as a map that `f64` refuses.
pub fn option_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| D::Error::custom(format!("invalid type: {v}, expected a number"))),
    }
}

pub fn f64<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    option_f64(d)?.ok_or_else(|| D::Error::custom("invalid type: null, expected a number"))
}

pub fn f64_map<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<std::collections::BTreeMap<String, f64>, D::Error> {
    std::collections::BTreeMap::<String, serde_json::Value>::deserialize(d)?
        .into_iter()
        .map(|(k, v)| match v.as_f64() {
            Some(n) => Ok((k, n)),
            None => Err(D::Error::custom(format!(
                "invalid type: {v}, expected a number"
            ))),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonParseError {
    pub what: &'static str,
    pub path: String,
    pub message: String,
    pub excerpt: String,
}

impl JsonParseError {
    pub fn param(&self) -> Option<&str> {
        (!self.path.is_empty()).then_some(self.path.as_str())
    }
}

impl fmt::Display for JsonParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}", self.what)?;
        if self.path.is_empty() {
            return write!(f, ": {}", self.message);
        }
        write!(
            f,
            " at `{}`: {}",
            self.path,
            without_position(&self.message)
        )
    }
}

fn without_position(message: &str) -> &str {
    match message.rfind(" at line ") {
        Some(cut) => &message[..cut],
        None => message,
    }
}

impl std::error::Error for JsonParseError {}

impl From<JsonParseError> for crate::protocol::ErrorInfo {
    fn from(e: JsonParseError) -> Self {
        let info = crate::protocol::ErrorInfo::new(
            crate::protocol::ErrorCode::InvalidResponse,
            e.to_string(),
        );
        match e.param() {
            Some(param) => info.with_param(param),
            None => info,
        }
    }
}

pub fn from_slice<T: DeserializeOwned>(
    what: &'static str,
    bytes: &[u8],
) -> Result<T, JsonParseError> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let value = serde_path_to_error::deserialize(&mut de).map_err(|e| {
        let path = e.path().to_string();
        let inner = e.into_inner();
        JsonParseError {
            what,
            path: if path == "." { String::new() } else { path },
            message: inner.to_string(),
            excerpt: excerpt_at(bytes, inner.line(), inner.column()),
        }
    })?;
    de.end().map_err(|e| JsonParseError {
        what,
        path: String::new(),
        message: e.to_string(),
        excerpt: excerpt_at(bytes, e.line(), e.column()),
    })?;
    Ok(value)
}

pub fn from_str<T: DeserializeOwned>(what: &'static str, text: &str) -> Result<T, JsonParseError> {
    from_slice(what, text.as_bytes())
}

pub fn from_value<T: DeserializeOwned>(
    what: &'static str,
    value: serde_json::Value,
) -> Result<T, JsonParseError> {
    serde_path_to_error::deserialize(&value).map_err(|e| {
        let path = e.path().to_string();
        JsonParseError {
            what,
            path: if path == "." { String::new() } else { path },
            message: e.into_inner().to_string(),
            excerpt: excerpt(value.to_string().as_bytes()),
        }
    })
}

pub fn excerpt(bytes: &[u8]) -> String {
    excerpt_at(bytes, 0, 0)
}

const MAX_MESSAGE: usize = 300;

pub fn error_message(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let field = |v: &serde_json::Value, key: &str| -> Option<String> {
        match v.get(key)? {
            serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            nested => nested
                .get("message")
                .or_else(|| nested.get("error"))
                .and_then(|m| m.as_str())
                .map(str::to_string),
        }
    };
    field(&value, "error")
        .or_else(|| field(&value, "message"))
        .or_else(|| field(&value, "detail"))
        .map(|m| one_line(&m, MAX_MESSAGE))
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

fn excerpt_at(bytes: &[u8], line: usize, column: usize) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let at = offset_of(bytes, line, column);
    let start = at.saturating_sub(WINDOW / 2);
    let end = (at + WINDOW / 2).min(bytes.len());

    let mut text = String::from_utf8_lossy(&bytes[start..end])
        .trim_matches(char::REPLACEMENT_CHARACTER)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if start > 0 {
        text.insert(0, '…');
    }
    if end < bytes.len() {
        text.push('…');
    }
    text
}

fn offset_of(bytes: &[u8], line: usize, column: usize) -> usize {
    let mut offset = 0;
    for _ in 1..line.max(1) {
        match bytes[offset..].iter().position(|b| *b == b'\n') {
            Some(i) => offset += i + 1,
            None => break,
        }
    }
    (offset + column.saturating_sub(1)).min(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Outer {
        #[allow(dead_code)]
        items: Vec<Inner>,
    }

    #[derive(Debug, Deserialize)]
    struct Inner {
        #[allow(dead_code)]
        name: String,
    }

    #[test]
    fn the_error_names_the_path_and_quotes_the_value() {
        let body = br#"{"items":[{"name":"ok"},{"name":42}]}"#;
        let err = from_slice::<Outer>("decision response", body).expect_err("42 is not a string");
        assert_eq!(err.path, "items[1].name");
        assert!(err.message.contains("invalid type"), "{}", err.message);
        assert!(err.excerpt.contains("42"), "{}", err.excerpt);
        let rendered = err.to_string();
        assert!(rendered.contains("invalid decision response at `items[1].name`"));
    }

    #[test]
    fn a_root_failure_has_no_path() {
        let err = from_slice::<Outer>("decision response", b"[]").expect_err("not an object");
        assert!(err.path.is_empty(), "{}", err.path);
    }

    #[test]
    fn trailing_content_is_a_failure_not_a_silent_prefix() {
        let body = br#"{"items":[]} <html>oops</html>"#;
        let err = from_slice::<Outer>("decision response", body).expect_err("trailing junk");
        assert!(err.excerpt.contains("html"), "{}", err.excerpt);
    }

    #[test]
    fn an_excerpt_is_bounded_and_single_line() {
        let long = format!(r#"{{"items":[{}]}}"#, "{\"name\":\n1},".repeat(200));
        let err =
            from_slice::<Outer>("decision response", long.as_bytes()).expect_err("not a string");
        assert!(err.excerpt.chars().count() <= WINDOW + 2, "{}", err.excerpt);
        assert!(!err.excerpt.contains('\n'), "{}", err.excerpt);
    }

    #[test]
    fn a_worker_error_body_gives_up_its_message() {
        assert_eq!(
            error_message(br#"{"error":"ANTHROPIC_API_KEY is not set"}"#).as_deref(),
            Some("ANTHROPIC_API_KEY is not set")
        );
        assert_eq!(
            error_message(br#"{"error":{"message":"model not found","type":"invalid_request"}}"#)
                .as_deref(),
            Some("model not found")
        );
        assert_eq!(
            error_message(br#"{"message":"Internal Server Error"}"#).as_deref(),
            Some("Internal Server Error")
        );
        assert_eq!(error_message(b"<html>500</html>"), None);
        assert_eq!(error_message(br#"{"actions":[]}"#), None);
    }
}

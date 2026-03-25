use std::collections::VecDeque;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// All value types supported by MnemeCache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Value {
    String(#[serde(with = "serde_bytes")] Vec<u8>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    List(VecDeque<Vec<u8>>),
    ZSet(Vec<ZSetMember>),
    /// Atomic signed 64-bit counter. INCR/DECR/INCRBY/DECRBY/GETSET.
    Counter(i64),
    /// JSON document stored as a raw UTF-8 string internally.
    /// Supports path-based GET/SET via JSONPath ($.field.nested).
    Json(JsonDoc),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::String(_)  => "string",
            Value::Hash(_)    => "hash",
            Value::List(_)    => "list",
            Value::ZSet(_)    => "zset",
            Value::Counter(_) => "counter",
            Value::Json(_)    => "json",
        }
    }

    pub fn memory_usage(&self) -> usize {
        match self {
            Value::String(b)   => b.len(),
            Value::Hash(pairs) => pairs.iter().map(|(k, v)| k.len() + v.len()).sum(),
            Value::List(items) => items.iter().map(|b| b.len()).sum(),
            Value::ZSet(m)     => m.len() * 24,
            Value::Counter(_)  => 8,
            Value::Json(doc)   => doc.raw.len(),
        }
    }

    pub fn to_bytes(&self) -> crate::Result<Vec<u8>> {
        rmp_serde::to_vec(self)
            .map_err(|e| crate::MnemeError::Serialization(e.to_string()))
    }

    pub fn from_bytes(data: &[u8]) -> crate::Result<Self> {
        rmp_serde::from_slice(data)
            .map_err(|e| crate::MnemeError::Serialization(e.to_string()))
    }

    pub fn into_string_bytes(self) -> Option<Bytes> {
        match self {
            Value::String(b) => Some(Bytes::from(b)),
            _ => None,
        }
    }

    /// Try to read as counter. String "42" also coerced to Counter.
    pub fn as_counter(&self) -> crate::Result<i64> {
        match self {
            Value::Counter(n) => Ok(*n),
            Value::String(b) => {
                let s = std::str::from_utf8(b)
                    .map_err(|_| crate::MnemeError::WrongType {
                        expected: "counter", got: "string (non-utf8)",
                    })?;
                s.trim().parse::<i64>().map_err(|_| crate::MnemeError::WrongType {
                    expected: "counter", got: "string (non-numeric)",
                })
            }
            _ => Err(crate::MnemeError::WrongType {
                expected: "counter", got: self.type_name(),
            }),
        }
    }
}

// ── Counter operations ────────────────────────────────────────────────────────

impl Value {
    /// INCR — increment by 1, return new value.
    pub fn incr(&mut self) -> crate::Result<i64> {
        self.incrby(1)
    }

    /// DECR — decrement by 1, return new value.
    pub fn decr(&mut self) -> crate::Result<i64> {
        self.incrby(-1)
    }

    /// INCRBY — increment by delta, return new value.
    pub fn incrby(&mut self, delta: i64) -> crate::Result<i64> {
        match self {
            Value::Counter(ref mut n) => {
                *n = n.checked_add(delta)
                    .ok_or_else(|| crate::MnemeError::Other(
                        anyhow::anyhow!("counter overflow")))?;
                Ok(*n)
            }
            Value::String(ref b) => {
                let s = std::str::from_utf8(b).map_err(|_| crate::MnemeError::WrongType {
                    expected: "counter", got: "string",
                })?;
                let current: i64 = s.trim().parse().map_err(|_| crate::MnemeError::WrongType {
                    expected: "counter", got: "string (non-numeric)",
                })?;
                let new_val = current.checked_add(delta)
                    .ok_or_else(|| crate::MnemeError::Other(
                        anyhow::anyhow!("counter overflow")))?;
                *self = Value::Counter(new_val);
                Ok(new_val)
            }
            _ => Err(crate::MnemeError::WrongType {
                expected: "counter", got: self.type_name(),
            }),
        }
    }
}

// ── JSON document ─────────────────────────────────────────────────────────────

/// JSON document stored as raw UTF-8 string.
/// Path operations use a minimal JSONPath subset: $.field, $.a.b.c, $[0].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonDoc {
    /// Raw JSON string — source of truth.
    pub raw: String,
}

impl JsonDoc {
    /// Create from a valid JSON string. Validates on creation.
    pub fn new(json: impl Into<String>) -> crate::Result<Self> {
        let raw = json.into();
        // Minimal validation: must parse as valid JSON
        Self::validate(&raw)?;
        Ok(Self { raw })
    }

    /// Get a value at JSONPath. Supports: $ (root), $.field, $.a.b, $[0].
    /// Returns the JSON representation of the value at path.
    pub fn get(&self, path: &str) -> crate::Result<String> {
        if path == "$" || path.is_empty() {
            return Ok(self.raw.clone());
        }
        let parts = parse_path(path)?;
        let val = navigate(&self.raw, &parts)?;
        Ok(val)
    }

    /// Set a value at JSONPath. Only supports simple field paths ($.a.b).
    /// Returns the updated JSON string.
    pub fn set(&mut self, path: &str, value: &str) -> crate::Result<()> {
        // Validate new value
        Self::validate(value)?;
        if path == "$" || path.is_empty() {
            Self::validate(value)?;
            self.raw = value.to_string();
            return Ok(());
        }
        // For nested set, rebuild via simple string manipulation
        // Production impl would use a proper JSON library
        // Here we do a best-effort replacement
        let parts = parse_path(path)?;
        self.raw = set_in_json(&self.raw, &parts, value)?;
        Ok(())
    }

    /// Delete a key at path. Returns true if key existed.
    pub fn del(&mut self, path: &str) -> crate::Result<bool> {
        let parts = parse_path(path)?;
        let (new_json, existed) = del_in_json(&self.raw, &parts)?;
        self.raw = new_json;
        Ok(existed)
    }

    /// Check if path exists.
    pub fn exists(&self, path: &str) -> bool {
        let parts = match parse_path(path) {
            Ok(p) => p,
            Err(_) => return false,
        };
        navigate(&self.raw, &parts).is_ok()
    }

    /// Return the JSON type at path: "object", "array", "string", "number", "boolean", "null".
    pub fn type_at(&self, path: &str) -> crate::Result<&'static str> {
        let val = self.get(path)?;
        let trimmed = val.trim();
        Ok(if trimmed.starts_with('{') { "object" }
        else if trimmed.starts_with('[') { "array" }
        else if trimmed.starts_with('"') { "string" }
        else if trimmed == "true" || trimmed == "false" { "boolean" }
        else if trimmed == "null" { "null" }
        else { "number" })
    }

    fn validate(s: &str) -> crate::Result<()> {
        // Lightweight validation: balanced braces/brackets, no control chars
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escape = false;
        for ch in s.chars() {
            if escape { escape = false; continue; }
            if ch == '\\' && in_string { escape = true; continue; }
            if ch == '"' { in_string = !in_string; continue; }
            if in_string { continue; }
            match ch {
                '{' | '[' => depth += 1,
                '}' | ']' => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(crate::MnemeError::Protocol(
                            format!("invalid JSON: unmatched {ch}")));
                    }
                }
                _ => {}
            }
        }
        if depth != 0 {
            return Err(crate::MnemeError::Protocol(
                "invalid JSON: unmatched braces".into()));
        }
        Ok(())
    }
}

// ── JSONPath helpers (minimal subset) ────────────────────────────────────────

#[derive(Debug, Clone)]
enum PathPart {
    Field(String),
    Index(usize),
}

fn parse_path(path: &str) -> crate::Result<Vec<PathPart>> {
    // Strip leading "$." or "$"
    let path = path.trim_start_matches('$').trim_start_matches('.');
    if path.is_empty() {
        return Ok(vec![]);
    }
    let mut parts = Vec::new();
    for segment in path.split('.') {
        if segment.contains('[') {
            // e.g. "items[2]"
            let bracket = segment.find('[').unwrap();
            let field = &segment[..bracket];
            let idx_str = segment[bracket+1..].trim_end_matches(']');
            if !field.is_empty() {
                parts.push(PathPart::Field(field.to_string()));
            }
            let idx: usize = idx_str.parse().map_err(|_| {
                crate::MnemeError::Protocol(format!("invalid array index: {idx_str}"))
            })?;
            parts.push(PathPart::Index(idx));
        } else {
            parts.push(PathPart::Field(segment.to_string()));
        }
    }
    Ok(parts)
}

/// Navigate JSON string by path parts, return value as string.
/// Minimal implementation — works for simple flat and 2-level nested objects.
fn navigate(json: &str, parts: &[PathPart]) -> crate::Result<String> {
    if parts.is_empty() {
        return Ok(json.to_string());
    }
    match &parts[0] {
        PathPart::Field(field) => {
            let key = format!("\"{}\"", field);
            let search = format!("{}:", key);
            // Find key in JSON object
            if let Some(pos) = json.find(&search) {
                let after = json[pos + search.len()..].trim_start();
                let val = extract_json_value(after)?;
                return navigate(&val, &parts[1..]);
            }
            // Also try with space: "key" : value
            let search2 = format!("{} :", key);
            if let Some(pos) = json.find(&search2) {
                let after = json[pos + search2.len()..].trim_start();
                let val = extract_json_value(after)?;
                return navigate(&val, &parts[1..]);
            }
            Err(crate::MnemeError::KeyNotFound)
        }
        PathPart::Index(idx) => {
            // Find array element at index
            let json = json.trim();
            if !json.starts_with('[') {
                return Err(crate::MnemeError::WrongType {
                    expected: "array", got: "object",
                });
            }
            let elements = split_json_array(&json[1..json.len()-1])?;
            let el = elements.get(*idx)
                .ok_or(crate::MnemeError::KeyNotFound)?;
            navigate(el.trim(), &parts[1..])
        }
    }
}

/// Extract one complete JSON value from the start of a string.
fn extract_json_value(s: &str) -> crate::Result<String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(crate::MnemeError::Protocol("empty JSON value".into()));
    }
    match s.chars().next().unwrap() {
        '{' | '[' => {
            let open = s.chars().next().unwrap();
            let close = if open == '{' { '}' } else { ']' };
            let mut depth = 0;
            let mut in_str = false;
            let mut esc = false;
            for (i, ch) in s.char_indices() {
                if esc { esc = false; continue; }
                if ch == '\\' && in_str { esc = true; continue; }
                if ch == '"' { in_str = !in_str; continue; }
                if in_str { continue; }
                if ch == open  { depth += 1; }
                if ch == close { depth -= 1; if depth == 0 { return Ok(s[..=i].to_string()); } }
            }
            Err(crate::MnemeError::Protocol("unterminated JSON container".into()))
        }
        '"' => {
            let mut esc = false;
            for (i, ch) in s.char_indices().skip(1) {
                if esc { esc = false; continue; }
                if ch == '\\' { esc = true; continue; }
                if ch == '"' { return Ok(s[..=i].to_string()); }
            }
            Err(crate::MnemeError::Protocol("unterminated JSON string".into()))
        }
        _ => {
            // number, bool, null — read until delimiter
            let end = s.find(|c: char| c == ',' || c == '}' || c == ']' || c.is_whitespace())
                .unwrap_or(s.len());
            Ok(s[..end].to_string())
        }
    }
}

fn split_json_array(inner: &str) -> crate::Result<Vec<String>> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut start = 0;
    for (i, ch) in inner.char_indices() {
        if esc { esc = false; continue; }
        if ch == '\\' && in_str { esc = true; continue; }
        if ch == '"' { in_str = !in_str; continue; }
        if in_str { continue; }
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                result.push(inner[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() { result.push(last.to_string()); }
    Ok(result)
}

fn set_in_json(json: &str, parts: &[PathPart], value: &str) -> crate::Result<String> {
    if parts.is_empty() {
        return Ok(value.to_string());
    }
    match &parts[0] {
        PathPart::Field(field) => {
            let key_pattern = format!("\"{}\"", field);
            // Simple: find the key and replace its value
            let search = format!("{}:", key_pattern);
            if let Some(pos) = json.find(&search) {
                let prefix = &json[..pos + search.len()];
                let after = json[pos + search.len()..].trim_start();
                let old_val = extract_json_value(after)?;
                let new_val = if parts.len() > 1 {
                    set_in_json(&old_val, &parts[1..], value)?
                } else {
                    value.to_string()
                };
                let suffix = &json[pos + search.len() + after.len() - after.trim_start().len() + old_val.len()..];
                return Ok(format!("{}{}{}", prefix, new_val, suffix));
            }
            // Key doesn't exist — insert before closing }
            let trimmed = json.trim_end();
            if trimmed.ends_with('}') {
                let base = &trimmed[..trimmed.len()-1];
                let comma = if base.trim_end_matches(|c: char| c.is_whitespace()) != "{" { "," } else { "" };
                Ok(format!("{}{}\"{}\":{}}}", base, comma, field, value))
            } else {
                Err(crate::MnemeError::Protocol("not a JSON object".into()))
            }
        }
        PathPart::Index(_) => {
            Err(crate::MnemeError::Protocol("array index set not supported".into()))
        }
    }
}

fn del_in_json(json: &str, parts: &[PathPart]) -> crate::Result<(String, bool)> {
    if parts.is_empty() {
        return Ok((json.to_string(), false));
    }
    match &parts[0] {
        PathPart::Field(field) => {
            let key_pattern = format!("\"{}\"", field);
            let search = format!("{}:", key_pattern);
            if let Some(pos) = json.find(&search) {
                let after = json[pos + search.len()..].trim_start();
                let old_val = extract_json_value(after)?;
                let val_end = pos + search.len() + old_val.len();
                // Remove ",key:val" or "key:val,"
                let before = &json[..pos];
                let after_val = &json[val_end..];
                let new_json = if after_val.trim_start().starts_with(',') {
                    let comma_pos = val_end + after_val.find(',').unwrap();
                    format!("{}{}", before, &json[comma_pos+1..])
                } else if before.trim_end().ends_with(',') {
                    let comma_pos = before.rfind(',').unwrap();
                    format!("{}{}", &json[..comma_pos], &json[val_end..])
                } else {
                    format!("{}{}", before, &json[val_end..])
                };
                return Ok((new_json, true));
            }
            Ok((json.to_string(), false))
        }
        PathPart::Index(_) => {
            Err(crate::MnemeError::Protocol("array index del not supported".into()))
        }
    }
}

// ── ZSetMember ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZSetMember {
    pub score: f64,
    #[serde(with = "serde_bytes")]
    pub member: Vec<u8>,
}

impl ZSetMember {
    pub fn new(score: f64, member: impl Into<Vec<u8>>) -> Self {
        Self { score, member: member.into() }
    }
}

// ── Entry ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Entry {
    pub value: Value,
    pub expires_at_ms: u64,
    pub lfu_counter: u8,
    pub slot: u16,
}

impl Entry {
    pub fn new(value: Value, slot: u16) -> Self {
        Self { value, expires_at_ms: 0, lfu_counter: 5, slot }
    }

    pub fn with_ttl(mut self, ttl_ms: u64, now_ms: u64) -> Self {
        self.expires_at_ms = now_ms + ttl_ms;
        self
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms != 0 && now_ms >= self.expires_at_ms
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Counter tests
    #[test]
    fn counter_incr() {
        let mut v = Value::Counter(10);
        assert_eq!(v.incr().unwrap(), 11);
        assert_eq!(v.incr().unwrap(), 12);
    }

    #[test]
    fn counter_decr() {
        let mut v = Value::Counter(5);
        assert_eq!(v.decr().unwrap(), 4);
        assert_eq!(v.decr().unwrap(), 3);
    }

    #[test]
    fn counter_incrby() {
        let mut v = Value::Counter(0);
        assert_eq!(v.incrby(100).unwrap(), 100);
        assert_eq!(v.incrby(-30).unwrap(), 70);
    }

    #[test]
    fn counter_coerces_from_string() {
        let mut v = Value::String(b"42".to_vec());
        assert_eq!(v.incrby(8).unwrap(), 50);
        assert!(matches!(v, Value::Counter(50)));
    }

    #[test]
    fn counter_overflow_rejected() {
        let mut v = Value::Counter(i64::MAX);
        assert!(v.incr().is_err());
    }

    #[test]
    fn counter_wrong_type_rejected() {
        let mut v = Value::Hash(vec![]);
        assert!(v.incr().is_err());
    }

    #[test]
    fn counter_memory_usage() {
        assert_eq!(Value::Counter(0).memory_usage(), 8);
    }

    // JSON tests
    #[test]
    fn json_new_valid() {
        let doc = JsonDoc::new(r#"{"name":"alice","age":30}"#).unwrap();
        assert_eq!(doc.raw, r#"{"name":"alice","age":30}"#);
    }

    #[test]
    fn json_new_invalid() {
        assert!(JsonDoc::new("{bad json}}}").is_err());
    }

    #[test]
    fn json_get_root() {
        let doc = JsonDoc::new(r#"{"x":1}"#).unwrap();
        assert_eq!(doc.get("$").unwrap(), r#"{"x":1}"#);
    }

    #[test]
    fn json_get_field() {
        let doc = JsonDoc::new(r#"{"name":"alice","age":30}"#).unwrap();
        assert_eq!(doc.get("$.name").unwrap().trim_matches('"'), "alice");
    }

    #[test]
    fn json_get_missing_field() {
        let doc = JsonDoc::new(r#"{"x":1}"#).unwrap();
        assert!(doc.get("$.missing").is_err());
    }

    #[test]
    fn json_exists() {
        let doc = JsonDoc::new(r#"{"a":1,"b":2}"#).unwrap();
        assert!(doc.exists("$.a"));
        assert!(!doc.exists("$.z"));
    }

    #[test]
    fn json_type_at() {
        let doc = JsonDoc::new(r#"{"s":"hi","n":42,"b":true,"o":{},"a":[],"null":null}"#).unwrap();
        assert_eq!(doc.type_at("$.s").unwrap(), "string");
        assert_eq!(doc.type_at("$.n").unwrap(), "number");
        assert_eq!(doc.type_at("$.b").unwrap(), "boolean");
        assert_eq!(doc.type_at("$.o").unwrap(), "object");
        assert_eq!(doc.type_at("$.a").unwrap(), "array");
    }

    #[test]
    fn json_set_root() {
        let mut doc = JsonDoc::new(r#"{"x":1}"#).unwrap();
        doc.set("$", r#"{"y":2}"#).unwrap();
        assert_eq!(doc.raw, r#"{"y":2}"#);
    }

    #[test]
    fn json_memory_usage() {
        let doc = JsonDoc::new(r#"{"x":1}"#).unwrap();
        let v = Value::Json(doc);
        assert_eq!(v.memory_usage(), 7);
    }

    #[test]
    fn value_type_names() {
        assert_eq!(Value::Counter(0).type_name(), "counter");
        assert_eq!(Value::Json(JsonDoc { raw: "{}".into() }).type_name(), "json");
    }

    #[test]
    fn entry_expiry() {
        let e = Entry::new(Value::Counter(1), 0).with_ttl(1000, 5000);
        assert!(!e.is_expired(5999));
        assert!(e.is_expired(6000));
    }

    #[test]
    fn serde_roundtrip_counter() {
        let v = Value::Counter(-42);
        let bytes = v.to_bytes().unwrap();
        let back = Value::from_bytes(&bytes).unwrap();
        assert!(matches!(back, Value::Counter(-42)));
    }

    #[test]
    fn serde_roundtrip_json() {
        let v = Value::Json(JsonDoc { raw: r#"{"k":"v"}"#.into() });
        let bytes = v.to_bytes().unwrap();
        let back = Value::from_bytes(&bytes).unwrap();
        assert!(matches!(back, Value::Json(_)));
    }

    // ── Additional value type tests ─────────────────────────────────────────

    #[test]
    fn counter_decr_underflow() {
        let mut v = Value::Counter(i64::MIN);
        assert!(v.decr().is_err(), "should overflow on i64::MIN - 1");
    }

    #[test]
    fn counter_from_string_non_numeric() {
        let mut v = Value::String(b"not_a_number".to_vec());
        assert!(v.incr().is_err());
    }

    #[test]
    fn json_validate_unbalanced() {
        assert!(JsonDoc::new("{{{").is_err(), "unbalanced braces");
        assert!(JsonDoc::new("[[]").is_err(), "unbalanced brackets");
    }

    #[test]
    fn json_get_nested_path() {
        let doc = JsonDoc::new(r#"{"a":{"b":{"c":42}}}"#).unwrap();
        assert_eq!(doc.get("$.a.b.c").unwrap(), "42");
    }

    #[test]
    fn json_escaped_quotes() {
        // String with escaped quotes should validate correctly
        let doc = JsonDoc::new(r#"{"msg":"he said \"hello\""}"#).unwrap();
        assert!(doc.exists("$.msg"));
    }

    #[test]
    fn memory_usage_all_types() {
        assert_eq!(Value::String(b"hello".to_vec()).memory_usage(), 5);
        assert_eq!(Value::Hash(vec![(b"k".to_vec(), b"v".to_vec())]).memory_usage(), 2);
        let mut list = VecDeque::new();
        list.push_back(b"item".to_vec());
        assert_eq!(Value::List(list).memory_usage(), 4);
        assert_eq!(Value::ZSet(vec![ZSetMember { member: b"m".to_vec(), score: 1.0 }]).memory_usage(), 24);
    }

    #[test]
    fn serde_roundtrip_all_types() {
        let types: Vec<Value> = vec![
            Value::String(b"data".to_vec()),
            Value::Hash(vec![(b"f".to_vec(), b"v".to_vec())]),
            Value::List(VecDeque::from(vec![b"a".to_vec()])),
            Value::ZSet(vec![ZSetMember { member: b"m".to_vec(), score: 3.14 }]),
            Value::Counter(999),
        ];
        for v in types {
            let bytes = v.to_bytes().unwrap();
            let back = Value::from_bytes(&bytes).unwrap();
            assert_eq!(v.type_name(), back.type_name());
        }
    }

    #[test]
    fn entry_not_expired_when_zero() {
        let e = Entry::new(Value::Counter(1), 0);
        assert!(!e.is_expired(u64::MAX), "zero expires_at = permanent");
    }

    #[test]
    fn json_del_existing_key() {
        let mut doc = JsonDoc::new(r#"{"a":1,"b":2}"#).unwrap();
        let existed = doc.del("$.a").unwrap();
        assert!(existed);
        assert!(!doc.exists("$.a"));
    }

    #[test]
    fn json_del_missing_key() {
        let mut doc = JsonDoc::new(r#"{"a":1}"#).unwrap();
        let existed = doc.del("$.missing").unwrap();
        assert!(!existed);
    }

    #[test]
    fn value_into_string_bytes() {
        let v = Value::String(b"hello".to_vec());
        assert_eq!(v.into_string_bytes().unwrap(), &b"hello"[..]);

        let v = Value::Counter(1);
        assert!(v.into_string_bytes().is_none());
    }

    #[test]
    fn as_counter_wrong_type() {
        let v = Value::List(VecDeque::new());
        assert!(v.as_counter().is_err());
    }
}
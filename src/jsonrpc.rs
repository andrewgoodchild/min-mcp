//! One definition of the JSON-RPC 2.0 response envelope, shared by the server
//! half (main.rs) and the client half (upstream.rs) so framing can't drift.

use serde_json::{json, Value};

pub const METHOD_NOT_FOUND: i64 = -32601;

/// A message's `id` as i64 — tolerating the float/string echoes some servers
/// produce (`1.0`, `"1"`) for the integer ids we send. Shared by both client
/// transports so one server is not matched by two different rules.
pub fn response_id(v: &Value) -> Option<i64> {
    let id = v.get("id")?;
    id.as_i64()
        .or_else(|| id.as_f64().map(|f| f as i64))
        .or_else(|| id.as_str().and_then(|s| s.parse().ok()))
}

pub fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error(id: &Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_envelope_carries_id_and_result() {
        let r = result(&json!(7), json!({"ok": true}));
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], json!(7));
        assert_eq!(r["result"]["ok"], json!(true));
        assert!(r.get("error").is_none());
    }

    #[test]
    fn error_envelope_carries_code_message_and_no_result() {
        let e = error(&json!("abc"), METHOD_NOT_FOUND, "nope".into());
        assert_eq!(e["id"], json!("abc"));
        assert_eq!(e["error"]["code"], json!(-32601));
        assert_eq!(e["error"]["message"], "nope");
        assert!(e.get("result").is_none());
    }

    #[test]
    fn response_ids_tolerate_float_and_string_echoes() {
        assert_eq!(response_id(&json!({"id": 7})), Some(7));
        assert_eq!(response_id(&json!({"id": 7.0})), Some(7));
        assert_eq!(response_id(&json!({"id": "7"})), Some(7));
        assert_eq!(response_id(&json!({"id": null})), None);
        assert_eq!(response_id(&json!({"method": "x"})), None);
    }

    #[test]
    fn deeply_nested_json_is_rejected_not_a_stack_overflow() {
        // A billion-laughs / deep-nesting DoS at the parse boundary must be a clean
        // Err, not a crash. serde_json's recursion limit (128) protects us — this
        // pins that protection so a future `disable_recursion_limit` can't sneak in.
        let deep = "[".repeat(50_000) + &"]".repeat(50_000);
        assert!(serde_json::from_str::<Value>(&deep).is_err());
        // and a deeply-nested object likewise
        let obj = "{\"a\":".repeat(50_000);
        assert!(serde_json::from_str::<Value>(&obj).is_err());
    }
}

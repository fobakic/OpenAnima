//! Wire protocol shared by `openanima-core` and the UI shell.
//!
//! Transport: Windows named pipe `\\.\pipe\openanima-core-v1`.
//! Framing: 4-byte little-endian length prefix + UTF-8 JSON body.
//! Every request carries an `id` that is echoed back in the response, so a
//! client can pipeline requests over one connection.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PIPE_NAME: &str = r"\\.\pipe\openanima-core-v1";
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum accepted frame size (16 MiB). Guards against a malformed or
/// hostile length prefix turning into a huge allocation.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub cmd: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Response { id, ok: true, result: Some(result), error: None }
    }

    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Response { id, ok: false, result: None, error: Some(error.into()) }
    }
}

/// Unsolicited notification pushed from the core to every connected client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    #[serde(default)]
    pub data: Value,
}

impl Event {
    pub fn new(event: impl Into<String>, data: Value) -> Self {
        Event { event: event.into(), data }
    }
}

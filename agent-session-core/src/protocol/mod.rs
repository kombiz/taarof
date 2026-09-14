//! Protocol v1: exactly one JSON response line per short-lived invocation.
use serde::{Deserialize, Serialize};
#[derive(Serialize)]
pub struct Request<'a> {
    pub protocol: u32,
    pub operation: &'a str,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<&'a str>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub protocol: u32,
    pub id: String,
    pub display_name: String,
    pub capabilities: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub protocol: u32,
    pub available: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    pub protocol: u32,
    pub sessions: Vec<Session>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    pub updated_at_unix_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub protocol: u32,
    pub program: String,
    pub argv: Vec<String>,
    pub cwd: String,
}

//! In-process turn record store (observable via the control endpoint).
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Default)]
pub struct TurnRecord {
    pub protocol: String,
    pub session_id: String,
    pub user_input: String,
    pub final_output: String,
    /// Verbatim business content (D4 content fidelity): original request and
    /// response bodies. Transport-layer noise (per-hop headers, TCP metadata)
    /// is never captured here because only body bytes are accumulated.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub raw_request: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub raw_response: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub breakpoint: bool,
    /// Turn timing in unix nanoseconds (0 = unknown).
    #[serde(default)]
    pub start_ns: u64,
    #[serde(default)]
    pub end_ns: u64,
    /// Token usage reported by the model protocol (None = not reported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::trace::adaptor::TurnUsage>,
    /// Protocol error marker (response.failed / event:error) when the turn
    /// terminated abnormally; serde-skipped when None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Default)]
pub struct TraceStore {
    records: Arc<Mutex<Vec<TurnRecord>>>,
}

impl TraceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, record: TurnRecord) {
        self.records.lock().push(record);
    }

    pub fn snapshot(&self) -> Vec<TurnRecord> {
        self.records.lock().clone()
    }
}

//! In-process turn record store (observable via the control endpoint).
use atg_model::TurnRecord;
use parking_lot::Mutex;
use std::sync::Arc;

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

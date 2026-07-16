//! A registered webhook: an URL to POST an event to, filtered by event name.
//! An empty `events` list matches every event.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub events: Vec<String>,
}

impl Webhook {
    /// Whether this hook wants to hear about `event`. Empty filter or a `*`
    /// entry means everything.
    pub fn wants(&self, event: &str) -> bool {
        self.events.is_empty() || self.events.iter().any(|e| e == event || e == "*")
    }
}

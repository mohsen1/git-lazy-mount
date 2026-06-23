//! Structured output envelopes (spec §6).

use serde::Serialize;
use serde_json::Value;

/// The JSON schema version emitted by all structured output.
pub const SCHEMA_VERSION: u32 = 1;

/// Whether to render human text, a single JSON object, or JSON Lines.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Human-readable text.
    Human,
    /// A single JSON object.
    Json,
    /// Newline-delimited JSON records.
    JsonLines,
}

/// A response envelope carrying stable metadata (spec §6).
#[derive(Serialize)]
pub struct Envelope {
    /// Output schema version.
    pub schema_version: u32,
    /// The command that produced this output.
    pub command: String,
    /// Owning workspace id, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Mount generation, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount_generation: Option<u64>,
    /// Operation id, if this command sealed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Command-specific result payload.
    pub result: Value,
    /// Non-fatal warnings.
    pub warnings: Vec<String>,
}

impl Envelope {
    /// Build an envelope for `command` with a result payload.
    pub fn new(command: &str, result: Value) -> Envelope {
        Envelope {
            schema_version: SCHEMA_VERSION,
            command: command.to_string(),
            workspace_id: None,
            mount_generation: None,
            operation_id: None,
            result,
            warnings: Vec::new(),
        }
    }

    /// Attach the workspace id.
    pub fn workspace(mut self, id: impl Into<String>) -> Self {
        self.workspace_id = Some(id.into());
        self
    }

    /// Attach a warning.
    pub fn warn(mut self, w: impl Into<String>) -> Self {
        self.warnings.push(w.into());
        self
    }

    /// Print this envelope as a single JSON object.
    pub fn print_json(&self) {
        println!("{}", serde_json::to_string_pretty(self).unwrap_or_default());
    }
}

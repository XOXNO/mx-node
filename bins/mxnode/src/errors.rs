//! Error UX commitment from the plan: every error printed by mxnode follows
//! the 3-line shape:
//!     error: <one-line summary>
//!       cause: <underlying technical cause>
//!        try: <next concrete step>
//!
//! `--json` emits the same as `{"error": {"summary","cause","try"}}`.

use std::fmt::Display;

use serde::Serialize;

/// What we accept from any command that fails. The orchestrator builds this
/// directly; library errors are wrapped at the call site so the surface text
/// stays operator-friendly.
#[derive(Debug, Serialize)]
pub struct CliError {
    pub summary: String,
    pub cause: String,
    pub r#try: String,
    #[serde(skip)]
    pub json: bool,
    /// Skip the `report_error` printer entirely. Commands that emit their
    /// own structured payload (e.g. `doctor --json`, which writes a unified
    /// JSON object covering both findings and the error) set this to avoid
    /// a second blob landing on stdout.
    #[serde(skip)]
    pub silent: bool,
}

impl CliError {
    pub fn new(
        summary: impl Into<String>,
        cause: impl Into<String>,
        try_: impl Into<String>,
    ) -> Self {
        Self {
            summary: summary.into(),
            cause: cause.into(),
            r#try: try_.into(),
            json: false,
            silent: false,
        }
    }

    pub fn json(mut self) -> Self {
        self.json = true;
        self
    }

    /// Mark this error as already-reported. The dispatcher's
    /// `report_error` will skip output but still return a non-zero exit
    /// code. Use this when a command has already emitted JSON or a
    /// human-readable payload that includes the error context.
    pub fn silent(mut self) -> Self {
        self.silent = true;
        self
    }
}

impl Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.summary)
    }
}

impl std::error::Error for CliError {}

pub fn report_error(err: CliError) {
    if err.silent {
        // The command already printed its own structured output that
        // includes the error context; we just need the non-zero exit code.
        return;
    }
    if err.json {
        // The shape is part of the public contract for `--json`.
        let payload = serde_json::json!({
            "error": {
                "summary": err.summary,
                "cause": err.cause,
                "try": err.r#try,
            }
        });
        println!("{}", payload);
    } else {
        eprintln!("error: {}", err.summary);
        eprintln!("  cause: {}", err.cause);
        eprintln!("   try: {}", err.r#try);
    }
}

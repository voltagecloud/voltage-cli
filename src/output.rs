//! Result envelopes, secret redaction, and terminal rendering.
//!
//! Every successful command writes one or more envelopes to stdout or to a private file.
//! Diagnostics go to stderr so scripts can parse stdout.

use crate::{Error, Result, config::new_private, error::ErrorDetail};
use clap::ValueEnum;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{IsTerminal, Write},
    path::Path,
};
use uuid::Uuid;

/// Keys whose values are never printed, whatever their nesting.
const SECRET_KEYS: [&str; 12] = [
    "api_key",
    "access_token",
    "refresh_token",
    "id_token",
    "checkout_token",
    "checkout_tokens",
    "stream_token",
    "device_code",
    "shared_secret",
    "secret",
    "checkout_url",
    "preimage",
];

/// Columns shown for list results in table output, in display order.
const TABLE_COLUMNS: [&str; 5] = ["id", "name", "status", "currency", "environment_id"];

const REDACTED: &str = "[REDACTED]";

/// Stdout rendering selected by `--json`, `--output`, or the terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    /// Readable columns for people; falls back to pretty JSON.
    Table,
    /// One stable envelope, with `--all` pages collected into `data.pages`.
    Json,
    /// One envelope per line as each page or event arrives.
    Ndjson,
}

impl OutputFormat {
    /// `--json` wins, then `--output`, then the terminal decides.
    pub fn select(json: bool, explicit: Option<Self>) -> Self {
        if json {
            Self::Json
        } else if let Some(format) = explicit {
            format
        } else if std::io::stdout().is_terminal() {
            Self::Table
        } else {
            Self::Json
        }
    }

    /// Machine-readable formats also report errors as JSON.
    pub fn is_machine_readable(self) -> bool {
        matches!(self, Self::Json | Self::Ndjson)
    }
}

/// What a result envelope says about the operation it reports.
#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// A local or mutating command finished.
    Succeeded,
    /// The API accepted a submission that continues on the server.
    Accepted,
    /// A read returned its projection.
    Retrieved,
    /// One server-sent event from a checkout stream.
    Event,
    /// The payer-facing invoice or address is available.
    Ready,
    /// The payment settled.
    Completed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Accepted => "accepted",
            Self::Retrieved => "retrieved",
            Self::Event => "event",
            Self::Ready => "ready",
            Self::Completed => "completed",
        }
    }
}

/// Fields present only on checkout stream event envelopes.
#[derive(Clone, Debug, Serialize)]
pub struct StreamEvent {
    pub event: String,
    pub id: Option<String>,
}

/// The stable result envelope written for every successful command.
#[derive(Clone, Debug, Serialize)]
pub struct Envelope {
    pub http_status: Option<u16>,
    pub data: Value,
    pub resource_id: Option<Uuid>,
    pub outcome: Outcome,
    #[serde(flatten)]
    pub event: Option<StreamEvent>,
}

impl Envelope {
    pub fn new(
        http_status: Option<u16>,
        data: Value,
        resource_id: Option<Uuid>,
        outcome: Outcome,
    ) -> Self {
        Self {
            http_status,
            data,
            resource_id,
            outcome,
            event: None,
        }
    }

    /// A local command result with no HTTP status or resource.
    pub fn local(data: impl Serialize) -> Result<Self> {
        Ok(Self::new(
            None,
            serde_json::to_value(data)?,
            None,
            Outcome::Succeeded,
        ))
    }

    /// One checkout stream event.
    pub fn event(data: Value, event: String, id: Option<String>) -> Self {
        Self {
            event: Some(StreamEvent { event, id }),
            ..Self::new(Some(200), data, None, Outcome::Event)
        }
    }
}

/// Destination and rendering for result envelopes.
pub struct Output {
    format: OutputFormat,
    show_secrets: bool,
    file: Option<File>,
}

impl Output {
    /// `output_file` is reserved immediately as a new owner-only file so a secret never
    /// waits on a path that another process could create first.
    pub fn new(
        format: OutputFormat,
        show_secrets: bool,
        output_file: Option<&Path>,
    ) -> Result<Self> {
        let file = output_file.map(new_private).transpose()?;
        Ok(Self {
            format,
            show_secrets,
            file,
        })
    }

    /// One-time secrets may only go to a private file or an explicitly unredacted stream.
    pub fn secure_destination(&self) -> bool {
        self.show_secrets || self.file.is_some()
    }

    /// JSON output collects `--all` pages into one envelope; NDJSON streams them.
    pub fn collects_pages(&self) -> bool {
        self.format == OutputFormat::Json
    }

    pub fn write(&mut self, mut envelope: Envelope, secrets: &[&str]) -> Result<()> {
        if let Some(file) = &mut self.file {
            serde_json::to_writer(&mut *file, &envelope)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            return Ok(());
        }
        if !self.show_secrets {
            redact(&mut envelope.data, secrets);
        }
        let mut stdout = std::io::stdout().lock();
        match self.format {
            OutputFormat::Table => human(&mut stdout, &envelope)?,
            OutputFormat::Json | OutputFormat::Ndjson => {
                serde_json::to_writer(&mut stdout, &envelope)?;
                stdout.write_all(b"\n")?;
            }
        }
        stdout.flush()?;
        Ok(())
    }
}

/// Replace known secret fields and any presented credential text, recursively.
pub fn redact(value: &mut Value, secrets: &[&str]) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                    *value = json!(REDACTED);
                } else {
                    redact(value, secrets);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact(value, secrets);
            }
        }
        Value::String(text) => {
            for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
                *text = text.replace(secret, REDACTED);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn human(out: &mut impl Write, envelope: &Envelope) -> Result<()> {
    let resource = envelope
        .resource_id
        .map(|id| format!("  {id}"))
        .unwrap_or_default();
    writeln!(out, "{}{resource}", envelope.outcome.as_str())?;
    let data = &envelope.data;
    let items = data
        .as_array()
        .or_else(|| data.get("items").and_then(Value::as_array));
    if let Some(items) = items {
        if items.is_empty() {
            writeln!(out, "No results.")?;
            return Ok(());
        }
        let columns: Vec<_> = TABLE_COLUMNS
            .iter()
            .filter(|column| items.iter().any(|item| item.get(**column).is_some()))
            .collect();
        if columns.is_empty() {
            writeln!(out, "{}", serde_json::to_string_pretty(data)?)?;
        } else {
            let header: Vec<_> = columns.iter().map(|column| column.to_uppercase()).collect();
            writeln!(out, "{}", header.join("  "))?;
            for item in items {
                let cells: Vec<_> = columns.iter().map(|column| cell(&item[**column])).collect();
                writeln!(out, "{}", cells.join("  "))?;
            }
        }
    } else if !data.is_null() {
        writeln!(out, "{}", serde_json::to_string_pretty(data)?)?;
    }
    if let Some(cursor) = data.get("next_cursor").and_then(Value::as_str) {
        writeln!(out, "Next cursor: {cursor}")?;
    }
    Ok(())
}

/// One table cell; control characters would let API data rewrite the terminal.
fn cell(value: &Value) -> String {
    value
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| value.to_string())
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[derive(Serialize)]
struct ErrorReport<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    message: &'a str,
    exit_code: i32,
    detail: Option<&'a ErrorDetail>,
}

/// Print an error to stderr in the format the selected output uses.
pub fn report_error(error: &Error, format: OutputFormat) {
    let report = ErrorReport {
        error: ErrorBody {
            message: &error.message,
            exit_code: error.kind.exit_code(),
            detail: error.detail.as_ref(),
        },
    };
    let mut body = serde_json::to_value(report)
        .unwrap_or_else(|_| json!({"error": {"message": error.message}}));
    redact(&mut body, &[]);
    if format.is_machine_readable() {
        eprintln!("{body}");
    } else {
        eprintln!("{}", error.message);
        if let Some(detail) = body["error"]["detail"].as_object() {
            eprintln!("{}", json!(detail));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_recursively() {
        let mut value = json!({"data":{"shared_secret":"abc","description":"token abc"}});
        redact(&mut value, &["abc"]);
        assert!(!value.to_string().contains("abc"));
    }

    #[test]
    fn envelopes_keep_their_documented_shape() {
        let id = Uuid::nil();
        let envelope = Envelope::new(Some(202), Value::Null, Some(id), Outcome::Accepted);
        assert_eq!(
            serde_json::to_value(envelope).unwrap(),
            json!({"http_status":202,"data":null,"resource_id":id,"outcome":"accepted"})
        );
        let event = Envelope::event(json!({"status":"completed"}), "updated".into(), None);
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({"http_status":200,"data":{"status":"completed"},"resource_id":null,"outcome":"event","event":"updated","id":null})
        );
        for outcome in [
            Outcome::Succeeded,
            Outcome::Accepted,
            Outcome::Retrieved,
            Outcome::Event,
            Outcome::Ready,
            Outcome::Completed,
        ] {
            assert_eq!(serde_json::to_value(outcome).unwrap(), outcome.as_str());
        }
    }

    #[test]
    fn tables_show_known_columns_and_strip_control_characters() {
        let envelope = Envelope::new(
            Some(200),
            json!({"items":[{"id":"a\u{1b}[31m","name":"x"}],"next_cursor":"n"}),
            None,
            Outcome::Retrieved,
        );
        let mut out = Vec::new();
        human(&mut out, &envelope).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "retrieved\nID  NAME\na [31m  x\nNext cursor: n\n");
    }

    #[test]
    fn explicit_json_beats_the_output_flag() {
        assert_eq!(
            OutputFormat::select(true, Some(OutputFormat::Table)),
            OutputFormat::Json
        );
        assert_eq!(
            OutputFormat::select(false, Some(OutputFormat::Ndjson)),
            OutputFormat::Ndjson
        );
        assert!(
            OutputFormat::Ndjson.is_machine_readable()
                && !OutputFormat::Table.is_machine_readable()
        );
    }
}

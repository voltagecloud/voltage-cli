//! Result envelopes, secret redaction, and terminal rendering.
//!
//! Every successful command writes one or more envelopes to stdout or to a private file.
//! Diagnostics go to stderr so scripts can parse stdout.

mod human;

use crate::{
    Error, Result,
    config::new_private,
    error::ErrorDetail,
    terminal::{self, scrub_line},
};
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
    /// `--dry-run` described a request without sending it.
    #[serde(rename = "dry_run")]
    DryRun,
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
            Self::DryRun => "dry_run",
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
    /// How a person reads the data; never part of the JSON.
    #[serde(skip)]
    pub layout: Layout,
}

/// How table output lays out a single result. Decided by the command that built the data, not
/// guessed from its shape, so API or price data can never be read as local settings.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Layout {
    /// Aligned `field  value` lines.
    #[default]
    Fields,
    /// `{value, source}` settings shown as `name  value  (source)`, from `voltage context`.
    Settings,
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
            layout: Layout::Fields,
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

    /// A local result made of `{value, source}` settings.
    pub fn settings(data: impl Serialize) -> Result<Self> {
        Ok(Self {
            layout: Layout::Settings,
            ..Self::local(data)?
        })
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
            let bytes = serde_json::to_vec(&envelope)?;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            return Ok(());
        }
        if !self.show_secrets {
            redact(&mut envelope.data, secrets);
        }
        let mut bytes = match self.format {
            OutputFormat::Table => {
                let mut buf = Vec::new();
                human::render(&mut buf, &envelope, terminal::width())?;
                buf
            }
            OutputFormat::Json | OutputFormat::Ndjson => serde_json::to_vec(&envelope)?,
        };
        if self.format != OutputFormat::Table {
            bytes.push(b'\n');
        }
        write_stdout(&bytes)
    }
}

/// Write rendered output; a closed stdout ends the pipeline successfully.
pub fn write_stdout(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(bytes)
        .and_then(|()| stdout.flush())
        .map_err(|error| Error {
            stdout_closed: error.kind() == std::io::ErrorKind::BrokenPipe,
            ..error.into()
        })
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

#[derive(Serialize)]
struct ErrorReport<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    message: &'a str,
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
    detail: Option<&'a ErrorDetail>,
}

/// Print an error to stderr in the format the selected output uses.
pub fn report_error(error: &Error, format: OutputFormat) {
    let report = ErrorReport {
        error: ErrorBody {
            message: &error.message,
            exit_code: error.kind.exit_code(),
            hint: error.hint,
            detail: error.detail.as_ref(),
        },
    };
    let mut body = serde_json::to_value(report)
        .unwrap_or_else(|_| json!({"error": {"message": error.message}}));
    redact(&mut body, &[]);
    if format.is_machine_readable() {
        eprintln!("{body}");
    } else {
        // Messages can interpolate untrusted input; keep control characters off the terminal.
        eprintln!("error: {}", scrub_line(&error.message));
        if let Some(hint) = error.hint {
            // Hints are static today; scrub defensively if they ever become dynamic.
            eprintln!("hint: {}", scrub_line(hint));
        }
        if let Some(detail) = body["error"]["detail"].as_object() {
            eprintln!("details: {}", scrub_line(&json!(detail).to_string()));
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
            Outcome::DryRun,
        ] {
            assert_eq!(serde_json::to_value(outcome).unwrap(), outcome.as_str());
        }
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

//! Result envelopes, secret redaction, and terminal rendering.
//!
//! Every successful command writes one or more envelopes to stdout or to a private file.
//! Diagnostics go to stderr so scripts can parse stdout.

mod human;

use crate::{
    Error, Result,
    config::{EXECUTE_VARIABLE, new_private},
    error::{ErrorDetail, ErrorKind},
    style::{self, ColorChoice, Depth, Glyphs, Line, Role},
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
    /// A change without `--execute` was described without being sent.
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
    /// On a terminal, a single result is drawn as a card with this title.
    #[serde(skip)]
    pub card: Option<&'static str>,
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
            card: None,
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

    /// Draw the result as a card with `title` on a terminal.
    pub fn titled(self, title: &'static str) -> Self {
        Self {
            card: Some(title),
            ..self
        }
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
    color: ColorChoice,
}

/// Reserve `--output-file`. It is reserved before any request, so a refusal sent nothing; the
/// error says why the path was refused and what to pass instead.
fn reserve_output_file(path: &Path) -> Result<File> {
    new_private(path).map_err(|error| {
        if path.symlink_metadata().is_ok() {
            Error::usage(format!(
                "{} already exists; --output-file writes only to a new file",
                path.display()
            ))
            .with_hint("Pass a path that does not exist yet, or move the existing file. No change was sent.")
        } else {
            error.and_hint("Pass a path in a directory you can write to. No change was sent.")
        }
    })
}

impl Output {
    /// `output_file` is reserved immediately as a new owner-only file so a secret never
    /// waits on a path that another process could create first.
    pub fn new(
        format: OutputFormat,
        show_secrets: bool,
        output_file: Option<&Path>,
        color: ColorChoice,
    ) -> Result<Self> {
        let file = output_file.map(reserve_output_file).transpose()?;
        Ok(Self {
            format,
            show_secrets,
            file,
            color,
        })
    }

    /// One-time secrets may only go to a private file or an explicitly unredacted stream.
    pub fn secure_destination(&self) -> bool {
        self.show_secrets || self.file.is_some()
    }

    /// Human output, where a next step on stderr is read rather than parsed.
    pub fn is_human(&self) -> bool {
        !self.format.is_machine_readable()
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
        if self.format == OutputFormat::Table {
            // Styled as for a terminal; the stream strips styles when color is off there.
            let view = human::View {
                width: terminal::width(),
                color: true,
                cards: std::io::stdout().is_terminal(),
                glyphs: Glyphs::detect(|name| std::env::var(name).ok()),
                depth: Depth::detect(|name| std::env::var(name).ok()),
                ..human::View::plain()
            };
            let mut buf = Vec::new();
            human::render(&mut buf, &envelope, view)?;
            return write_stdout_styled(&buf, self.color);
        }
        let mut bytes = serde_json::to_vec(&envelope)?;
        bytes.push(b'\n');
        write_stdout(&bytes)
    }
}

/// Write rendered output; a closed stdout ends the pipeline successfully.
pub fn write_stdout(bytes: &[u8]) -> Result<()> {
    finish_stdout(std::io::stdout().lock(), bytes)
}

/// `write_stdout` for human output, keeping its styles only when `color` allows them there.
fn write_stdout_styled(bytes: &[u8], color: ColorChoice) -> Result<()> {
    finish_stdout(style::stdout(color), bytes)
}

fn finish_stdout(mut stdout: impl Write, bytes: &[u8]) -> Result<()> {
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
        Value::String(text) => redact_text(text, secrets),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Replace any presented credential text in free text, such as a message quoting the API.
pub fn redact_text(text: &mut String, secrets: &[&str]) {
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        *text = text.replace(secret, REDACTED);
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
    hint: Option<&'a str>,
    detail: Option<&'a ErrorDetail>,
}

/// Print an error to stderr in the format the selected output uses; `color` styles only the
/// human report's labels.
pub fn report_error(error: &Error, format: OutputFormat, color: ColorChoice) {
    let report = ErrorReport {
        error: ErrorBody {
            message: &error.message,
            exit_code: error.kind.exit_code(),
            hint: error.hint.as_deref(),
            detail: error.detail.as_ref(),
        },
    };
    let mut body = serde_json::to_value(report)
        .unwrap_or_else(|_| json!({"error": {"message": error.message}}));
    redact(&mut body, &[]);
    if format.is_machine_readable() {
        eprintln!("{body}");
    } else if error.kind == ErrorKind::NotExecuted && std::io::stderr().is_terminal() {
        // The description above says it was not sent; what is left is how to send it.
        let step = Glyphs::detect(|name| std::env::var(name).ok()).step;
        style::write_stderr(color, &format!("{}\n", send_it(step).as_str()));
    } else {
        style::write_stderr(color, &human_error(error, &body["error"]["detail"]));
    }
}

/// On a terminal, the line after a described change.
fn send_it(step: &'static str) -> Line {
    Line::default()
        .paint(Role::Hint, step)
        .text(" Send it: add ")
        .styled(Role::Emphasis, "--execute")
        .text(", or set ")
        .styled(Role::Emphasis, &format!("{EXECUTE_VARIABLE}=1"))
}

/// The human error report with painted labels, which the stream strips when color is off.
fn human_error(error: &Error, detail: &Value) -> String {
    // Messages can interpolate untrusted input; keep control characters off the terminal.
    let mut report = format!(
        "{} {}\n",
        style::paint(Role::Failure, "error:"),
        scrub_line(&error.message)
    );
    if let Some(hint) = &error.hint {
        // Hints can name saved credentials and profiles, so they are scrubbed like messages.
        let hint = scrub_line(hint);
        report.push_str(&format!("{} {hint}\n", style::paint(Role::Hint, "hint:")));
    }
    let repeated = error.detail.as_ref().is_some_and(ErrorDetail::repeats_hint);
    if let Some(detail) = human_detail(detail, &error.message).filter(|_| !repeated) {
        let mut lines = Vec::new();
        if human::detail(&mut lines, &detail).is_ok() {
            report.push_str(&format!("{}\n", style::paint(Role::Muted, "details:")));
            for line in String::from_utf8_lossy(&lines).lines() {
                report.push_str(&format!("  {line}\n"));
            }
        }
    }
    report
}

/// A JSON object as the aligned, scrubbed `field  value` lines human output uses.
pub fn field_lines(value: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    match human::detail(&mut lines, value) {
        Ok(()) => String::from_utf8_lossy(&lines)
            .lines()
            .map(str::to_owned)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The part of an error's detail a person needs. A client error (4xx) shows the API's error
/// without the reason `message` already gives; `--json` keeps the whole response.
fn human_detail(detail: &Value, message: &str) -> Option<Value> {
    let status = detail.get("http_status").and_then(Value::as_u64);
    let detail = match (status, detail.get("data")) {
        (Some(status), Some(data)) => match data.get("error") {
            Some(Value::Object(error)) => {
                let mut error = error.clone();
                if (400..500).contains(&status)
                    && error
                        .get("detail")
                        .and_then(Value::as_str)
                        .is_some_and(|reason| message.contains(reason))
                {
                    error.remove("detail");
                }
                // Keep the response's other fields, such as a request ID, beside the error's.
                for (key, value) in data.as_object().into_iter().flatten() {
                    if key != "error" {
                        error.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
                Value::Object(error)
            }
            _ => data.clone(),
        },
        _ => detail.clone(),
    };
    match &detail {
        Value::Null => None,
        Value::Object(map) if map.is_empty() => None,
        _ => Some(detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_details_drop_the_reason_the_message_already_gives() {
        let message = "Voltage API returned HTTP 400: Quote is required for payment";
        let rejection = json!({"http_status": 400, "data": {"error": {
            "type": "quote_required", "code": "required",
            "detail": "Quote is required for payment",
            "context": {"field": "quote_id", "required": true}
        }}});
        assert_eq!(
            human_detail(&rejection, message),
            Some(json!({"type": "quote_required", "code": "required",
                "context": {"field": "quote_id", "required": true}}))
        );
        let reason_only = json!({"http_status": 400, "data": {"error": {"detail": "too small"}}});
        assert_eq!(
            human_detail(&reason_only, "Voltage API returned HTTP 400: too small"),
            None
        );
        // A reason the message does not carry stays, so human output never loses it.
        let untyped = json!({"http_status": 400, "data": {"error": {
            "type": null, "detail": "Wallet is frozen"
        }}});
        assert_eq!(
            human_detail(&untyped, "Voltage API returned HTTP 400"),
            Some(json!({"type": null, "detail": "Wallet is frozen"}))
        );
        // Fields beside the error, such as a request ID, are kept.
        let traced = json!({"http_status": 400, "data": {
            "error": {"detail": "too small"}, "request_id": "req-1"
        }});
        assert_eq!(
            human_detail(&traced, "Voltage API returned HTTP 400: too small"),
            Some(json!({"request_id": "req-1"}))
        );
        let server = json!({"http_status": 500, "data": {"error": {"detail": "db down"}}});
        assert_eq!(
            human_detail(&server, "Voltage API returned HTTP 500: db down"),
            Some(json!({"detail": "db down"}))
        );
        let unstructured = json!({"http_status": 502, "data": "bad gateway"});
        assert_eq!(human_detail(&unstructured, ""), Some(json!("bad gateway")));
        assert_eq!(
            human_detail(&json!({"http_status": 502, "data": null}), ""),
            None
        );
        let pending = json!({"resource_id": "id", "outcome": "pending"});
        assert_eq!(human_detail(&pending, ""), Some(pending.clone()));
        assert_eq!(human_detail(&Value::Null, ""), None);
    }

    #[test]
    fn human_errors_paint_only_their_labels() {
        let hostile = "bad\u{1b}[2J\u{1b}]8;;https://evil.test\u{7}key";
        let mut error = Error::usage(format!("Rejected {hostile}"));
        error.hint = Some(format!("Try {hostile}").into());
        let report = human_error(&error, &json!({"request_id": hostile}));
        let plain = "error: Rejected bad [2J ]8;;https://evil.test key\n\
            hint: Try bad [2J ]8;;https://evil.test key\n\
            details:\n  request_id  bad [2J ]8;;https://evil.test key\n";
        assert_eq!(anstream::adapter::strip_str(&report).to_string(), plain);
        // Every escape sequence left is one of the three labels' styles.
        let mut rest = report.clone();
        for (role, label) in [
            (Role::Failure, "error:"),
            (Role::Hint, "hint:"),
            (Role::Muted, "details:"),
        ] {
            rest = rest.replacen(&style::paint(role, label), label, 1);
        }
        assert_eq!(rest, plain);
    }

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

use crate::{Error, Result, cli, config};
use clap::ArgMatches;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{IsTerminal, Write},
};

pub struct Output {
    pub format: String,
    pub show_secrets: bool,
    file: Option<File>,
}
impl Output {
    pub fn new(m: &ArgMatches) -> Result<Self> {
        let format = if cli::enabled(m, "json") {
            "json".into()
        } else {
            cli::value(m, "output").unwrap_or_else(|| {
                if std::io::stdout().is_terminal() {
                    "table"
                } else {
                    "json"
                }
                .into()
            })
        };
        let file = cli::value(m, "output-file")
            .map(|p| config::new_private(std::path::Path::new(&p)))
            .transpose()?;
        Ok(Self {
            format,
            show_secrets: cli::enabled(m, "show-secrets"),
            file,
        })
    }
    pub fn secure_destination(&self) -> bool {
        self.show_secrets || self.file.is_some()
    }
    pub fn write(&mut self, mut value: Value, secrets: &[&str]) -> Result<()> {
        if let Some(file) = &mut self.file {
            serde_json::to_writer(&mut *file, &value)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            return Ok(());
        }
        if !self.show_secrets {
            redact(&mut value, secrets);
        }
        let mut stdout = std::io::stdout().lock();
        if self.format == "table" {
            human(&mut stdout, &value)?;
        } else {
            serde_json::to_writer(&mut stdout, &value)?;
            stdout.write_all(b"\n")?;
        }
        stdout.flush()?;
        Ok(())
    }
}
pub fn envelope(status: Option<u16>, data: Value, id: Option<String>, outcome: &str) -> Value {
    json!({"http_status":status,"data":data,"resource_id":id,"outcome":outcome})
}
pub fn redact(value: &mut Value, secrets: &[&str]) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if [
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
                ]
                .contains(&key.to_ascii_lowercase().as_str())
                {
                    *value = json!("[REDACTED]");
                } else {
                    redact(value, secrets);
                }
            }
        }
        Value::Array(values) => {
            for v in values {
                redact(v, secrets);
            }
        }
        Value::String(s) => {
            for secret in secrets.iter().filter(|s| !s.is_empty()) {
                *s = s.replace(secret, "[REDACTED]");
            }
        }
        _ => {}
    }
}
fn human(out: &mut impl Write, value: &Value) -> Result<()> {
    if let Some(outcome) = value["outcome"].as_str() {
        writeln!(
            out,
            "{}{}",
            outcome,
            value["resource_id"]
                .as_str()
                .map(|id| format!("  {id}"))
                .unwrap_or_default()
        )?;
    }
    let data = value.get("data").unwrap_or(value);
    let items = data
        .as_array()
        .or_else(|| data.get("items").and_then(Value::as_array));
    if let Some(items) = items {
        if items.is_empty() {
            writeln!(out, "No results.")?;
            return Ok(());
        }
        let fields = ["id", "name", "status", "currency", "environment_id"];
        let columns: Vec<_> = fields
            .iter()
            .filter(|k| items.iter().any(|v| v.get(**k).is_some()))
            .collect();
        if !columns.is_empty() {
            writeln!(
                out,
                "{}",
                columns
                    .iter()
                    .map(|c| c.to_uppercase())
                    .collect::<Vec<_>>()
                    .join("  ")
            )?;
            for item in items {
                writeln!(
                    out,
                    "{}",
                    columns
                        .iter()
                        .map(|c| display(&item[**c]))
                        .collect::<Vec<_>>()
                        .join("  ")
                )?;
            }
        } else {
            writeln!(out, "{}", serde_json::to_string_pretty(data)?)?;
        }
    } else if !data.is_null() {
        writeln!(out, "{}", serde_json::to_string_pretty(data)?)?;
    }
    if let Some(cursor) = data.get("next_cursor").and_then(Value::as_str) {
        writeln!(out, "Next cursor: {cursor}")?;
    }
    Ok(())
}
fn display(v: &Value) -> String {
    v.as_str()
        .map(String::from)
        .unwrap_or_else(|| v.to_string())
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
pub fn report_error(error: &Error, json_output: bool) {
    let mut body =
        json!({"error":{"message":error.message,"exit_code":error.code,"detail":error.detail}});
    redact(&mut body, &[]);
    if json_output {
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
        let mut v = json!({"data":{"shared_secret":"abc","description":"token abc"}});
        redact(&mut v, &["abc"]);
        assert!(!v.to_string().contains("abc"));
    }
}

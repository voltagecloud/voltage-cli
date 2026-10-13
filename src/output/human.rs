//! Table-format rendering of result envelopes for people.
//!
//! Lists become aligned tables whose columns are the preferred fields the items carry;
//! single resources become aligned `field  value` lines. Every API string is scrubbed of
//! terminal control and bidirectional formatting characters. A table wider than the terminal
//! first hides trailing columns it cannot fit, then truncates text cells, and says what it
//! hid; identifiers, timestamps, and amounts are never truncated. comfy-table measures and
//! truncates table cells by display width, so wide glyphs line up in tables.

use super::{Envelope, Layout, Outcome};
use crate::{Result, terminal::scrub_line};
use comfy_table::{ColumnConstraint, ContentArrangement, Row, Table, TableStyle, Width};
use serde_json::{Map, Value};
use std::io::Write;

/// Preferred list columns, in display order.
const COLUMNS: [&str; 12] = [
    "id",
    "name",
    "type",
    "direction",
    "payment_kind",
    "status",
    "amount",
    "currency",
    "network",
    "environment_id",
    "created_at",
    "updated_at",
];
/// A table shows at most this many columns; the rest are named as hidden.
const MAX_COLUMNS: usize = 7;
/// Text cells shrink to no fewer than this many columns.
const MIN_CELL: u16 = 10;
const GAP: &str = "  ";
const EMPTY_CELL: &str = "-";

pub fn render(out: &mut impl Write, envelope: &Envelope, width: Option<usize>) -> Result<()> {
    let data = &envelope.data;
    // A plain success with data says nothing the data does not; other outcomes, such as
    // `accepted` or `dry_run`, and results without a body still need the line.
    let routine = matches!(envelope.outcome, Outcome::Succeeded | Outcome::Retrieved);
    if !routine || data.is_null() {
        let resource = envelope
            .resource_id
            .map(|id| format!("  {id}"))
            .unwrap_or_default();
        writeln!(out, "{}{resource}", envelope.outcome.as_str())?;
    }
    if let Some(items) = list_items(data) {
        table(out, items, width)?;
        summary(out, data, items.len())?;
        return Ok(());
    }
    let local = envelope.layout == Layout::Settings;
    match data {
        Value::Null => {}
        Value::Object(map) => fields(out, map, local)?,
        other => writeln!(out, "{}", cell(other))?,
    }
    Ok(())
}

/// A bare array, or a page's `items` or `entries`.
fn list_items(data: &Value) -> Option<&[Value]> {
    data.as_array()
        .or_else(|| data.get("items").and_then(Value::as_array))
        .or_else(|| data.get("entries").and_then(Value::as_array))
        .map(Vec::as_slice)
}

fn summary(out: &mut impl Write, data: &Value, count: usize) -> Result<()> {
    match count {
        0 => writeln!(out, "No results.")?,
        1 => writeln!(out, "1 result.")?,
        count => writeln!(out, "{count} results.")?,
    }
    if let Some(cursor) = data.get("next_cursor").and_then(Value::as_str) {
        writeln!(out, "Next cursor: {}", scrub_line(cursor))?;
    }
    Ok(())
}

/// The preferred columns the items carry, or else the first item's scalar fields.
fn columns(items: &[Value]) -> Vec<String> {
    let preferred: Vec<String> = COLUMNS
        .iter()
        .filter(|column| items.iter().any(|item| item.get(**column).is_some()))
        .map(|column| (*column).to_owned())
        .collect();
    if preferred.is_empty() {
        items
            .first()
            .and_then(Value::as_object)
            .map(|first| {
                first
                    .iter()
                    .filter(|(_, value)| !value.is_object() && !value.is_array())
                    .map(|(key, _)| key.clone())
                    .collect()
            })
            .unwrap_or_default()
    } else {
        preferred
    }
}

/// Identifiers, timestamps, and amounts are hidden rather than truncated.
fn truncatable(column: &str) -> bool {
    column != "id" && column != "amount" && !column.ends_with("_id") && !column.ends_with("_at")
}

fn table(out: &mut impl Write, items: &[Value], width: Option<usize>) -> Result<()> {
    let columns = columns(items);
    // Bare values, such as a list of IDs, have no column to judge, so none is shortened.
    if columns.is_empty() {
        for item in items {
            writeln!(out, "{}", cell(item))?;
        }
        return Ok(());
    }
    // The column cap applies before width, and the notice names what either one hid.
    let capped = &columns[..columns.len().min(MAX_COLUMNS)];
    let mut table = Table::new();
    table
        .load_style(TableStyle::new())
        .set_truncation_indicator("…")
        .set_header(row(capped
            .iter()
            .map(|column| scrub_line(&column.replace('_', " ").to_uppercase()))));
    for item in items {
        table.add_row(row(capped.iter().map(|column| {
            item.get(column).map_or_else(|| EMPTY_CELL.into(), cell)
        })));
    }
    let widths = table.column_max_content_widths();
    let narrowest: Vec<u16> = capped
        .iter()
        .zip(&widths)
        .map(|(column, width)| {
            if truncatable(column) {
                (*width).min(MIN_CELL)
            } else {
                *width
            }
        })
        .collect();
    let shown = match width {
        Some(limit) => {
            table
                .set_width(u16::try_from(limit).unwrap_or(u16::MAX))
                .set_content_arrangement(ContentArrangement::Dynamic);
            fitting(&narrowest, limit)
        }
        None => capped.len(),
    };
    // Constraint widths include the gap, which pads every shown column but the last. Scrubbed
    // text holds no NUL, so this delimiter makes truncation keep every grapheme that fits
    // rather than stop at a word boundary.
    for (index, column) in table.column_iter_mut().enumerate() {
        let gap = if index + 1 < shown {
            GAP.len() as u16
        } else {
            0
        };
        column.set_padding((0, gap)).set_delimiter('\0');
        column.set_constraint(if index >= shown {
            ColumnConstraint::Hidden
        } else if truncatable(&capped[index]) {
            ColumnConstraint::LowerBoundary(Width::Fixed(narrowest[index] + gap))
        } else {
            ColumnConstraint::Absolute(Width::Fixed(widths[index] + gap))
        });
    }
    for line in table.lines() {
        writeln!(out, "{}", line.trim_end())?;
    }
    if shown < columns.len() {
        // Without preferred fields, column names are the API's own keys.
        let hidden: Vec<String> = columns[shown..]
            .iter()
            .map(|column| scrub_line(column))
            .collect();
        let advice = if shown < capped.len() {
            "Use a wider terminal or --json for every field."
        } else {
            "Use --json for every field."
        };
        writeln!(out, "Hidden columns: {}. {advice}", hidden.join(", "))?;
    }
    Ok(())
}

/// One table line; a cell too wide for its column ends in an ellipsis rather than wrapping.
fn row(cells: impl Iterator<Item = String>) -> Row {
    let mut row = Row::from(cells.collect::<Vec<_>>());
    row.max_height(1);
    row
}

/// How many leading columns fit `limit` at their narrowest, with a gap between each; the
/// first is always shown.
fn fitting(narrowest: &[u16], limit: usize) -> usize {
    narrowest
        .iter()
        .scan(0, |used, width| {
            *used += usize::from(*width) + GAP.len();
            Some(*used)
        })
        .take_while(|used| *used <= limit.saturating_add(GAP.len()))
        .count()
        .max(1)
}

/// One value on one line: strings as themselves, amounts with their base unit, and other
/// structures as compact JSON.
fn cell(value: &Value) -> String {
    match value {
        Value::String(text) => timestamp(text).unwrap_or_else(|| scrub_line(text)),
        Value::Null => EMPTY_CELL.into(),
        other => amount(other).unwrap_or_else(|| scrub_line(&other.to_string())),
    }
}

/// An RFC 3339 UTC timestamp, `2024-02-27T01:58:51.693810000Z`, shown to the second as
/// `2024-02-27 01:58:51 UTC`. Anything else, including other offsets, is left as sent;
/// JSON output keeps the exact value.
fn timestamp(text: &str) -> Option<String> {
    let (whole, rest) = text.split_at_checked(19)?;
    let fraction = rest.strip_suffix('Z')?;
    let shaped = whole.bytes().enumerate().all(|(index, byte)| match index {
        4 | 7 => byte == b'-',
        10 => byte == b'T',
        13 | 16 => byte == b':',
        _ => byte.is_ascii_digit(),
    }) && (fraction.is_empty()
        || fraction
            .strip_prefix('.')
            .is_some_and(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit())));
    shaped.then(|| format!("{} {} UTC", &whole[..10], &whole[11..19]))
}

/// An `Amount` as the API sends it: `amount` and `currency`, with an optional `unit` and
/// `negative` sign. Without a `unit`, BTC amounts are msats and USD amounts are cents, so
/// `{"amount": 1000, "currency": "btc"}` is 1000 msats.
fn amount(value: &Value) -> Option<String> {
    let map = value.as_object()?;
    let amount = map.get("amount").filter(|amount| amount.is_number())?;
    let currency = map.get("currency")?.as_str()?;
    if map
        .keys()
        .any(|key| !matches!(key.as_str(), "amount" | "currency" | "unit" | "negative"))
    {
        return None;
    }
    let unit = match map.get("unit") {
        Some(unit) => unit.as_str()?,
        None => match currency {
            "btc" => "msats",
            "usd" => "cents",
            other => other,
        },
    };
    let sign = match map.get("negative") {
        Some(negative) if negative.as_bool()? => "-",
        Some(_) | None => "",
    };
    Some(scrub_line(&format!("{sign}{amount} {unit}")))
}

/// One field: its value's lines, and where a local setting's value came from.
struct Line {
    key: String,
    values: Vec<String>,
    source: Option<String>,
}

/// Nested objects flatten to dotted field names; amounts stay whole. A list of strings, such
/// as a dry run's notes, puts each item on its own line under the first. In a local result, a
/// `{value, source}` setting reads `name  value  (source)`.
fn fields(out: &mut impl Write, map: &Map<String, Value>, local: bool) -> Result<()> {
    let mut lines = Vec::new();
    flatten(String::new(), map, local, &mut lines);
    let width = lines
        .iter()
        .map(|line| line.key.chars().count())
        .max()
        .unwrap_or(0);
    for Line {
        key,
        values,
        source,
    } in lines
    {
        let source = source
            .map(|source| format!("{GAP}({source})"))
            .unwrap_or_default();
        for (index, value) in values.iter().enumerate() {
            let (key, source) = if index == 0 {
                (key.as_str(), source.as_str())
            } else {
                ("", "")
            };
            writeln!(out, "{key:<width$}{GAP}{value}{source}")?;
        }
    }
    Ok(())
}

fn flatten(prefix: String, map: &Map<String, Value>, local: bool, lines: &mut Vec<Line>) {
    for (key, value) in map {
        let key = scrub_line(&if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        });
        if local && let Some((value, source)) = value.as_object().and_then(setting) {
            lines.push(Line {
                key,
                values: value_lines(value),
                source: Some(scrub_line(source)),
            });
            continue;
        }
        match value {
            Value::Object(nested) if !nested.is_empty() && amount(value).is_none() => {
                flatten(key, nested, local, lines);
            }
            other => lines.push(Line {
                key,
                values: value_lines(other),
                source: None,
            }),
        }
    }
}

/// A local `{value, source}` setting.
fn setting(map: &Map<String, Value>) -> Option<(&Value, &str)> {
    if map.len() != 2 {
        return None;
    }
    Some((map.get("value")?, map.get("source")?.as_str()?))
}

/// A field's value as display lines: one per item of a list of strings, `-` for an empty
/// list, and otherwise the single `cell`.
fn value_lines(value: &Value) -> Vec<String> {
    let strings = value
        .as_array()
        .and_then(|items| items.iter().map(Value::as_str).collect::<Option<Vec<_>>>());
    match strings {
        Some(items) if items.is_empty() => vec![EMPTY_CELL.into()],
        Some(items) => items.into_iter().map(scrub_line).collect(),
        None => vec![cell(value)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Outcome;
    use serde_json::json;

    const ID: &str = "11111111-1111-4111-8111-111111111111";

    fn rendered(data: Value, width: Option<usize>) -> String {
        let envelope = Envelope::new(Some(200), data, None, Outcome::Retrieved);
        let mut out = Vec::new();
        render(&mut out, &envelope, width).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn lists_use_preferred_columns_aligned_with_readable_labels_and_amounts() {
        let text = rendered(
            json!({"items": [
                {"id": "a", "status": "completed", "amount": {"amount": 1000, "currency": "btc"}, "extra": 1},
                {"id": "bb", "name": "treasury", "amount": {"amount": 250, "currency": "usd"}}
            ], "next_cursor": "c1"}),
            None,
        );
        assert_eq!(
            text,
            "ID  NAME      STATUS     AMOUNT\n\
             a   -         completed  1000 msats\n\
             bb  treasury  -          250 cents\n\
             2 results.\n\
             Next cursor: c1\n"
        );
    }

    #[test]
    fn response_amounts_use_their_unit_and_sign() {
        let text = rendered(
            json!({
                "received": {"amount": 1000, "currency": "btc", "unit": "msats"},
                "spent": {"amount": 250, "currency": "usd", "unit": "cents", "negative": true},
                "held": {"amount": 5, "currency": "btc", "unit": "msats", "negative": false},
                "other": {"amount": 1, "currency": "btc", "note": "x"}
            }),
            None,
        );
        // An object with fields beyond an amount's is not an amount.
        assert_eq!(
            text,
            "held            5 msats\n\
             other.amount    1\n\
             other.currency  btc\n\
             other.note      x\n\
             received        1000 msats\n\
             spent           -250 cents\n"
        );
    }

    #[test]
    fn narrow_tables_hide_trailing_columns_then_truncate_text_but_never_ids() {
        let item = json!({
            "id": ID, "name": "a very long wallet name indeed",
            "status": "completed", "created_at": "2026-09-29T10:00:00Z"
        });
        let text = rendered(json!([item]), Some(60));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("{:<36}  {:<10}  STATUS", "ID", "NAME"));
        assert_eq!(lines[1], format!("{ID}  a very lo…  completed"));
        assert_eq!(
            lines[2],
            "Hidden columns: created_at. Use a wider terminal or --json for every field."
        );
        let wide = rendered(json!([item]), None);
        assert!(wide.contains("a very long wallet name indeed"));
        assert!(wide.contains("2026-09-29 10:00:00 UTC"));
        let tiny = rendered(json!([item]), Some(10));
        assert!(tiny.lines().nth(1).unwrap().starts_with(ID));
    }

    #[test]
    fn wide_glyphs_align_and_truncate_by_display_width() {
        let text = rendered(
            json!([{"id": "a", "name": "日本語の長い名前", "status": "x"}, {"id": "b", "name": "ok", "status": "y"}]),
            Some(22),
        );
        assert_eq!(
            text,
            "ID  NAME        STATUS\n\
             a   日本語の…   x\n\
             b   ok          y\n\
             2 results.\n"
        );
    }

    #[test]
    fn columns_beyond_the_cap_are_named_as_hidden() {
        let item = json!({
            "id": "p", "type": "bolt11", "direction": "send", "payment_kind": "lightning",
            "status": "completed", "amount": {"amount": 1, "currency": "btc"},
            "currency": "btc", "network": "mainnet", "environment_id": "e",
            "created_at": "2026-09-29T10:00:00Z", "updated_at": "2026-09-29T10:01:00Z"
        });
        let text = rendered(json!([item]), None);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>().join(" "),
            "ID TYPE DIRECTION PAYMENT KIND STATUS AMOUNT CURRENCY"
        );
        assert_eq!(
            lines[2],
            "Hidden columns: network, environment_id, created_at, updated_at. Use --json for every field."
        );
    }

    #[test]
    fn untrusted_text_is_scrubbed_in_cells_fields_keys_and_cursors() {
        let hostile = "x\u{1b}]52;c;AAAA\u{7}\u{202e}y\nz";
        let list = rendered(
            json!({"items": [{"id": hostile}], "next_cursor": hostile}),
            None,
        );
        let single = rendered(json!({hostile: {"name": hostile}}), None);
        let bare = rendered(json!([hostile]), None);
        let scalar = rendered(json!(hostile), None);
        // Too narrow for the hostile key's column, so it is named in the hidden notice.
        let hidden = rendered(json!([{"a": "1", hostile: "2"}]), Some(3));
        assert!(hidden.contains("Hidden columns:"), "{hidden:?}");
        for text in [list, single, bare, scalar, hidden] {
            assert!(
                !text
                    .chars()
                    .any(|c| (c.is_control() && c != '\n') || c == '\u{202e}'),
                "{text:?}"
            );
            assert!(!text.contains("y\nz"), "{text:?}");
        }
    }

    #[test]
    fn single_resources_render_as_flattened_fields() {
        let text = rendered(
            json!({"id": "w", "balance": {"available": {"amount": 5, "currency": "usd"}, "held": null}, "tags": ["a", "b"], "empty": {}}),
            None,
        );
        assert_eq!(
            text,
            "balance.available  5 cents\n\
             balance.held       -\n\
             empty              {}\n\
             id                 w\n\
             tags               a\n\
             \x20                  b\n"
        );
    }

    #[test]
    fn local_settings_render_as_value_and_source() {
        let envelope = Envelope::settings(json!({
                "organization_id": {"value": ID, "source": "flag"},
                "environment_ids": {"value": [ID, "e2"], "source": "profile"},
                "wallet_id": {"value": null, "source": "unset"},
                "credential": null,
                "config_file": {"path": "/c/config.toml", "exists": false},
                "ignored_variables": ["VOLTAGE_WALLET_ID"],
                "notes": []
        }))
        .unwrap();
        let mut out = Vec::new();
        render(&mut out, &envelope, None).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "config_file.exists  false\n\
                 config_file.path    /c/config.toml\n\
                 credential          -\n\
                 environment_ids     {ID}  (profile)\n\
                 \x20                   e2\n\
                 ignored_variables   VOLTAGE_WALLET_ID\n\
                 notes               -\n\
                 organization_id     {ID}  (flag)\n\
                 wallet_id           -  (unset)\n"
            )
        );
        // API data, or local data from a remote service such as a price, that happens to
        // have the same shape keeps the generic layout.
        assert_eq!(
            rendered(json!({"x": {"value": 1, "source": "s"}}), None),
            "x.source  s\nx.value   1\n"
        );
        let price = Envelope::local(json!({"x": {"value": 1, "source": "s"}})).unwrap();
        let mut out = Vec::new();
        render(&mut out, &price, None).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "x.source  s\nx.value   1\n"
        );
    }

    #[test]
    fn the_outcome_line_appears_only_when_it_adds_something() {
        let render_with = |outcome, data: Value| {
            let envelope = Envelope::new(Some(202), data, Some(ID.parse().unwrap()), outcome);
            let mut out = Vec::new();
            render(&mut out, &envelope, None).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert_eq!(
            render_with(Outcome::Accepted, json!({"status": "pending"})),
            format!("accepted  {ID}\nstatus  pending\n")
        );
        assert_eq!(
            render_with(Outcome::Succeeded, Value::Null),
            format!("succeeded  {ID}\n")
        );
        assert_eq!(
            render_with(Outcome::Retrieved, json!({"status": "pending"})),
            "status  pending\n"
        );
    }

    #[test]
    fn utc_timestamps_are_shown_to_the_second_and_anything_else_is_left_alone() {
        assert_eq!(
            timestamp("2024-02-27T01:58:51.693810000Z").as_deref(),
            Some("2024-02-27 01:58:51 UTC")
        );
        assert_eq!(
            timestamp("2026-09-29T10:00:00Z").as_deref(),
            Some("2026-09-29 10:00:00 UTC")
        );
        for other in [
            "2026-09-29T10:00:00+02:00",
            "2026-09-29T10:00:00.Z",
            "2026-09-29 10:00:00Z",
            "2026-9-29T10:00:00Z",
            "not a timestamp at all, Z",
            "₿₿₿₿₿₿₿₿₿₿₿₿₿₿₿₿₿₿₿₿Z",
        ] {
            assert_eq!(timestamp(other), None, "{other}");
        }
    }

    #[test]
    fn empty_lists_and_fieldless_items_have_summaries_and_fallback_columns() {
        assert_eq!(rendered(json!([]), None), "No results.\n");
        assert_eq!(
            rendered(json!({"entries": [{"delta": 5, "nested": {}}]}), None),
            "DELTA\n5\n1 result.\n"
        );
        assert_eq!(
            rendered(json!([ID]), Some(12)),
            format!("{ID}\n1 result.\n")
        );
    }
}

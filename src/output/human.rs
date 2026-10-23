//! Table-format rendering of result envelopes for people.
//!
//! Lists become aligned tables whose columns are the preferred fields the items carry;
//! single resources become aligned `field  value` lines. Every API string is scrubbed of
//! terminal control and bidirectional formatting characters. A table wider than the terminal
//! first hides trailing columns it cannot fit, then truncates text cells, and says what it
//! hid; identifiers, timestamps, and amounts are never truncated. comfy-table measures and
//! truncates table cells by display width, so wide glyphs line up in tables.

use super::{Envelope, Layout, Outcome};
use crate::{
    Result,
    style::{Depth, Glyphs, Line as Styled, Role},
    terminal::scrub_line,
};
use comfy_table::{ColumnConstraint, ContentArrangement, Row, Table, TableStyle, Width};
use serde_json::{Map, Value};
use std::{
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};
use unicode_width::UnicodeWidthChar;

/// Preferred list columns, in display order.
const COLUMNS: [&str; 13] = [
    "id",
    "name",
    "organization_name",
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

/// What a rendering may assume beyond the data: the terminal's width, the current time for
/// relative timestamps, and whether to paint headers, field names, and statuses. Styles
/// are added after layout, so they never change a width.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub width: Option<usize>,
    /// Seconds since the Unix epoch.
    pub now: i64,
    pub color: bool,
    /// Draw a titled result as a card; only for a terminal.
    pub cards: bool,
    /// Unicode or ASCII box lines for a card.
    pub glyphs: Glyphs,
    /// The depth of the brand color for a card's title.
    pub depth: Depth,
}

impl View {
    /// No width limit and no styles, at the current time.
    pub fn plain() -> Self {
        Self {
            width: None,
            now: unix_seconds(SystemTime::now()),
            color: false,
            cards: false,
            glyphs: Glyphs::ASCII,
            depth: Depth::Basic,
        }
    }
}

/// Seconds since the epoch; a clock set before 1970 reads as the epoch itself.
fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
    })
}

pub fn render(out: &mut impl Write, envelope: &Envelope, view: View) -> Result<()> {
    let data = &envelope.data;
    // A plain success with data says nothing the data does not; other outcomes, such as
    // `accepted` or `dry_run`, and results without a body still need the line.
    let routine = matches!(envelope.outcome, Outcome::Succeeded | Outcome::Retrieved);
    if !routine || data.is_null() {
        let resource = envelope
            .resource_id
            .map(|id| format!("  {id}"))
            .unwrap_or_default();
        let outcome = envelope.outcome.as_str();
        let outcome = match outcome_role(envelope.outcome).filter(|_| view.color) {
            Some(role) => Styled::default().paint(role, outcome).as_str().to_owned(),
            None => outcome.to_owned(),
        };
        writeln!(out, "{outcome}{resource}")?;
    }
    if let Some(items) = list_items(data) {
        table(out, items, view)?;
        summary(out, data, items.len())?;
        return Ok(());
    }
    match data {
        Value::Null => {}
        Value::Object(map) => object(out, envelope, map, view)?,
        other => writeln!(out, "{}", cell(other, view, Place::Field))?,
    }
    Ok(())
}

/// A single result's fields, inside a card when the result has a title and the card fits.
fn object(
    out: &mut impl Write,
    envelope: &Envelope,
    map: &Map<String, Value>,
    view: View,
) -> Result<()> {
    let local = envelope.layout == Layout::Settings;
    let Some(title) = envelope.card.filter(|_| view.cards) else {
        return fields(out, map, local, view);
    };
    let mut lines = Vec::new();
    fields(&mut lines, map, local, view)?;
    let lines: Vec<String> = String::from_utf8_lossy(&lines)
        .lines()
        .map(str::to_owned)
        .collect();
    match card(title, &lines, view) {
        Some(card) => {
            for line in card {
                writeln!(out, "{line}")?;
            }
            Ok(())
        }
        None => fields(out, map, local, view),
    }
}

/// Box lines for a card: rounded corners where the terminal draws Unicode.
struct Frame {
    top_left: char,
    top_right: char,
    bottom_left: char,
    bottom_right: char,
    across: char,
    down: char,
}

impl Frame {
    fn for_glyphs(glyphs: Glyphs) -> Self {
        if glyphs == Glyphs::UNICODE {
            Self {
                top_left: '╭',
                top_right: '╮',
                bottom_left: '╰',
                bottom_right: '╯',
                across: '─',
                down: '│',
            }
        } else {
            Self {
                top_left: '+',
                top_right: '+',
                bottom_left: '+',
                bottom_right: '+',
                across: '-',
                down: '|',
            }
        }
    }
}

/// `lines` inside a box titled `title`, or `None` when the box would not fit the terminal.
/// Widths are measured on the text without styles, so styled lines line up.
fn card(title: &'static str, lines: &[String], view: View) -> Option<Vec<String>> {
    let widths: Vec<usize> = lines
        .iter()
        .map(|line| display_width(&anstream::adapter::strip_str(line).to_string()))
        .collect();
    // The title sits in the top line as `╭─ title ─╮`, so the inside fits it with a dash each side.
    let inner = widths
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
        .max(title.len() + 2);
    if view.width.is_some_and(|width| inner + 4 > width) {
        return None;
    }
    let frame = Frame::for_glyphs(view.glyphs);
    let border = |text: String| {
        if view.color {
            Styled::default()
                .styled(Role::Muted, &text)
                .as_str()
                .to_owned()
        } else {
            text
        }
    };
    let title_text = if view.color {
        Styled::default()
            .brand(view.depth, title)
            .as_str()
            .to_owned()
    } else {
        title.to_owned()
    };
    let across = |count: usize| frame.across.to_string().repeat(count);
    let mut card = vec![format!(
        "{}{title_text}{}",
        border(format!("{}{} ", frame.top_left, frame.across)),
        border(format!(
            " {}{}",
            across(inner - title.len() - 1),
            frame.top_right
        ))
    )];
    for (line, width) in lines.iter().zip(widths) {
        card.push(format!(
            "{} {line}{} {}",
            border(frame.down.to_string()),
            " ".repeat(inner - width),
            border(frame.down.to_string())
        ));
    }
    card.push(border(format!(
        "{}{}{}",
        frame.bottom_left,
        across(inner + 2),
        frame.bottom_right
    )));
    Some(card)
}

/// An error's detail as aligned `field  value` lines, or one line when it is not an object.
pub fn detail(out: &mut impl Write, detail: &Value) -> Result<()> {
    let view = View::plain();
    match detail {
        Value::Object(map) => fields(out, map, false, view),
        other => Ok(writeln!(out, "{}", cell(other, view, Place::Field))?),
    }
}

/// An outcome worth a color: what finished, what is still in flight, and what was not sent.
fn outcome_role(outcome: Outcome) -> Option<Role> {
    match outcome {
        Outcome::Completed | Outcome::Ready => Some(Role::Success),
        Outcome::Accepted => Some(Role::Pending),
        Outcome::DryRun => Some(Role::Caution),
        Outcome::Succeeded | Outcome::Retrieved | Outcome::Event => None,
    }
}

/// A status cell's color, for the words whose meaning is clear across resources. Any
/// other status, including one the API adds later, stays plain.
fn status_role(status: &str) -> Option<Role> {
    match status {
        "completed" | "succeeded" | "ready" | "active" | "paid" => Some(Role::Success),
        "failed" | "expired" | "error" | "rejected" => Some(Role::Failure),
        "sending" | "receiving" | "generating" | "approved" | "pending" | "processing" => {
            Some(Role::Pending)
        }
        _ => None,
    }
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

fn table(out: &mut impl Write, items: &[Value], view: View) -> Result<()> {
    let width = view.width;
    let columns = columns(items);
    // Bare values, such as a list of IDs, have no column to judge, so none is shortened.
    if columns.is_empty() {
        for item in items {
            writeln!(out, "{}", cell(item, view, Place::Table))?;
        }
        return Ok(());
    }
    // The column cap applies before width, and the notice names what either one hid.
    let capped = &columns[..columns.len().min(MAX_COLUMNS)];
    let labels: Vec<String> = capped
        .iter()
        .map(|column| scrub_line(&column.replace('_', " ").to_uppercase()))
        .collect();
    let mut table = Table::new();
    table
        .load_style(TableStyle::new())
        .set_truncation_indicator("…")
        .set_header(row(labels.iter().cloned()));
    for item in items {
        table.add_row(row(capped.iter().map(|column| {
            item.get(column).map_or_else(
                || EMPTY_CELL.into(),
                |value| cell(value, view, Place::Table),
            )
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
    let mut lines: Vec<String> = table
        .lines()
        .map(|line| line.trim_end().to_owned())
        .collect();
    if view.color {
        let status = capped[..shown].iter().position(|column| column == "status");
        paint_table(&mut lines, &labels[..shown], status);
    }
    for line in lines {
        writeln!(out, "{line}")?;
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

/// Bold the header and color each known status word. The header labels locate the status
/// column by display width; when a label before it was shortened, nothing is colored.
fn paint_table(lines: &mut [String], labels: &[String], status: Option<usize>) {
    let Some((header, rows)) = lines.split_first_mut() else {
        return;
    };
    let start = status.and_then(|status| column_start(header, &labels[..=status]));
    *header = Styled::default()
        .styled(Role::Emphasis, header)
        .as_str()
        .to_owned();
    let Some(start) = start else {
        return;
    };
    for line in rows {
        let Some(at) = char_at_column(line, start) else {
            continue;
        };
        let word: String = line[at..].chars().take_while(|c| *c != ' ').collect();
        if let Some(role) = status_role(&word) {
            let rest = &line[at + word.len()..];
            *line = format!(
                "{}{}{rest}",
                &line[..at],
                Styled::default().styled(role, &word).as_str()
            );
        }
    }
}

/// The display column where the last of `labels` starts in `header`, found by matching each
/// label in turn; `None` when one does not appear whole.
fn column_start(header: &str, labels: &[String]) -> Option<usize> {
    let mut from = 0;
    let mut start = 0;
    for label in labels {
        start = from + header[from..].find(label.as_str())?;
        from = start + label.len();
    }
    Some(display_width(&header[..start]))
}

/// The byte index where display column `column` starts in `line`, if a character starts there.
fn char_at_column(line: &str, column: usize) -> Option<usize> {
    let mut width = 0;
    for (index, c) in line.char_indices() {
        if width == column {
            return Some(index);
        }
        width += c.width().unwrap_or(0);
    }
    None
}

fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
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

/// Where a value is shown: a table cell has room for a relative time, and a field line
/// for the exact time beside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Place {
    Table,
    Field,
}

/// One value on one line: strings as themselves, timestamps and amounts in readable form,
/// and other structures as compact JSON.
fn cell(value: &Value, view: View, place: Place) -> String {
    match value {
        Value::String(text) => when(text, view.now, place).unwrap_or_else(|| scrub_line(text)),
        Value::Null => EMPTY_CELL.into(),
        other => amount(other).unwrap_or_else(|| scrub_line(&other.to_string())),
    }
}

/// An RFC 3339 UTC timestamp as a table's `3 minutes ago`, or a field's
/// `2024-02-27 01:58:51 UTC (7 months ago)`.
fn when(text: &str, now: i64, place: Place) -> Option<String> {
    let (shown, seconds) = timestamp(text)?;
    let relative = relative(seconds, now);
    Some(match place {
        Place::Table => relative,
        Place::Field => format!("{shown} ({relative})"),
    })
}

/// An RFC 3339 UTC timestamp, `2024-02-27T01:58:51.693810000Z`, shown to the second as
/// `2024-02-27 01:58:51 UTC`, with its seconds since the epoch. Anything else, including
/// other offsets, is left as sent; JSON output keeps the exact value.
fn timestamp(text: &str) -> Option<(String, i64)> {
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
    if !shaped {
        return None;
    }
    let number = |range: std::ops::Range<usize>| whole[range].parse::<i64>().ok();
    let days = days_from_civil(number(0..4)?, number(5..7)?, number(8..10)?);
    let seconds = days * 86_400 + number(11..13)? * 3_600 + number(14..16)? * 60 + number(17..19)?;
    Some((format!("{} {} UTC", &whole[..10], &whole[11..19]), seconds))
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `then` against `now` in the largest whole unit: `3 minutes ago`, `in 2 days`.
fn relative(then: i64, now: i64) -> String {
    let delta = now.saturating_sub(then);
    let seconds = delta.unsigned_abs();
    // Within a minute either way, so a clock a little behind the server's reads the same.
    if seconds < 60 {
        return "just now".into();
    }
    let (count, unit) = match seconds {
        0..3_600 => (seconds / 60, "minute"),
        3_600..86_400 => (seconds / 3_600, "hour"),
        86_400..2_592_000 => (seconds / 86_400, "day"),
        2_592_000..31_536_000 => (seconds / 2_592_000, "month"),
        _ => (seconds / 31_536_000, "year"),
    };
    let plural = if count == 1 { "" } else { "s" };
    if delta >= 0 {
        format!("{count} {unit}{plural} ago")
    } else {
        format!("in {count} {unit}{plural}")
    }
}

/// An `Amount` as the API sends it: `amount` and `currency`, with an optional `unit` and
/// `negative` sign. Without a `unit`, BTC amounts are msats and USD amounts are cents, so
/// `{"amount": 1000, "currency": "btc"}` is 1000 msats, shown as `1 sat`.
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
    Some(scrub_line(&format!(
        "{sign}{}",
        friendly(&amount.to_string(), unit)
    )))
}

/// The readable form of a whole number of `unit`: every BTC amount in sats, with up to three
/// decimals for msats, so one column never mixes units; cents as dollars; anything else
/// with its digits grouped. A number that is not a plain integer is left as sent.
fn friendly(amount: &str, unit: &str) -> String {
    if amount.is_empty() || !amount.bytes().all(|byte| byte.is_ascii_digit()) {
        return format!("{amount} {unit}");
    }
    match unit {
        "msats" => {
            let padded = format!("{amount:0>4}");
            let (sats, msats) = padded.split_at(padded.len() - 3);
            match msats.trim_end_matches('0') {
                "" => counted(sats, "sat"),
                fraction => format!("{}.{fraction} sats", grouped(sats)),
            }
        }
        "sats" => counted(amount, "sat"),
        "cents" => {
            let padded = format!("{amount:0>3}");
            let (dollars, cents) = padded.split_at(padded.len() - 2);
            format!("${}.{cents}", grouped(dollars))
        }
        unit => format!("{} {unit}", grouped(amount)),
    }
}

/// `1 sat`, `1,000 sats`.
fn counted(amount: &str, unit: &str) -> String {
    let plural = if amount == "1" { "" } else { "s" };
    format!("{} {unit}{plural}", grouped(amount))
}

/// Digits grouped in threes: `1234567` reads `1,234,567`.
fn grouped(digits: &str) -> String {
    let digits = digits.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// One field: its value's lines, and where a local setting's value came from.
struct Line {
    key: String,
    values: Vec<String>,
    source: Option<String>,
}

/// Nested objects flatten to dotted field names; amounts stay whole. A list of strings, such
/// as a described change's notes, puts each item on its own line under the first. In a local result, a
/// `{value, source}` setting reads `name  value  (source)`.
fn fields(out: &mut impl Write, map: &Map<String, Value>, local: bool, view: View) -> Result<()> {
    let mut lines = Vec::new();
    flatten(String::new(), map, local, view, &mut lines);
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
            .map(|source| {
                let source = format!("({source})");
                if view.color {
                    let dim = Styled::default().styled(Role::Muted, &source);
                    format!("{GAP}{}", dim.as_str())
                } else {
                    format!("{GAP}{source}")
                }
            })
            .unwrap_or_default();
        for (index, value) in values.iter().enumerate() {
            let (key, source) = if index == 0 {
                (key.as_str(), source.as_str())
            } else {
                ("", "")
            };
            let pad = " ".repeat(width - key.chars().count());
            let key = if view.color && !key.is_empty() {
                Styled::default()
                    .styled(Role::Emphasis, key)
                    .as_str()
                    .to_owned()
            } else {
                key.to_owned()
            };
            writeln!(out, "{key}{pad}{GAP}{value}{source}")?;
        }
    }
    Ok(())
}

fn flatten(
    prefix: String,
    map: &Map<String, Value>,
    local: bool,
    view: View,
    lines: &mut Vec<Line>,
) {
    for (key, value) in map {
        let key = scrub_line(&if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        });
        if local && let Some((value, source)) = value.as_object().and_then(setting) {
            lines.push(Line {
                key,
                values: value_lines(value, view),
                source: Some(scrub_line(source)),
            });
            continue;
        }
        match value {
            Value::Object(nested) if !nested.is_empty() && amount(value).is_none() => {
                flatten(key, nested, local, view, lines);
            }
            other => lines.push(Line {
                key,
                values: value_lines(other, view),
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
fn value_lines(value: &Value, view: View) -> Vec<String> {
    let strings = value
        .as_array()
        .and_then(|items| items.iter().map(Value::as_str).collect::<Option<Vec<_>>>());
    match strings {
        Some(items) if items.is_empty() => vec![EMPTY_CELL.into()],
        Some(items) => items.into_iter().map(scrub_line).collect(),
        None => vec![cell(value, view, Place::Field)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{output::Outcome, style::paint};
    use serde_json::json;
    use std::time::Duration;

    const ID: &str = "11111111-1111-4111-8111-111111111111";

    /// 2024-03-01 00:00:00 UTC, so relative times in expectations stay fixed.
    const NOW: i64 = 1_709_251_200;

    fn view(width: Option<usize>) -> View {
        View {
            width,
            now: NOW,
            ..View::plain()
        }
    }

    fn rendered(data: Value, width: Option<usize>) -> String {
        let envelope = Envelope::new(Some(200), data, None, Outcome::Retrieved);
        let mut out = Vec::new();
        render(&mut out, &envelope, view(width)).unwrap();
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
             a   -         completed  1 sat\n\
             bb  treasury  -          $2.50\n\
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
            "held            0.005 sats\n\
             other.amount    1\n\
             other.currency  btc\n\
             other.note      x\n\
             received        1 sat\n\
             spent           -$2.50\n"
        );
    }

    #[test]
    fn amounts_read_in_their_friendliest_exact_unit_with_grouped_digits() {
        for (amount, unit, shown) in [
            // Every BTC amount reads in sats, so msats become up to three decimals.
            ("0", "msats", "0 sats"),
            ("1", "msats", "0.001 sats"),
            ("999", "msats", "0.999 sats"),
            ("1000", "msats", "1 sat"),
            ("1500", "msats", "1.5 sats"),
            ("1234567", "msats", "1,234.567 sats"),
            ("150000", "msats", "150 sats"),
            ("150000000", "msats", "150,000 sats"),
            ("2100000000000000000", "msats", "2,100,000,000,000,000 sats"),
            ("1000", "sats", "1,000 sats"),
            ("0", "cents", "$0.00"),
            ("5", "cents", "$0.05"),
            ("123456", "cents", "$1,234.56"),
            ("1000", "usd", "1,000 usd"),
            // A number that is not a plain integer is left as sent.
            ("1.5", "msats", "1.5 msats"),
            ("1e3", "cents", "1e3 cents"),
        ] {
            assert_eq!(friendly(amount, unit), shown, "{amount} {unit}");
        }
        assert_eq!(grouped("000123"), "123");
        assert_eq!(grouped("0"), "0");
    }

    #[test]
    fn times_read_relative_to_now_in_the_largest_whole_unit() {
        for (offset, shown) in [
            (0, "just now"),
            (59, "just now"),
            (60, "1 minute ago"),
            (7_199, "1 hour ago"),
            (7_200, "2 hours ago"),
            // Each unit starts exactly at its boundary.
            (3_600, "1 hour ago"),
            (86_400, "1 day ago"),
            (2_592_000, "1 month ago"),
            (31_536_000, "1 year ago"),
            (86_400 * 3, "3 days ago"),
            (86_400 * 45, "1 month ago"),
            (86_400 * 800, "2 years ago"),
            (-30, "just now"),
            (-59, "just now"),
            (-86_400 * 2, "in 2 days"),
        ] {
            assert_eq!(relative(NOW - offset, NOW), shown, "{offset}");
        }
        let created = "2024-02-27T01:58:51Z";
        assert_eq!(when(created, NOW, Place::Table).unwrap(), "2 days ago");
        assert_eq!(
            when(created, NOW, Place::Field).unwrap(),
            "2024-02-27 01:58:51 UTC (2 days ago)"
        );
        assert_eq!(when("yesterday", NOW, Place::Field), None);
    }

    fn styled(envelope: &Envelope, width: Option<usize>) -> String {
        let mut out = Vec::new();
        let view = View {
            color: true,
            ..view(width)
        };
        render(&mut out, envelope, view).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn strip(text: &str) -> String {
        anstream::adapter::strip_str(text).to_string()
    }

    #[test]
    fn color_marks_headers_statuses_and_outcomes_without_moving_anything() {
        let items = json!([
            {"id": "日本", "status": "completed", "name": "x"},
            {"id": "b", "status": "failed", "name": "y"},
            {"id": "c", "status": "brand-new", "name": "z"}
        ]);
        let envelope = Envelope::new(Some(202), items.clone(), None, Outcome::Accepted);
        let text = styled(&envelope, None);
        let mut plain = Vec::new();
        render(&mut plain, &envelope, view(None)).unwrap();
        assert_eq!(strip(&text), String::from_utf8(plain).unwrap());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], paint(Role::Pending, "accepted"));
        assert!(lines[1].starts_with("\u{1b}[1m"), "{:?}", lines[1]);
        // A wide glyph before the status column does not shift where the color goes.
        assert!(lines[2].contains(&paint(Role::Success, "completed")));
        assert!(lines[3].contains(&paint(Role::Failure, "failed")));
        // A status the palette does not know stays plain.
        assert_eq!(lines[4], "c     z     brand-new");
    }

    #[test]
    fn painting_an_empty_or_short_table_changes_nothing_it_cannot_place() {
        // No lines: nothing to paint.
        let mut empty: Vec<String> = Vec::new();
        paint_table(&mut empty, &[], Some(0));
        assert!(empty.is_empty());
        // A row that ends before the status column is left as it is.
        let mut lines = vec!["ID  STATUS".to_owned(), "a".to_owned()];
        paint_table(&mut lines, &["ID".to_owned(), "STATUS".to_owned()], Some(1));
        assert_eq!(lines[1], "a");
    }

    #[test]
    fn a_clock_before_1970_reads_as_the_epoch() {
        assert_eq!(unix_seconds(UNIX_EPOCH - Duration::from_secs(5)), 0);
        assert_eq!(unix_seconds(UNIX_EPOCH + Duration::from_secs(5)), 5);
    }

    #[test]
    fn a_shortened_label_before_the_status_leaves_statuses_plain() {
        let header = "ORGANIZAT…  STATUS".to_owned();
        let labels = ["ORGANIZATION NAME".to_owned(), "STATUS".to_owned()];
        let mut lines = vec![header, "acme corp   failed".to_owned()];
        paint_table(&mut lines, &labels, Some(1));
        assert!(lines[0].starts_with("\u{1b}[1m"));
        assert_eq!(lines[1], "acme corp   failed");
        assert_eq!(char_at_column("ab", 5), None);
    }

    #[test]
    fn color_bolds_field_names_only() {
        let envelope = Envelope::new(
            Some(200),
            json!({"id": "w", "tags": ["a", "b"]}),
            None,
            Outcome::Retrieved,
        );
        let text = styled(&envelope, None);
        assert_eq!(strip(&text), "id    w\ntags  a\n      b\n");
        assert!(text.starts_with(&format!(
            "{}    w\n",
            Styled::default().styled(Role::Emphasis, "id").as_str()
        )));
    }

    fn carded(width: Option<usize>, color: bool, glyphs: Glyphs) -> String {
        let envelope = Envelope::new(
            None,
            json!({"account": "me", "kind": "user"}),
            None,
            Outcome::Succeeded,
        )
        .titled("voltage auth status");
        let view = View {
            cards: true,
            color,
            glyphs,
            ..view(width)
        };
        let mut out = Vec::new();
        render(&mut out, &envelope, view).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_titled_result_is_a_card_that_fits_its_title_and_widest_line() {
        // The title is wider than every field, so it sets the width.
        assert_eq!(
            carded(None, false, Glyphs::UNICODE),
            "╭─ voltage auth status ─╮\n\
             │ account  me           │\n\
             │ kind     user         │\n\
             ╰───────────────────────╯\n"
        );
        assert_eq!(
            carded(Some(25), false, Glyphs::ASCII),
            "+- voltage auth status -+\n\
             | account  me           |\n\
             | kind     user         |\n\
             +-----------------------+\n"
        );
        // Styles do not move the borders.
        let styled = carded(None, true, Glyphs::UNICODE);
        assert!(styled.contains("\u{1b}[2m"), "{styled:?}");
        assert_eq!(strip(&styled), carded(None, false, Glyphs::UNICODE));
    }

    #[test]
    fn a_card_too_wide_for_the_terminal_or_off_a_terminal_is_plain_fields() {
        let plain = "account  me\nkind     user\n";
        assert_eq!(carded(Some(24), false, Glyphs::UNICODE), plain);
        let envelope = Envelope::new(
            None,
            json!({"account": "me", "kind": "user"}),
            None,
            Outcome::Succeeded,
        )
        .titled("voltage auth status");
        let mut out = Vec::new();
        render(&mut out, &envelope, view(None)).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), plain);
    }

    #[test]
    fn color_dims_a_settings_source() {
        let envelope =
            Envelope::settings(json!({"account": {"value": "me", "source": "flag"}})).unwrap();
        let text = styled(&envelope, None);
        assert!(
            text.contains(&format!(
                "me  {}",
                Styled::default().styled(Role::Muted, "(flag)").as_str()
            )),
            "{text:?}"
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
        // A table shows a timestamp relative to now; JSON keeps the exact value.
        assert!(wide.contains("in 2 years"), "{wide}");
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
            json!({"id": "w", "balance": {"available": {"amount": 5, "currency": "usd"}, "held": null}, "tags": ["a", "b"], "none": [], "empty": {}}),
            None,
        );
        // Each list item gets its own line; an empty list reads `-`, not `[]`.
        assert_eq!(
            text,
            "balance.available  $0.05\n\
             balance.held       -\n\
             empty              {}\n\
             id                 w\n\
             none               -\n\
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
        render(&mut out, &envelope, view(None)).unwrap();
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
        render(&mut out, &price, view(None)).unwrap();
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
            render(&mut out, &envelope, view(None)).unwrap();
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
            timestamp("2024-02-27T01:58:51.693810000Z"),
            Some(("2024-02-27 01:58:51 UTC".into(), 1_708_999_131))
        );
        assert_eq!(
            timestamp("2026-09-29T10:00:00Z"),
            Some(("2026-09-29 10:00:00 UTC".into(), 1_790_676_000))
        );
        assert_eq!(
            timestamp("1970-01-01T00:00:00Z"),
            Some(("1970-01-01 00:00:00 UTC".into(), 0))
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

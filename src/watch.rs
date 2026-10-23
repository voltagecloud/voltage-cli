//! What changed between two reads of a payments list, for `payments list --watch`.

use crate::Error;
use serde_json::Value;
use std::{collections::HashMap, time::Duration};

/// The longest pause between refreshes after repeated failures.
pub const BACKOFF_CEILING: Duration = Duration::from_secs(60);

/// A watch compares pages of the most recently updated payments, so a change always lands on
/// the first page; a cursor or another order would hide it.
pub fn follow_updates(query: &mut Vec<(String, String)>) {
    query.retain(|(name, _)| !matches!(name.as_str(), "sort_key" | "sort_order" | "cursor"));
    query.extend(
        [("sort_key", "updated_at"), ("sort_order", "DESC")]
            .map(|(name, value)| (name.to_owned(), value.to_owned())),
    );
}

/// A refresh the server could not serve now: a transport failure, a 429, or any 5xx, with or
/// without a JSON body.
pub fn retries(error: &Error) -> bool {
    error.is_transport() || matches!(error.response_status(), Some(429 | 500..=599))
}

/// How a payment appears in a watch: in the first read, new since the last read, or
/// changed since then.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Change {
    Snapshot,
    New,
    Updated,
}

impl Change {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::New => "new",
            Self::Updated => "updated",
        }
    }
}

/// Each payment's last seen state, by ID.
#[derive(Debug, Default)]
pub struct Seen {
    states: HashMap<String, String>,
    started: bool,
}

impl Seen {
    /// The page's payments with how each changed since the last page, newest first as the
    /// API lists them. A payment without an ID cannot be followed and is left out.
    pub fn update(&mut self, page: &Value) -> Vec<(String, Value, Change)> {
        let first = !std::mem::replace(&mut self.started, true);
        let mut changes = Vec::new();
        for payment in items(page) {
            let Some(id) = payment.get("id").and_then(Value::as_str) else {
                continue;
            };
            let state = state(payment);
            let change = match self.states.insert(id.to_owned(), state.clone()) {
                _ if first => Change::Snapshot,
                None => Change::New,
                Some(previous) if previous != state => Change::Updated,
                Some(_) => continue,
            };
            changes.push((id.to_owned(), payment.clone(), change));
        }
        changes
    }
}

/// The page with at most `rows` payments, and how many it left out.
pub fn first(page: &Value, rows: Option<usize>) -> (Value, usize) {
    let all = items(page);
    let Some(rows) = rows.filter(|rows| *rows < all.len()) else {
        return (page.clone(), 0);
    };
    let kept = Value::Array(all[..rows].to_vec());
    let page = match page {
        Value::Object(map) => {
            let mut map = map.clone();
            map.insert("items".into(), kept);
            Value::Object(map)
        }
        _ => kept,
    };
    (page, all.len() - rows)
}

/// A bare array or a page's `items`.
pub fn items(page: &Value) -> &[Value] {
    page.as_array()
        .or_else(|| page.get("items").and_then(Value::as_array))
        .map_or(&[], Vec::as_slice)
}

/// What marks a payment as changed: its status and when it was last updated.
fn state(payment: &Value) -> String {
    let field = |name: &str| payment.get(name).map(Value::to_string).unwrap_or_default();
    format!("{} {}", field("status"), field("updated_at"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use serde_json::json;

    #[test]
    fn a_watch_follows_the_newest_updates_whatever_the_query_asked() {
        let mut query = vec![
            ("wallet_id".to_owned(), "w".to_owned()),
            ("sort_key".to_owned(), "created_at".to_owned()),
            ("sort_order".to_owned(), "ASC".to_owned()),
            ("cursor".to_owned(), "c1".to_owned()),
        ];
        follow_updates(&mut query);
        let pairs: Vec<(&str, &str)> = query
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("wallet_id", "w"),
                ("sort_key", "updated_at"),
                ("sort_order", "DESC")
            ]
        );
    }

    #[test]
    fn a_page_is_cut_to_the_rows_that_fit() {
        let page = json!({"items": [{"id": "a"}, {"id": "b"}, {"id": "c"}], "next_cursor": "x"});
        let (cut, more) = first(&page, Some(2));
        assert_eq!((items(&cut).len(), more), (2, 1));
        assert_eq!(cut["next_cursor"], "x");
        assert_eq!(first(&page, None).1, 0);
        assert_eq!(first(&page, Some(5)).1, 0);
        let (bare, more) = first(&json!([1, 2, 3]), Some(1));
        assert_eq!((bare, more), (json!([1]), 2));
    }

    #[test]
    fn only_failures_the_server_may_get_past_are_retried() {
        let status = |status: u16| Error {
            response_status: Some(status),
            ..Error::new(ErrorKind::for_http_status(status), "x")
        };
        assert!(retries(&Error::transport("reset")));
        for code in [429, 500, 502, 503] {
            assert!(retries(&status(code)), "{code}");
        }
        for code in [400, 401, 403, 404] {
            assert!(!retries(&status(code)), "{code}");
        }
        assert!(!retries(&Error::api("no status")));
    }

    #[test]
    fn the_first_page_is_a_snapshot_then_only_new_and_changed_payments_count() {
        let mut seen = Seen::default();
        let first = json!({"items": [
            {"id": "a", "status": "receiving", "updated_at": "t1"},
            {"status": "completed"}
        ]});
        let snapshot = seen.update(&first);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            (snapshot[0].0.as_str(), snapshot[0].2),
            ("a", Change::Snapshot)
        );
        assert!(seen.update(&first).is_empty());
        let next = json!([
            {"id": "b", "status": "receiving"},
            {"id": "a", "status": "completed", "updated_at": "t2"}
        ]);
        let changes: Vec<_> = seen
            .update(&next)
            .into_iter()
            .map(|(id, _, change)| (id, change.as_str()))
            .collect();
        assert_eq!(changes, [("b".into(), "new"), ("a".into(), "updated")]);
        assert!(items(&json!({"other": 1})).is_empty());
    }
}

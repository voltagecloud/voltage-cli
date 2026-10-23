//! What a payment wait tells the person at the terminal. An interactive stderr gets one line
//! per status change and a closing line with a symbol; redirected stderr keeps plain
//! sentences, repeated while the status holds, so logs read as they always have.

use crate::{
    output::Outcome,
    payment::{PaymentStatus, StatusText},
    style::{Glyphs, Line, Role},
    terminal::{self, Terminal},
};
use std::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

/// Redirected stderr repeats an unchanged status this often, so a log shows the wait is alive.
const NOTICE_INTERVAL: Duration = Duration::from_secs(15);

pub struct Timeline {
    terminal: Terminal,
    id: Uuid,
    target: &'static str,
    pub started: Instant,
    /// When the invoice expires, once it is shown.
    expires: Option<Instant>,
    last: Option<StatusText>,
    last_notice: Option<Instant>,
}

impl Timeline {
    /// Begin a wait for `target`; `accepted` announces that the payment was just submitted.
    pub fn start(terminal: Terminal, id: Uuid, target: &'static str, accepted: bool) -> Self {
        let timeline = Self {
            terminal,
            id,
            target,
            started: Instant::now(),
            expires: None,
            last: None,
            last_notice: None,
        };
        if accepted {
            if terminal.decorates() {
                terminal.important_line(
                    timeline
                        .step()
                        .text(&format!("Payment {id} accepted; waiting for {target}")),
                );
            } else {
                terminal.important(format!("Payment {id} accepted; waiting for {target}."));
            }
        }
        timeline
    }

    /// Report a polled status: a new line when it changes, and on redirected stderr also
    /// when it has held for a while. On a terminal a final status is left to the closing line.
    pub fn status(&mut self, status: &StatusText) {
        let changed = self.last.as_ref() != Some(status);
        if self.terminal.decorates() {
            if changed && !is_final(status) {
                let line = with_status(self.step(), status);
                self.terminal.notice_line(self.waited(line));
            }
        } else if changed
            || self
                .last_notice
                .is_none_or(|noticed| noticed.elapsed() >= NOTICE_INTERVAL)
        {
            self.terminal.notice(format!(
                "Payment {} status: {}; waiting for {}.",
                self.id,
                status.as_str(),
                self.target
            ));
            self.last_notice = Some(Instant::now());
        }
        self.last = Some(status.clone());
    }

    /// Count down to the invoice's expiry, given in seconds since the epoch, while waiting.
    pub fn expires_at(&mut self, expires: Option<i64>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
            });
        self.expires = expires
            .and_then(|expires| u64::try_from(expires.saturating_sub(now)).ok())
            .filter(|left| *left > 0)
            .and_then(|left| Instant::now().checked_add(Duration::from_secs(left)));
    }

    /// Show `label` with the time waited, and the expiry countdown once there is one, while
    /// `work` runs.
    pub async fn pause<T>(&self, label: String, work: impl Future<Output = T>) -> T {
        match self.expires {
            Some(expires) => {
                self.terminal
                    .expiring(label, self.started, expires, work)
                    .await
            }
            None => self.terminal.waiting(label, self.started, work).await,
        }
    }

    /// The wait reached its target.
    pub fn reached(&self, outcome: Outcome) {
        if self.terminal.decorates() {
            let (role, symbol) = closing(outcome, self.terminal.glyphs());
            let line = Line::default()
                .paint(role, symbol)
                .text(&format!(" Payment {} ", self.id))
                .paint(Role::Success, outcome.as_str());
            self.terminal.notice_line(self.waited(line));
        }
    }

    /// The payment failed or expired; the error report follows. The report already names
    /// the payment and its state, so this line is optional and `--quiet` hides it.
    pub fn unsuccessful(&self, status: Option<&StatusText>) {
        if self.terminal.decorates() {
            let glyphs = self.terminal.glyphs();
            let line = Line::default()
                .paint(Role::Failure, glyphs.failed)
                .text(&format!(" Payment {} ", self.id));
            let line = match status {
                Some(status) => with_status(line, status),
                None => line.text("did not succeed"),
            };
            self.terminal.notice_line(self.waited(line));
        }
    }

    fn step(&self) -> Line {
        Line::default()
            .paint(Role::Hint, self.terminal.glyphs().step)
            .text(" ")
    }

    fn waited(&self, line: Line) -> Line {
        let separator = self.terminal.glyphs().separator;
        line.text(&format!(
            " {separator} {}",
            terminal::elapsed(self.started.elapsed())
        ))
    }
}

/// A known status in its color; text the API added later stays plain.
fn with_status(line: Line, status: &StatusText) -> Line {
    match status {
        StatusText::Known(known) => line.paint(status_role(*known), known.as_str()),
        StatusText::Unknown(text) => line.text(text),
    }
}

/// A settled payment closes with the lightning bolt; a ready invoice, not yet paid, with a
/// check mark.
fn closing(outcome: Outcome, glyphs: Glyphs) -> (Role, &'static str) {
    match outcome {
        Outcome::Completed => (Role::Bolt, glyphs.paid),
        _ => (Role::Success, glyphs.done),
    }
}

fn is_final(status: &StatusText) -> bool {
    matches!(
        status,
        StatusText::Known(
            PaymentStatus::Completed | PaymentStatus::Failed | PaymentStatus::Expired
        )
    )
}

fn status_role(status: PaymentStatus) -> Role {
    match status {
        PaymentStatus::Completed => Role::Success,
        PaymentStatus::Failed | PaymentStatus::Expired => Role::Failure,
        PaymentStatus::Sending
        | PaymentStatus::Receiving
        | PaymentStatus::Approved
        | PaymentStatus::Generating => Role::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settled_payment_closes_with_a_bolt_and_a_ready_one_with_a_check() {
        assert_eq!(
            closing(Outcome::Completed, Glyphs::UNICODE),
            (Role::Bolt, "⚡")
        );
        assert_eq!(
            closing(Outcome::Ready, Glyphs::UNICODE),
            (Role::Success, "✓")
        );
        assert_eq!(
            closing(Outcome::Completed, Glyphs::ASCII),
            (Role::Bolt, "ok")
        );
    }

    #[test]
    fn statuses_are_colored_by_meaning_and_unknown_text_stays_plain() {
        assert_eq!(status_role(PaymentStatus::Completed), Role::Success);
        assert_eq!(status_role(PaymentStatus::Expired), Role::Failure);
        assert_eq!(status_role(PaymentStatus::Failed), Role::Failure);
        assert_eq!(status_role(PaymentStatus::Sending), Role::Pending);
        assert!(is_final(&StatusText::Known(PaymentStatus::Expired)));
        assert!(!is_final(&StatusText::Known(PaymentStatus::Receiving)));
        assert!(!is_final(&StatusText::Unknown("completed later".into())));
        let known = with_status(Line::default(), &StatusText::Known(PaymentStatus::Sending));
        assert_eq!(
            known.as_str(),
            crate::style::paint(Role::Pending, "sending")
        );
        let unknown = with_status(Line::default(), &StatusText::Unknown("new\u{1b}[2J".into()));
        assert_eq!(unknown.as_str(), "new [2J");
    }
}

//! The status line on an interactive stderr: a spinner, a label, and the time waited, redrawn
//! until its guard drops, which clears it. Without animation the label is drawn once.

use crate::{
    style::{Glyphs, Role, paint},
    terminal,
};
use std::{
    io::Write,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::{task::JoinHandle, time::Instant};

const FRAME: Duration = Duration::from_millis(100);
/// A quick wait shows no time; one the caller times shows it from the start.
const SHOW_WAITED_AFTER: Duration = Duration::from_secs(2);

/// How the status line is drawn on this stderr.
#[derive(Clone, Copy, Debug)]
pub struct Look {
    pub animate: bool,
    pub color: bool,
    pub glyphs: Glyphs,
}

/// Clears its status line when dropped, including when Ctrl-C cancels the owning future.
#[must_use = "progress is cleared as soon as the guard is dropped"]
pub struct Progress {
    line: Option<Arc<StatusLine>>,
    redraw: Option<JoinHandle<()>>,
}

impl Progress {
    pub fn hidden() -> Self {
        Self {
            line: None,
            redraw: None,
        }
    }

    /// Draw `label` now and, when animated, every frame after. `since` starts the time of a
    /// wait that spans several status lines.
    pub fn show(label: String, since: Option<Instant>, look: Look) -> Self {
        Self::start(label, since, look, to_stderr)
    }

    /// `show`, writing each frame with `write`.
    fn start(label: String, since: Option<Instant>, look: Look, write: fn(&[u8])) -> Self {
        let line = Arc::new(StatusLine {
            label,
            since: since.unwrap_or_else(Instant::now),
            timed: since.is_some(),
            look,
            // Read once: where crossterm cannot open `/dev/tty`, each read runs `tput`.
            width: terminal::stderr_width(),
            write,
            cleared: Mutex::new(false),
        });
        line.draw(0);
        // Outside a runtime the line stays drawn but still.
        let redraw = look
            .animate
            .then(tokio::runtime::Handle::try_current)
            .and_then(Result::ok)
            .map(|runtime| runtime.spawn(redraw(Arc::clone(&line))));
        Self {
            line: Some(line),
            redraw,
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(redraw) = self.redraw.take() {
            redraw.abort();
        }
        if let Some(line) = &self.line {
            line.clear();
        }
    }
}

async fn redraw(line: Arc<StatusLine>) {
    let mut frames = tokio::time::interval_at(Instant::now() + FRAME, FRAME);
    // Runs until the guard aborts it; the frame number only picks a glyph, so it may wrap.
    let mut frame: usize = 0;
    loop {
        frames.tick().await;
        frame = frame.wrapping_add(1);
        line.draw(frame);
    }
}

struct StatusLine {
    label: String,
    since: Instant,
    timed: bool,
    look: Look,
    /// The terminal's width when the line was first drawn.
    width: Option<usize>,
    /// Where frames and the clear go: stderr, except in tests.
    write: fn(&[u8]),
    /// Set once the line is cleared. A redraw already running when the guard drops checks it
    /// under the lock, so no frame can follow the clear.
    cleared: Mutex<bool>,
}

impl StatusLine {
    fn draw(&self, frame: usize) {
        let cleared = self.cleared.lock().unwrap_or_else(PoisonError::into_inner);
        if *cleared {
            return;
        }
        let text = self.text(frame, self.since.elapsed(), self.width);
        (self.write)(format!("\r\x1b[2K{text}").as_bytes());
    }

    fn clear(&self) {
        let mut cleared = self.cleared.lock().unwrap_or_else(PoisonError::into_inner);
        *cleared = true;
        (self.write)(b"\r\x1b[2K");
    }

    /// The line for one frame, cut to fit `width` so clearing never leaves a wrapped remnant.
    fn text(&self, frame: usize, waited: Duration, width: Option<usize>) -> String {
        let label = if self.timed || waited >= SHOW_WAITED_AFTER {
            format!("{} {}", self.label, terminal::elapsed(waited))
        } else {
            self.label.clone()
        };
        if !self.look.animate {
            return fit(&label, width);
        }
        let frames = self.look.glyphs.frames;
        let glyph = frames[frame % frames.len()];
        let label = fit(
            &label,
            width.map(|width| width.saturating_sub(glyph.chars().count() + 1)),
        );
        if self.look.color {
            format!("{} {label}", paint(Role::Pending, glyph))
        } else {
            format!("{glyph} {label}")
        }
    }
}

/// A frame or clear on stderr; a failed write has nowhere to be reported.
fn to_stderr(bytes: &[u8]) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(bytes);
    let _ = stderr.flush();
}

/// `text` cut to leave the last of `width` columns free, so the cursor never wraps.
fn fit(text: &str, width: Option<usize>) -> String {
    match width {
        Some(width) if text.chars().count() >= width => {
            text.chars().take(width.saturating_sub(1)).collect()
        }
        _ => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(timed: bool, look: Look) -> StatusLine {
        StatusLine {
            label: "Waiting...".into(),
            since: Instant::now(),
            timed,
            look,
            width: None,
            write: |_| {},
            cleared: Mutex::new(false),
        }
    }

    const STILL: Look = Look {
        animate: false,
        color: false,
        glyphs: Glyphs::UNICODE,
    };
    const SPINNING: Look = Look {
        animate: true,
        ..STILL
    };

    #[test]
    fn frames_advance_and_wrap_with_the_label_after_the_glyph() {
        let spinner = line(false, SPINNING);
        let short = Duration::from_millis(300);
        assert_eq!(spinner.text(0, short, None), "⠋ Waiting...");
        assert_eq!(spinner.text(1, short, None), "⠙ Waiting...");
        assert_eq!(spinner.text(10, short, None), "⠋ Waiting...");
        let ascii = line(
            false,
            Look {
                glyphs: Glyphs::ASCII,
                ..SPINNING
            },
        );
        assert_eq!(ascii.text(5, short, None), "\\ Waiting...");
        let colored = line(
            false,
            Look {
                color: true,
                ..SPINNING
            },
        );
        let text = colored.text(0, short, None);
        assert!(text.starts_with(&paint(Role::Pending, "⠋")), "{text:?}");
        assert!(text.ends_with(" Waiting..."));
    }

    #[test]
    fn time_shows_from_two_seconds_or_from_the_start_of_a_timed_wait() {
        let quick = line(false, STILL);
        assert_eq!(
            quick.text(0, Duration::from_millis(1999), None),
            "Waiting..."
        );
        assert_eq!(quick.text(0, Duration::from_secs(2), None), "Waiting... 2s");
        let timed = line(true, STILL);
        assert_eq!(timed.text(0, Duration::ZERO, None), "Waiting... 0s");
        assert_eq!(
            timed.text(0, Duration::from_secs(65), None),
            "Waiting... 1m 05s"
        );
    }

    #[test]
    fn a_line_never_reaches_the_last_column() {
        let still = line(false, STILL);
        assert_eq!(still.text(0, Duration::ZERO, Some(8)), "Waiting");
        assert_eq!(still.text(0, Duration::ZERO, Some(11)), "Waiting...");
        assert_eq!(still.text(0, Duration::ZERO, Some(10)), "Waiting..");
        let spinner = line(false, SPINNING);
        assert_eq!(spinner.text(0, Duration::ZERO, Some(8)), "⠋ Waiti");
        assert_eq!(fit("abc", Some(0)), "");
        assert_eq!(fit("abc", None), "abc");
    }

    #[tokio::test]
    async fn dropping_the_guard_stops_the_redraw_and_clears_once() {
        let progress = Progress::start("x".into(), None, SPINNING, |_| {});
        let redraw = progress
            .redraw
            .as_ref()
            .map(JoinHandle::abort_handle)
            .unwrap();
        let line = Arc::clone(progress.line.as_ref().unwrap());
        drop(progress);
        assert!(*line.cleared.lock().unwrap());
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(redraw.is_finished());
        // A still line never starts a redraw.
        let still = Progress::start("x".into(), None, STILL, |_| {});
        assert!(still.redraw.is_none());
        assert!(Progress::hidden().line.is_none());
    }
}

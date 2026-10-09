//! Terminal interaction policy: prompts, progress, and diagnostics on stderr. Results and
//! errors are written by `output`.

use crate::{Error, Result, secret::Secret};
use inquire::{InquireError, Select};
use std::{
    fmt::{self, Display},
    io::{ErrorKind, IsTerminal, Write},
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// What the process may show to and ask of the person at the terminal, from `--quiet` and
/// `--no-input`.
#[derive(Clone, Copy)]
pub struct Terminal {
    quiet: bool,
    no_input: bool,
}

impl Terminal {
    pub fn new(quiet: bool, no_input: bool) -> Self {
        Self { quiet, no_input }
    }

    /// A prompt needs an interactive stdin and must not be disabled by `--no-input`.
    pub fn can_prompt(self) -> bool {
        !self.no_input && std::io::stdin().is_terminal()
    }

    /// Optional status for a human, cleared when the guard drops; no control codes on
    /// redirected stderr.
    pub fn progress(self, label: &'static str) -> Progress {
        let active = !self.quiet && std::io::stderr().is_terminal();
        if active {
            eprint!("{label}");
            let _ = std::io::stderr().flush();
        }
        Progress { active }
    }

    /// Show `label` while `work` runs. The status cannot outlive the work: it clears on
    /// completion, error, or cancellation.
    pub async fn during<T>(self, label: &'static str, work: impl Future<Output = T>) -> T {
        let _progress = self.progress(label);
        work.await
    }

    /// Optional information that `--quiet` suppresses.
    pub fn notice(self, message: impl Display) {
        if !self.quiet {
            eprintln!("{}", scrub_text(&message.to_string()));
        }
    }

    /// Information needed to confirm or recover an operation; `--quiet` never hides it.
    pub fn important(self, message: impl Display) {
        eprintln!("{}", scrub_text(&message.to_string()));
    }

    /// A picker also draws on stderr, so it needs both streams to be interactive.
    pub fn can_pick(self) -> bool {
        self.can_prompt() && std::io::stderr().is_terminal()
    }

    /// Ask the person to pick one choice, filtering as they type. `None` means there was
    /// nothing to pick or they pressed Esc. Raw mode turns a typed Ctrl-C into a key, so the
    /// picker reports Ctrl-C as an interruption.
    pub async fn pick(self, question: &'static str, choices: Vec<Choice>) -> Result<Option<Uuid>> {
        if choices.is_empty() {
            return Ok(None);
        }
        #[cfg(unix)]
        let _modes = TerminalModes::save();
        let _screen = PickerScreen;
        let picked =
            read_detached(move || Select::new(question, choices).with_page_size(12).prompt())
                .await?;
        match picked {
            Ok(choice) => Ok(Some(choice.id)),
            Err(InquireError::OperationCanceled) => Ok(None),
            Err(InquireError::OperationInterrupted) => Err(Error::interrupted(
                "Interrupted before any resource change was submitted",
            )),
            Err(_) => Err(Error::transport(
                "Could not read the selection from the terminal",
            )),
        }
    }

    /// Read a secret without echo.
    pub async fn read_hidden(self, prompt: &'static str) -> Result<Secret> {
        // The detached read restores echo only when it returns, and Ctrl-C does not wait for
        // it. This guard restores the terminal when the read completes or is cancelled.
        #[cfg(unix)]
        let _modes = TerminalModes::save();
        match read_detached(move || rpassword::prompt_password(prompt).map(Secret::new)).await? {
            // rpassword turns off terminal signals, so a typed Ctrl-C reaches it as a
            // character. It raises SIGINT itself and then returns this error; waiting here
            // lets the interrupt handler report the interruption instead of an I/O failure.
            Err(error) if error.kind() == ErrorKind::Interrupted => std::future::pending().await,
            read => Ok(read?),
        }
    }
}

/// The picker turns on bracketed paste and hides the cursor, and undoes both only when its
/// read returns. A signal that ends the command first leaves that to this guard.
struct PickerScreen;

impl Drop for PickerScreen {
    fn drop(&mut self) {
        let mut stderr = std::io::stderr();
        let _ = stderr.write_all(b"\x1b[?2004l\x1b[?25h");
        let _ = stderr.flush();
    }
}

/// One resource a person can pick: its name, scrubbed because the API supplied it, and its ID.
pub struct Choice {
    id: Uuid,
    name: String,
}

impl Choice {
    pub fn new(id: Uuid, name: &str) -> Self {
        Self {
            id,
            name: scrub_line(name),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}  {}", self.name, self.id)
    }
}

/// The terminal's width in columns: `COLUMNS` when set, else the controlling terminal's
/// size while stdout is interactive. crossterm falls back to running `tput` when the size
/// query fails. `None` means output is not width-limited.
pub fn width() -> Option<usize> {
    if let Some(columns) = std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|columns| *columns > 0)
    {
        return Some(columns);
    }
    if !std::io::stdout().is_terminal() {
        return None;
    }
    crossterm::terminal::size()
        .ok()
        .map(|(columns, _)| usize::from(columns))
        .filter(|columns| *columns > 0)
}

/// Characters that could rewrite, reorder, or hide text on the terminal: C0 and C1
/// controls, every Unicode `Bidi_Control` mark (including U+061C), zero-width and other
/// invisible formatting characters, the line and paragraph separators, interlinear
/// annotation marks, Hangul fillers that render as blank, and the tag characters that can
/// carry hidden text.
fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{115F}'..='\u{1160}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{3164}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// Untrusted text for one line: every unsafe character, including a newline, becomes a space.
pub fn scrub_line(text: &str) -> String {
    text.chars()
        .map(|c| if is_unsafe(c) { ' ' } else { c })
        .collect()
}

/// Text that may span lines: newlines stay, and every other unsafe character becomes a space.
pub fn scrub_text(text: &str) -> String {
    text.chars()
        .map(|c| if c != '\n' && is_unsafe(c) { ' ' } else { c })
        .collect()
}

/// A blocked read of a terminal, stdin, or FIFO cannot be cancelled, and a runtime waits for
/// its blocking pool at shutdown. The read therefore runs on a detached thread whose only
/// completion signal is the channel: Ctrl-C ends the process without waiting for the input,
/// and process exit discards the unfinished read.
pub async fn read_detached<T: Send + 'static>(
    read: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    let (sender, receiver) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read());
    });
    receiver
        .await
        .map_err(|_| Error::transport("Terminal input ended unexpectedly"))
}

/// The controlling terminal's attributes, put back when dropped. `rpassword` turns off echo
/// on `/dev/tty` and turns it back on only when its read returns.
#[cfg(unix)]
struct TerminalModes {
    tty: std::fs::File,
    saved: libc::termios,
}

#[cfg(unix)]
impl TerminalModes {
    /// `None` without a controlling terminal, where there is nothing to restore.
    fn save() -> Option<Self> {
        use std::os::fd::AsRawFd;
        let tty = std::fs::File::open("/dev/tty").ok()?;
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: the descriptor is open for the duration of the call, and tcgetattr fully
        // initializes `saved` when it returns 0.
        if unsafe { libc::tcgetattr(tty.as_raw_fd(), saved.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: tcgetattr returned 0 above.
        let saved = unsafe { saved.assume_init() };
        Some(Self { tty, saved })
    }
}

#[cfg(unix)]
impl Drop for TerminalModes {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is still owned by `tty`, and `saved` holds attributes read
        // from the same terminal.
        unsafe { libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.saved) };
    }
}

/// Clears its status line when dropped, including when Ctrl-C cancels the owning future.
#[must_use = "progress is cleared as soon as the guard is dropped"]
pub struct Progress {
    active: bool,
}

impl Drop for Progress {
    fn drop(&mut self) {
        if self.active {
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbing_removes_controls_and_bidirectional_marks() {
        let hostile = "a\u{1b}[2J\u{9b}b\u{202e}c\u{200b}d\te\nf";
        assert_eq!(scrub_line(hostile), "a [2J b c d e f");
        assert_eq!(scrub_text(hostile), "a [2J b c d e\nf");
        assert_eq!(scrub_line("plain ₿ text"), "plain ₿ text");
        // The rest of Bidi_Control, invisible operators, soft hyphen, and line separators.
        assert_eq!(
            scrub_line("a\u{061c}b\u{2028}c\u{2029}d\u{2060}e\u{2064}f\u{00ad}g"),
            "a b c d e f g"
        );
        // Tag characters can spell hidden ASCII; fillers and annotation marks render blank.
        assert_eq!(
            scrub_line("ok\u{e0068}\u{e0069}\u{e0064}\u{e0065}"),
            "ok    "
        );
        assert_eq!(
            scrub_line("a\u{180e}b\u{3164}c\u{115f}d\u{ffa0}e\u{fff9}f\u{e007f}g"),
            "a b c d e f g"
        );
    }
}

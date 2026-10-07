//! Terminal interaction policy: prompts, progress, and diagnostics on stderr. Results and
//! errors are written by `output`.

use crate::{Error, Result, secret::Secret};
use std::{
    fmt::Display,
    io::{ErrorKind, IsTerminal, Write},
};
use tokio::sync::oneshot;

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
            eprintln!("{message}");
        }
    }

    /// Information needed to confirm or recover an operation; `--quiet` never hides it.
    pub fn important(self, message: impl Display) {
        eprintln!("{message}");
    }

    /// Ask a yes/no question on stderr. Anything but an explicit yes declines.
    pub async fn confirm(self, question: &str) -> Result<bool> {
        eprint!("{question} [y/N] ");
        std::io::stderr().flush()?;
        let reply = read_detached(|| {
            let mut reply = String::new();
            std::io::stdin().read_line(&mut reply).map(|_| reply)
        })
        .await??;
        Ok(matches!(reply.trim(), "y" | "Y" | "yes"))
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

//! Terminal interaction policy: prompts, progress, and diagnostics on stderr. Results and
//! errors are written by `output`.

use crate::{Error, Result, secret::Secret};
use std::{
    fmt::Display,
    io::{IsTerminal, Write},
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
        .await?;
        Ok(matches!(reply.trim(), "y" | "Y" | "yes"))
    }

    /// Read a secret without echo.
    pub async fn read_hidden(self, prompt: &'static str) -> Result<Secret> {
        read_detached(move || rpassword::prompt_password(prompt).map(Secret::new)).await
    }
}

/// A blocked terminal read cannot be cancelled, and a runtime waits for its blocking pool at
/// shutdown. The read therefore runs on a detached thread whose only completion signal is
/// the channel: Ctrl-C ends the process without waiting for the person to press Enter, and
/// process exit discards the unfinished read.
async fn read_detached<T: Send + 'static>(
    read: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T> {
    let (sender, receiver) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read());
    });
    receiver
        .await
        .map_err(|_| Error::transport("Terminal input ended unexpectedly"))?
        .map_err(Error::from)
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

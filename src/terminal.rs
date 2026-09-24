//! Terminal-only progress and diagnostic policy. Results and errors are handled by `output`.
use std::io::{IsTerminal, Write};

#[derive(Clone, Copy)]
pub struct Terminal {
    quiet: bool,
}

impl Terminal {
    pub fn new(quiet: bool) -> Self {
        Self { quiet }
    }

    pub fn can_prompt(self, no_input: bool) -> bool {
        !no_input && std::io::stdin().is_terminal()
    }

    /// Optional status for a human; no control codes on redirected stderr.
    pub fn progress(self, label: &'static str) -> Progress {
        let active = !self.quiet && std::io::stderr().is_terminal();
        if active {
            eprint!("{label}");
            let _ = std::io::stderr().flush();
        }
        Progress { active }
    }

    pub fn notice(self, message: impl std::fmt::Display) {
        if !self.quiet {
            eprintln!("{message}");
        }
    }

    /// Always show information necessary to confirm or recover an operation.
    pub fn important(self, message: impl std::fmt::Display) {
        eprintln!("{message}");
    }
}

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

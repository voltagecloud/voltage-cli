//! Terminal color: one policy for clap's help and parse errors and for the CLI's own labels.
//!
//! `--color always|never` decides; otherwise each stream follows anstream's automatic choice,
//! which honors `NO_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR`, `TERM=dumb`, and whether that
//! stream is a terminal. Only static labels are painted. Untrusted text stays scrubbed and
//! plain, so every escape sequence the CLI writes comes from this palette. Machine-readable
//! output never passes through here.

use clap::{
    ValueEnum,
    builder::{
        Styles,
        styling::{AnsiColor, Style},
    },
};
use std::{ffi::OsStr, io::Write};

/// When to color terminal output, from `--color` or `--no-color`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ColorChoice {
    /// Color a stream only when it is a terminal and the environment allows it
    #[default]
    Auto,
    /// Color even when redirected; JSON output is never colored
    Always,
    /// Never color
    Never,
}

impl ColorChoice {
    /// The choice on a raw command line, for output written before clap has parsed it: clap's
    /// own help and parse errors. Stops at `--`, and the last `--color` or `--no-color` wins;
    /// a value clap would reject reads as `Auto`, and clap then reports it.
    pub fn requested<I, T>(args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: AsRef<OsStr>,
    {
        let mut choice = Self::Auto;
        let mut args = args
            .into_iter()
            .skip(1)
            .take_while(|arg| arg.as_ref() != OsStr::new("--"));
        while let Some(arg) = args.next() {
            let Some(arg) = arg.as_ref().to_str() else {
                continue;
            };
            let value = match arg {
                "--no-color" => {
                    choice = Self::Never;
                    continue;
                }
                "--color" => args
                    .next()
                    .and_then(|v| v.as_ref().to_str().map(str::to_owned)),
                _ => arg.strip_prefix("--color=").map(str::to_owned),
            };
            if let Some(value) = value {
                choice = Self::from_str(&value, false).unwrap_or_default();
            }
        }
        choice
    }

    pub fn as_clap(self) -> clap::ColorChoice {
        match self {
            Self::Auto => clap::ColorChoice::Auto,
            Self::Always => clap::ColorChoice::Always,
            Self::Never => clap::ColorChoice::Never,
        }
    }

    fn as_stream(self) -> anstream::ColorChoice {
        match self {
            Self::Auto => anstream::ColorChoice::Auto,
            Self::Always => anstream::ColorChoice::Always,
            Self::Never => anstream::ColorChoice::Never,
        }
    }
}

/// What a painted label means. Color never carries the meaning alone: each label is a word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// `error:`
    Failure,
    /// `hint:`
    Hint,
    /// `details:`, which introduces lines a person may skip
    Muted,
}

impl Role {
    const fn style(self) -> Style {
        match self {
            Self::Failure => AnsiColor::Red.on_default().bold(),
            Self::Hint => AnsiColor::Cyan.on_default().bold(),
            Self::Muted => Style::new().dimmed(),
        }
    }
}

/// `label` wrapped in its role's style. Labels are fixed text, never API or user data.
pub fn paint(role: Role, label: &'static str) -> String {
    let style = role.style();
    format!("{style}{label}{style:#}")
}

/// Write `text` to stderr, keeping its styles only when `choice` allows color there. A
/// stderr write failure has nowhere to be reported, so it is ignored.
pub fn write_stderr(choice: ColorChoice, text: &str) {
    let mut stderr = anstream::AutoStream::new(std::io::stderr().lock(), choice.as_stream());
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
}

/// Help and parse-error styles in cargo's manner, using only the 16 basic colors so the
/// terminal's theme keeps them legible.
pub const HELP: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().bold())
    .usage(AnsiColor::Green.on_default().bold())
    .literal(AnsiColor::Cyan.on_default().bold())
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::Red.on_default().bold())
    .valid(AnsiColor::Green.on_default().bold())
    .invalid(AnsiColor::Yellow.on_default().bold());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_color_option_before_a_separator_wins() {
        let requested = |args: &[&str]| ColorChoice::requested(args);
        assert_eq!(
            requested(&["voltage", "wallets", "list"]),
            ColorChoice::Auto
        );
        assert_eq!(
            requested(&["voltage", "--color", "always"]),
            ColorChoice::Always
        );
        assert_eq!(
            requested(&["voltage", "x", "--color=never"]),
            ColorChoice::Never
        );
        assert_eq!(requested(&["voltage", "--no-color"]), ColorChoice::Never);
        assert_eq!(
            requested(&["voltage", "--no-color", "--color", "always"]),
            ColorChoice::Always
        );
        // After `--` the spelling is a positional value, and a bad value is clap's to reject.
        assert_eq!(
            requested(&["voltage", "--", "--color=always"]),
            ColorChoice::Auto
        );
        assert_eq!(
            requested(&["voltage", "--color", "loud"]),
            ColorChoice::Auto
        );
        assert_eq!(requested(&["voltage", "--color"]), ColorChoice::Auto);
        // The program name is not an option.
        assert_eq!(requested(&["--no-color"]), ColorChoice::Auto);
    }

    #[test]
    fn a_painted_label_is_the_label_once_styles_are_stripped() {
        for role in [Role::Failure, Role::Hint, Role::Muted] {
            let painted = paint(role, "error:");
            assert!(painted.starts_with('\u{1b}') && painted.ends_with("\u{1b}[0m"));
            assert_eq!(anstream::adapter::strip_str(&painted).to_string(), "error:");
        }
    }

    #[test]
    fn choices_map_one_to_one_onto_clap_and_anstream() {
        for (choice, clap, stream) in [
            (
                ColorChoice::Auto,
                clap::ColorChoice::Auto,
                anstream::ColorChoice::Auto,
            ),
            (
                ColorChoice::Always,
                clap::ColorChoice::Always,
                anstream::ColorChoice::Always,
            ),
            (
                ColorChoice::Never,
                clap::ColorChoice::Never,
                anstream::ColorChoice::Never,
            ),
        ] {
            assert_eq!(choice.as_clap(), clap);
            assert_eq!(choice.as_stream(), stream);
        }
    }
}

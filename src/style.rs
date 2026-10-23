//! Terminal color: one policy for clap's help and parse errors and for the CLI's own labels.
//!
//! `--color always|never` decides; otherwise each stream follows anstream's automatic choice,
//! which honors `NO_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR`, `TERM=dumb`, and whether that
//! stream is a terminal. A `Line` takes fixed labels and untrusted text, and scrubs the text
//! before styling it, so every escape sequence the CLI writes comes from this palette or from
//! an OSC 8 link whose visible text is its target. Machine-readable output never passes
//! through here.

use crate::terminal::scrub_line;
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

    /// Whether text written straight to stderr, outside an anstream writer, may carry styles.
    pub fn colors_stderr(self) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => {
                anstream::AutoStream::choice(&std::io::stderr()) == anstream::ColorChoice::Always
            }
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
    /// `✓` and a completed status
    Success,
    /// The spinner and in-flight statuses such as `sending`
    Pending,
    /// `!`: something was deliberately not done
    Caution,
    /// A URL a person opens
    Link,
    /// A value a person reads or types, such as a device code
    Emphasis,
}

impl Role {
    const fn style(self) -> Style {
        match self {
            Self::Failure => AnsiColor::Red.on_default().bold(),
            Self::Hint => AnsiColor::Cyan.on_default().bold(),
            Self::Muted => Style::new().dimmed(),
            Self::Success => AnsiColor::Green.on_default(),
            Self::Pending => AnsiColor::Yellow.on_default(),
            Self::Caution => AnsiColor::Yellow.on_default().bold(),
            Self::Link => AnsiColor::Blue.on_default().underline(),
            Self::Emphasis => Style::new().bold(),
        }
    }
}

/// One line of terminal text built only from painted labels and scrubbed text, so its only
/// escape sequences are the palette's.
#[derive(Debug, Default)]
pub struct Line(String);

impl Line {
    pub fn paint(mut self, role: Role, label: &'static str) -> Self {
        self.0.push_str(&paint(role, label));
        self
    }

    /// Untrusted or variable text, with every unsafe character made a space.
    pub fn text(mut self, text: &str) -> Self {
        self.0.push_str(&scrub_line(text));
        self
    }

    /// `text` in a role's style.
    pub fn styled(mut self, role: Role, text: &str) -> Self {
        let style = role.style();
        self.0
            .push_str(&format!("{style}{}{style:#}", scrub_line(text)));
        self
    }

    /// A URL that terminals with OSC 8 support make clickable. The visible text is the whole
    /// URL, so a link cannot hide where it goes; anstream strips the sequence with color off.
    pub fn link(mut self, url: &str) -> Self {
        let url = scrub_line(url);
        let style = Role::Link.style();
        self.0.push_str(&format!(
            "\x1b]8;;{url}\x1b\\{style}{url}{style:#}\x1b]8;;\x1b\\"
        ));
        self
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Status symbols: Unicode where the terminal can draw it, ASCII otherwise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Glyphs {
    pub done: &'static str,
    pub failed: &'static str,
    pub step: &'static str,
    /// Separates a status line's facts, as in `completed · 4s`.
    pub separator: &'static str,
    pub frames: &'static [&'static str],
}

impl Glyphs {
    pub const UNICODE: Self = Self {
        done: "✓",
        failed: "✗",
        step: "›",
        separator: "·",
        frames: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    };
    pub const ASCII: Self = Self {
        done: "ok",
        failed: "x",
        step: ">",
        separator: "-",
        frames: &["-", "\\", "|", "/"],
    };

    /// Unicode on Windows Terminal, or where `TERM` is not `dumb` or `linux` and the locale
    /// that applies (the first set of `LC_ALL`, `LC_CTYPE`, `LANG`) is UTF-8.
    pub fn detect(var: impl Fn(&str) -> Option<String>) -> Self {
        let unicode = if cfg!(windows) {
            var("WT_SESSION").is_some()
        } else {
            !matches!(var("TERM").as_deref(), Some("dumb" | "linux"))
                && ["LC_ALL", "LC_CTYPE", "LANG"]
                    .into_iter()
                    .find_map(|name| var(name).filter(|value| !value.is_empty()))
                    .is_some_and(|locale| {
                        let locale = locale.to_ascii_lowercase();
                        locale.contains("utf-8") || locale.contains("utf8")
                    })
        };
        if unicode { Self::UNICODE } else { Self::ASCII }
    }
}

/// `label` wrapped in its role's style. Labels are fixed text, never API or user data.
pub fn paint(role: Role, label: &'static str) -> String {
    let style = role.style();
    format!("{style}{label}{style:#}")
}

/// Write `text` to stderr, keeping its styles only when `choice` allows color there; anstream
/// strips them otherwise. A stderr write failure has nowhere to be reported, so it is ignored.
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
        for role in [
            Role::Failure,
            Role::Hint,
            Role::Muted,
            Role::Success,
            Role::Pending,
            Role::Caution,
            Role::Link,
            Role::Emphasis,
        ] {
            let painted = paint(role, "error:");
            assert!(painted.starts_with('\u{1b}') && painted.ends_with("\u{1b}[0m"));
            assert_eq!(anstream::adapter::strip_str(&painted).to_string(), "error:");
        }
    }

    #[test]
    fn lines_scrub_text_and_paint_labels() {
        let line = Line::default()
            .paint(Role::Success, "✓")
            .text(" Payment \u{1b}[2Jx\ncompleted");
        assert_eq!(
            anstream::adapter::strip_str(line.as_str()).to_string(),
            "✓ Payment  [2Jx completed"
        );
        assert_eq!(line.as_str().matches('\u{1b}').count(), 2);
    }

    #[test]
    fn a_link_shows_its_whole_scrubbed_url_and_strips_to_it() {
        let line = Line::default().link("https://x.test/a\u{1b}]8;;https://evil.test\u{7}");
        let shown = "https://x.test/a ]8;;https://evil.test ";
        assert_eq!(
            anstream::adapter::strip_str(line.as_str()).to_string(),
            shown
        );
        assert!(
            line.as_str()
                .starts_with(&format!("\u{1b}]8;;{shown}\u{1b}\\"))
        );
        assert!(line.as_str().ends_with("\u{1b}]8;;\u{1b}\\"));
        let code = Line::default().styled(Role::Emphasis, "AB\u{1b}CD");
        assert_eq!(
            anstream::adapter::strip_str(code.as_str()).to_string(),
            "AB CD"
        );
    }

    #[test]
    fn glyphs_are_unicode_only_for_a_utf8_locale_on_a_capable_terminal() {
        let detect = |vars: &[(&str, &str)]| {
            Glyphs::detect(|name| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            })
        };
        if cfg!(windows) {
            assert_eq!(detect(&[("WT_SESSION", "1")]), Glyphs::UNICODE);
            assert_eq!(detect(&[("LANG", "en_US.UTF-8")]), Glyphs::ASCII);
            return;
        }
        assert_eq!(detect(&[("LANG", "en_US.UTF-8")]), Glyphs::UNICODE);
        assert_eq!(detect(&[("LC_CTYPE", "C.utf8")]), Glyphs::UNICODE);
        assert_eq!(detect(&[]), Glyphs::ASCII);
        assert_eq!(detect(&[("LANG", "C")]), Glyphs::ASCII);
        // LC_ALL overrides LANG, and an empty value is unset.
        assert_eq!(
            detect(&[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")]),
            Glyphs::ASCII
        );
        assert_eq!(
            detect(&[("LC_ALL", ""), ("LANG", "en_US.UTF-8")]),
            Glyphs::UNICODE
        );
        for term in ["dumb", "linux"] {
            assert_eq!(
                detect(&[("TERM", term), ("LANG", "en_US.UTF-8")]),
                Glyphs::ASCII
            );
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

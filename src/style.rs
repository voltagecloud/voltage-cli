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
        styling::{Ansi256Color, AnsiColor, Color, RgbColor, Style},
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

    /// Fold in `FORCE_COLOR` when no flag decided: `0` or `false` turns color off, and any
    /// other non-empty value turns it on even when redirected. `NO_COLOR` still wins, and
    /// `FORCE_COLOR` wins over `CLICOLOR_FORCE` and `CLICOLOR`, which anstream reads.
    pub fn with_environment(self, var: impl Fn(&str) -> Option<String>) -> Self {
        let set = |name| var(name).filter(|value: &String| !value.is_empty());
        if self != Self::Auto || set("NO_COLOR").is_some() {
            return self;
        }
        match set("FORCE_COLOR").as_deref() {
            Some("0" | "false") => Self::Never,
            Some(_) => Self::Always,
            None => self,
        }
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
    /// The wordmark's lightning bolt; decoration, never a status
    Bolt,
    /// A QR code: black on bright white whatever the theme, so it scans
    QrCode,
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
            Self::Bolt => AnsiColor::Yellow.on_default().bold(),
            Self::QrCode => Style::new()
                .fg_color(Some(Color::Ansi(AnsiColor::Black)))
                .bg_color(Some(Color::Ansi(AnsiColor::BrightWhite))),
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

    /// Variable text in a style chosen at run time, such as a network's color.
    pub fn with_style(mut self, style: Style, text: &str) -> Self {
        self.0
            .push_str(&format!("{style}{}{style:#}", scrub_line(text)));
        self
    }

    /// Untrusted or variable text in the brand color, such as a wallet's name.
    pub fn styled_brand(mut self, depth: Depth, text: &str) -> Self {
        let style = brand(depth);
        self.0
            .push_str(&format!("{style}{}{style:#}", scrub_line(text)));
        self
    }

    /// Fixed text in the brand color, at the depth the terminal draws.
    pub fn brand(mut self, depth: Depth, label: &'static str) -> Self {
        let style = brand(depth);
        self.0.push_str(&format!("{style}{label}{style:#}"));
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
    /// A settled payment: the lightning bolt.
    pub paid: &'static str,
    pub failed: &'static str,
    pub step: &'static str,
    /// Separates a status line's facts, as in `completed · 4s`.
    pub separator: &'static str,
    pub frames: &'static [&'static str],
}

impl Glyphs {
    pub const UNICODE: Self = Self {
        done: "✓",
        paid: "⚡",
        failed: "✗",
        step: "›",
        separator: "·",
        frames: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    };
    pub const ASCII: Self = Self {
        done: "ok",
        paid: "ok",
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

/// Locked stdout that keeps styles only when `choice` allows color there.
pub fn stdout(choice: ColorChoice) -> anstream::AutoStream<std::io::StdoutLock<'static>> {
    anstream::AutoStream::new(std::io::stdout().lock(), choice.as_stream())
}

/// How many colors the terminal draws, for the one brand color; every other style uses
/// the 16 basic colors, which the terminal's theme keeps legible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Depth {
    Basic,
    Ansi256,
    TrueColor,
}

impl Depth {
    /// `FORCE_COLOR=2` or `3` names a depth, as chalk reads it; otherwise `COLORTERM` of
    /// `truecolor` or `24bit` (or Windows Terminal) means 24-bit, and a `TERM` naming
    /// `256color` means 256 colors.
    pub fn detect(var: impl Fn(&str) -> Option<String>) -> Self {
        match var("FORCE_COLOR").as_deref() {
            Some("3") => return Self::TrueColor,
            Some("2") => return Self::Ansi256,
            _ => {}
        }
        let truecolor = matches!(var("COLORTERM").as_deref(), Some("truecolor" | "24bit"))
            || (cfg!(windows) && var("WT_SESSION").is_some());
        if truecolor {
            Self::TrueColor
        } else if var("TERM").is_some_and(|term| term.contains("256color")) {
            Self::Ansi256
        } else {
            Self::Basic
        }
    }
}

/// Voltage orange, `#FF5000`, at the depth the terminal draws; bold alone without one.
pub fn brand(depth: Depth) -> Style {
    let color = match depth {
        Depth::TrueColor => Some(Color::Rgb(RgbColor(0xFF, 0x50, 0x00))),
        Depth::Ansi256 => Some(Color::Ansi256(Ansi256Color(202))),
        Depth::Basic => None,
    };
    Style::new().fg_color(color).bold()
}

/// Help and parse-error styles in cargo's manner, with headers and usage in the brand color.
pub fn help(depth: Depth) -> Styles {
    Styles::styled()
        .header(brand(depth))
        .usage(brand(depth))
        .literal(AnsiColor::Cyan.on_default().bold())
        .placeholder(AnsiColor::Cyan.on_default())
        .error(AnsiColor::Red.on_default().bold())
        .valid(AnsiColor::Green.on_default().bold())
        .invalid(AnsiColor::Yellow.on_default().bold())
}

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
            Role::Bolt,
            Role::QrCode,
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

    fn vars<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn force_color_applies_only_without_a_flag_or_no_color() {
        use ColorChoice::{Always, Auto, Never};
        for (choice, env, expected) in [
            (Auto, &[][..], Auto),
            (Auto, &[("FORCE_COLOR", "1")][..], Always),
            (Auto, &[("FORCE_COLOR", "3")][..], Always),
            (Auto, &[("FORCE_COLOR", "true")][..], Always),
            (Auto, &[("FORCE_COLOR", "0")][..], Never),
            (Auto, &[("FORCE_COLOR", "false")][..], Never),
            (Auto, &[("FORCE_COLOR", "")][..], Auto),
            // NO_COLOR wins; anstream then turns color off.
            (Auto, &[("FORCE_COLOR", "1"), ("NO_COLOR", "1")][..], Auto),
            (Auto, &[("FORCE_COLOR", "1"), ("NO_COLOR", "")][..], Always),
            // A flag wins over both.
            (Never, &[("FORCE_COLOR", "1")][..], Never),
            (Always, &[("FORCE_COLOR", "0")][..], Always),
        ] {
            assert_eq!(
                choice.with_environment(vars(env)),
                expected,
                "{choice:?} {env:?}"
            );
        }
    }

    #[test]
    fn depth_follows_force_color_then_colorterm_then_term() {
        assert_eq!(Depth::detect(vars(&[])), Depth::Basic);
        assert_eq!(
            Depth::detect(vars(&[("TERM", "xterm-256color")])),
            Depth::Ansi256
        );
        assert_eq!(
            Depth::detect(vars(&[("COLORTERM", "truecolor")])),
            Depth::TrueColor
        );
        assert_eq!(
            Depth::detect(vars(&[("COLORTERM", "24bit")])),
            Depth::TrueColor
        );
        assert_eq!(
            Depth::detect(vars(&[("FORCE_COLOR", "2"), ("COLORTERM", "truecolor")])),
            Depth::Ansi256
        );
        assert_eq!(
            Depth::detect(vars(&[("FORCE_COLOR", "3")])),
            Depth::TrueColor
        );
        assert_eq!(Depth::detect(vars(&[("FORCE_COLOR", "1")])), Depth::Basic);
    }

    #[test]
    fn the_brand_color_degrades_to_bold() {
        assert_eq!(
            format!("{}", brand(Depth::TrueColor)),
            "\u{1b}[1m\u{1b}[38;2;255;80;0m"
        );
        assert_eq!(
            format!("{}", brand(Depth::Ansi256)),
            "\u{1b}[1m\u{1b}[38;5;202m"
        );
        assert_eq!(format!("{}", brand(Depth::Basic)), "\u{1b}[1m");
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

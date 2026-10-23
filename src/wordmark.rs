//! The `voltage` wordmark, shown on a terminal at moments worth marking, such as a first
//! login: a lightning bolt beside the name. ASCII only, so every font draws it; the bolt is
//! yellow and the name is in the brand color where there is one.

use crate::{
    style::{Depth, Line, Role},
    terminal::{self, Terminal},
};

/// The bolt, padded to one width with the gap before the name, so the name lines up.
#[rustfmt::skip]
const BOLT: [&str; 5] = [
    "    __  ",
    "   / /  ",
    "  / /_  ",
    " /_  /  ",
    "   //   ",
];

/// figlet's `smslant` font. Raw strings keep the backslashes as drawn.
const WORDMARK: [&str; 5] = [
    r"            ____",
    r" _  _____  / / /____ ____ ____",
    r"| |/ / _ \/ / __/ _ `/ _ `/ -_)",
    r"|___/\___/_/\__/\_,_/\_, /\__/",
    r"                    /___/",
];

/// Columns the bolt and name need; a narrower terminal would wrap them, so they are left out.
const WIDTH: usize = 40;

/// Show the wordmark on an interactive stderr wide enough for it. `--quiet` hides it.
pub fn show(terminal: Terminal) {
    if !terminal.decorates() || !fits(terminal::stderr_width()) {
        return;
    }
    let depth = Depth::detect(|name| std::env::var(name).ok());
    for line in lines(depth) {
        terminal.notice_line(line);
    }
}

/// An unknown width is assumed to fit, as a table assumes it is not width-limited.
fn fits(width: Option<usize>) -> bool {
    width.is_none_or(|width| width >= WIDTH)
}

fn lines(depth: Depth) -> Vec<Line> {
    BOLT.iter()
        .zip(WORDMARK)
        .map(|(bolt, name)| Line::default().paint(Role::Bolt, bolt).brand(depth, name))
        .chain(std::iter::once(Line::default()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wordmark_is_ascii_and_fits_its_width() {
        for (bolt, name) in BOLT.iter().zip(WORDMARK) {
            assert!(bolt.is_ascii() && bolt.len() == BOLT[0].len(), "{bolt}");
            assert!(name.is_ascii() && bolt.len() + name.len() < WIDTH, "{name}");
            assert_eq!(name, name.trim_end());
        }
    }

    #[test]
    fn a_terminal_narrower_than_the_wordmark_leaves_it_out() {
        assert!(fits(None));
        assert!(fits(Some(WIDTH)));
        assert!(!fits(Some(WIDTH - 1)));
    }

    #[test]
    fn each_line_is_painted_in_the_brand_color_and_a_blank_line_follows() {
        let lines = lines(Depth::Ansi256);
        assert_eq!(lines.len(), WORDMARK.len() + 1);
        assert_eq!(
            lines[1].as_str(),
            format!(
                "{}\u{1b}[1m\u{1b}[38;5;202m{}\u{1b}[0m",
                crate::style::paint(Role::Bolt, BOLT[1]),
                WORDMARK[1]
            )
        );
        assert_eq!(lines[WORDMARK.len()].as_str(), "");
    }
}

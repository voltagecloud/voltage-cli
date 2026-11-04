//! A BOLT11 invoice as a terminal QR code. On a terminal the code is drawn black on bright
//! white with the spec's four-module quiet zone, so a phone scans it on a dark theme as well as a
//! light one, under a caption led by the network in its website color. Redirected stderr
//! keeps the compact code it always had.

use crate::{
    Error, Result,
    output::{readable_amount, relative_time},
    payment::PaymentView,
    style::{Depth, Glyphs, Line, Role},
    terminal::Terminal,
};
use clap::builder::styling::{Ansi256Color, AnsiColor, Color, RgbColor, Style};
use qrcode::{EcLevel, QrCode, render::unicode::Dense1x2};

/// Light modules around a framed code: the four the QR spec asks for. Even, because a
/// character cell holds two modules from top to bottom.
const QUIET_MODULES: usize = 4;

/// Show `invoice` for scanning, with what `view` says about the payment. `network` is the
/// wallet's network when it was read; the invoice's prefix stands in otherwise.
pub fn show(
    invoice: &str,
    view: &PaymentView,
    network: Option<&str>,
    terminal: Terminal,
    now: i64,
) -> Result<()> {
    let code = encode(invoice)?;
    if !terminal.decorates() {
        let image = render(&code, false)
            .iter()
            .map(|line| format!("  {line}  "))
            .collect::<Vec<_>>()
            .join("\n");
        terminal.important(format!("\nScan to pay:\n\n{image}\n"));
        return Ok(());
    }
    let network = network.or_else(|| invoice_network(invoice));
    let depth = Depth::detect(|name| std::env::var(name).ok());
    terminal.important_line(Line::default());
    terminal.important_line(caption(view, network, terminal.glyphs(), depth, now));
    for line in image(&code, terminal.colors()) {
        terminal.important_line(line);
    }
    Ok(())
}

/// Whether `invoice` can be drawn, for a caller to know before it writes anything else.
pub fn check(invoice: &str) -> Result<()> {
    encode(invoice).map(|_| ())
}

/// The code as lines: framed black on bright white with color, so it scans on any theme.
/// Without color it cannot be, so it is the compact form, which follows the theme and may
/// read inverted on a dark one.
fn image(code: &QrCode, colored: bool) -> Vec<Line> {
    if colored {
        render(code, true)
            .iter()
            .map(|line| Line::default().styled(Role::QrCode, line))
            .collect()
    } else {
        render(code, false)
            .iter()
            .map(|line| Line::default().text(&format!("  {line}  ")))
            .collect()
    }
}

/// BOLT11 is case-insensitive. Uppercase enables QR alphanumeric mode, and low error
/// correction keeps the code small enough for a terminal.
fn encode(invoice: &str) -> Result<QrCode> {
    QrCode::with_error_correction_level(invoice.to_ascii_uppercase(), EcLevel::L)
        .map_err(|_| Error::transport("Could not encode the invoice as a QR code"))
}

/// Two modules per character cell. `framed` adds a quiet zone of `QUIET_MODULES`: half that
/// many blank rows above and below, and that many blank columns each side. The compact form
/// leaves it to the margin around the code.
fn render(code: &QrCode, framed: bool) -> Vec<String> {
    let lines = code
        .render::<Dense1x2>()
        .quiet_zone(false)
        .build()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !framed {
        return lines;
    }
    let side = " ".repeat(QUIET_MODULES);
    let blank = " ".repeat(code.width() + 2 * QUIET_MODULES);
    let margin = || std::iter::repeat_n(blank.clone(), QUIET_MODULES / 2);
    margin()
        .chain(lines.into_iter().map(|line| format!("{side}{line}{side}")))
        .chain(margin())
        .collect()
}

/// The network a BOLT11 prefix names. Mutinynet invoices use signet's prefix.
fn invoice_network(invoice: &str) -> Option<&'static str> {
    let invoice = invoice.to_ascii_lowercase();
    [
        ("lnbcrt", "regtest"),
        ("lntbs", "signet"),
        ("lntb", "testnet"),
        ("lnbc", "mainnet"),
    ]
    .into_iter()
    .find(|(prefix, _)| invoice.starts_with(prefix))
    .map(|(_, network)| network)
}

/// A network's color from the website's network icons, at the terminal's depth.
fn network_style(network: &str, depth: Depth) -> Option<Style> {
    let ((r, g, b), ansi256, basic) = match network {
        "mainnet" => ((0xFF, 0x73, 0x33), 209, AnsiColor::Yellow),
        "mutinynet" => ((0xAD, 0x44, 0xFF), 135, AnsiColor::Magenta),
        "signet" => ((0xA3, 0x1A, 0xF7), 129, AnsiColor::Magenta),
        "testnet" | "testnet4" => ((0x13, 0xCE, 0x66), 41, AnsiColor::Green),
        _ => return None,
    };
    let color = match depth {
        Depth::TrueColor => Color::Rgb(RgbColor(r, g, b)),
        Depth::Ansi256 => Color::Ansi256(Ansi256Color(ansi256)),
        Depth::Basic => Color::Ansi(basic),
    };
    Some(Style::new().fg_color(Some(color)))
}

/// `● network · Scan to pay` in bold, then whichever of the amount, memo, and expiry the
/// payment has. The dot takes the network's color; a network without one is not named.
fn caption(
    view: &PaymentView,
    network: Option<&str>,
    glyphs: Glyphs,
    depth: Depth,
    now: i64,
) -> Line {
    let separator = glyphs.separator;
    let mut line = Line::default();
    if let Some((network, style)) =
        network.and_then(|network| Some((network, network_style(network, depth)?)))
    {
        let dot = if glyphs == Glyphs::UNICODE {
            "●"
        } else {
            "*"
        };
        line = line
            .with_style(style, dot)
            .text(&format!(" {network} {separator} "));
    }
    line = line.styled(Role::Emphasis, "Scan to pay");
    let facts = [
        view.data.amount.as_ref().and_then(readable_amount),
        view.memo().map(str::to_owned),
        view.expires_at()
            .map(|expires| format!("expires {}", relative_time(expires, now))),
    ];
    for fact in facts.into_iter().flatten() {
        line = line.text(&format!(" {separator} {fact}"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const INVOICE: &str = "lntbs10n1p4ft36app509h8j7cecenwaktcsrshu6cx4xsczarwfj4nl4tfjq6y7rz79vmqdqqcqzzsxqrrsssp5grs2zd2al99uw93vja4mc5wus9lshcjteuqwef2xkhlp9ppmkens9qxpqysgqmn4s544p8p0263cv83s3c3qjv69z3xxy3fh64ny2y766mfnqfxrrhjj8s7h0lrkxux23dfyzxfydvgjhrsw2p8z0nw54en5a9rvk4kgqxkh73g";

    fn strip(line: &Line) -> String {
        anstream::adapter::strip_str(line.as_str()).to_string()
    }

    /// The modules a half-block cell draws in the foreground, top then bottom.
    fn modules(cell: char) -> [bool; 2] {
        match cell {
            '█' => [true, true],
            '▀' => [true, false],
            '▄' => [false, true],
            ' ' => [false, false],
            other => panic!("unexpected cell {other:?}"),
        }
    }

    #[test]
    fn a_framed_code_draws_dark_modules_in_the_foreground_inside_a_quiet_zone() {
        let code = encode(INVOICE).unwrap();
        let width = code.width();
        let dark: Vec<bool> = code
            .to_colors()
            .into_iter()
            .map(|color| color == qrcode::Color::Dark)
            .collect();
        let lines = render(&code, true);
        assert_eq!(lines[0].chars().count(), width + 2 * QUIET_MODULES);
        assert_eq!(lines.len(), width.div_ceil(2) + QUIET_MODULES);
        for (row, line) in lines.iter().enumerate() {
            for (column, cell) in line.chars().enumerate() {
                for (half, foreground) in modules(cell).into_iter().enumerate() {
                    let y = (row * 2 + half)
                        .checked_sub(QUIET_MODULES)
                        .filter(|y| *y < width);
                    let x = column.checked_sub(QUIET_MODULES).filter(|x| *x < width);
                    let expected = matches!((x, y), (Some(x), Some(y)) if dark[y * width + x]);
                    assert_eq!(
                        foreground, expected,
                        "row {row} column {column} half {half}"
                    );
                }
            }
        }
        // The compact form has no quiet zone.
        assert_eq!(render(&code, false)[0].chars().count(), width);
    }

    #[test]
    fn without_color_the_code_is_the_compact_form_and_carries_no_styles() {
        let code = encode(INVOICE).unwrap();
        let colored = image(&code, true);
        assert_eq!(colored.len(), render(&code, true).len());
        assert!(
            colored
                .iter()
                .all(|line| line.as_str().contains("\u{1b}[107m"))
        );
        let plain = image(&code, false);
        assert_eq!(plain.len(), render(&code, false).len());
        assert!(plain.iter().all(|line| !line.as_str().contains('\u{1b}')));
        assert!(check(INVOICE).is_ok());
    }

    #[test]
    fn networks_come_from_the_wallet_or_else_the_invoice_prefix() {
        assert_eq!(invoice_network("lnbc10n1x"), Some("mainnet"));
        assert_eq!(invoice_network("LNTBS10N1X"), Some("signet"));
        assert_eq!(invoice_network("lntb10n1x"), Some("testnet"));
        assert_eq!(invoice_network("lnbcrt10n1x"), Some("regtest"));
        assert_eq!(invoice_network("bitcoin:bc1q"), None);
        assert_eq!(network_style("regtest", Depth::TrueColor), None);
        for network in ["mainnet", "mutinynet", "signet", "testnet", "testnet4"] {
            assert!(network_style(network, Depth::Basic).is_some(), "{network}");
        }
        let style = |network, depth| network_style(network, depth).unwrap().to_string();
        assert_eq!(
            style("mutinynet", Depth::TrueColor),
            "\u{1b}[38;2;173;68;255m"
        );
        assert_eq!(style("mainnet", Depth::Ansi256), "\u{1b}[38;5;209m");
        assert_eq!(style("testnet", Depth::Basic), "\u{1b}[32m");
    }

    #[test]
    fn the_caption_names_the_network_and_what_the_payment_has() {
        let now = 1_792_540_800;
        let view = PaymentView::from_body(&json!({"data": {
            "amount": {"amount": 1_000_000, "currency": "btc", "unit": "msats"},
            "memo": "order\u{1b}[2J 42",
            "expiration": 3600
        }, "created_at": "2026-10-21T00:00:00Z"}));
        let full = caption(
            &view,
            Some("mutinynet"),
            Glyphs::UNICODE,
            Depth::TrueColor,
            now,
        );
        assert!(
            full.as_str().starts_with("\u{1b}[38;2;173;68;255m●"),
            "{:?}",
            full.as_str()
        );
        assert_eq!(
            strip(&full),
            "● mutinynet · Scan to pay · 1,000 sats · order [2J 42 · expires in 1 hour"
        );
        let ascii = caption(
            &PaymentView::default(),
            Some("mainnet"),
            Glyphs::ASCII,
            Depth::Basic,
            now,
        );
        assert_eq!(strip(&ascii), "* mainnet - Scan to pay");
        let unknown = caption(
            &PaymentView::default(),
            Some("regtest"),
            Glyphs::UNICODE,
            Depth::Basic,
            now,
        );
        assert_eq!(strip(&unknown), "Scan to pay");
    }
}

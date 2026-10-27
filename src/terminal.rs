//! Terminal interaction policy: prompts, progress, and diagnostics on stderr. Results and
//! errors are written by `output`.

use crate::{
    Error, Result,
    progress::{Look, Progress},
    secret::Secret,
    style::{self, ColorChoice, Glyphs, Line},
};
use crossterm::{
    Command, cursor,
    event::{self, DisableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use inquire::{InquireError, Select, Text};
use std::{
    fmt::{self, Display},
    io::{IsTerminal, Write},
    time::Duration,
};
use tokio::{sync::oneshot, time::Instant};
use uuid::Uuid;
use zeroize::Zeroizing;

/// What the process may show to and ask of the person at the terminal, from `--quiet`,
/// `--no-input`, and `--color`.
#[derive(Clone, Copy)]
pub struct Terminal {
    quiet: bool,
    no_input: bool,
    color: ColorChoice,
}

impl Terminal {
    pub fn new(quiet: bool, no_input: bool) -> Self {
        Self {
            quiet,
            no_input,
            color: ColorChoice::Auto,
        }
    }

    pub fn with_color(self, color: ColorChoice) -> Self {
        Self { color, ..self }
    }

    /// The same terminal with optional progress and notices hidden.
    pub fn quieted(self) -> Self {
        Self {
            quiet: true,
            ..self
        }
    }

    /// Status lines get symbols and color only on an interactive stderr; redirected stderr
    /// keeps plain sentences for logs.
    pub fn decorates(self) -> bool {
        std::io::stderr().is_terminal()
    }

    /// Whether styles written to stderr will show, from `--color` and the environment.
    pub fn colors(self) -> bool {
        self.color.colors_stderr()
    }

    pub fn glyphs(self) -> Glyphs {
        Glyphs::detect(|name| std::env::var(name).ok())
    }

    /// A prompt needs an interactive stdin and must not be disabled by `--no-input`.
    pub fn can_prompt(self) -> bool {
        !self.no_input && std::io::stdin().is_terminal()
    }

    /// Optional status for a human, cleared when the guard drops; no control codes on
    /// redirected stderr.
    pub fn progress(self, label: impl Display) -> Progress {
        self.status(label, None, None)
    }

    /// Show `label` while `work` runs. The status cannot outlive the work: it clears on
    /// completion, error, or cancellation.
    pub async fn during<T>(self, label: impl Display, work: impl Future<Output = T>) -> T {
        let _progress = self.progress(label);
        work.await
    }

    /// `during` for one step of a longer wait: the line shows the time since `since`.
    pub async fn waiting<T>(
        self,
        label: impl Display,
        since: Instant,
        work: impl Future<Output = T>,
    ) -> T {
        let _progress = self.status(label, Some(since), None);
        work.await
    }

    /// `waiting` for a wait that ends at `expires`: the line also counts down to it.
    pub async fn expiring<T>(
        self,
        label: impl Display,
        since: Instant,
        expires: Instant,
        work: impl Future<Output = T>,
    ) -> T {
        let _progress = self.status(label, Some(since), Some(expires));
        work.await
    }

    fn status(
        self,
        label: impl Display,
        since: Option<Instant>,
        expires: Option<Instant>,
    ) -> Progress {
        if self.quiet || !self.decorates() || !draws_control_codes() {
            return Progress::hidden();
        }
        let look = Look {
            animate: animates(|name| std::env::var(name).ok()),
            color: self.color.colors_stderr(),
            glyphs: self.glyphs(),
        };
        // Labels are fixed text today; scrubbing keeps every escape the line's own.
        Progress::show(scrub_line(&label.to_string()), since, expires, look)
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

    /// `notice` for a line with symbols or color.
    pub fn notice_line(self, line: Line) {
        if !self.quiet {
            self.important_line(line);
        }
    }

    /// `important` for a line with symbols or color.
    pub fn important_line(self, line: Line) {
        style::write_stderr(self.color, &format!("{}\n", line.as_str()));
    }

    /// A picker also draws on stderr, so it needs both streams to be interactive.
    pub fn can_pick(self) -> bool {
        self.can_prompt() && std::io::stderr().is_terminal()
    }

    /// Ask the person to pick one of a non-empty list of choices, filtering as they type.
    /// `None` means they pressed Esc. Raw mode turns a typed Ctrl-C into a key, so the picker
    /// reports Ctrl-C as an interruption.
    pub async fn pick(self, question: &'static str, choices: Vec<Choice>) -> Result<Option<Uuid>> {
        let _raw = RawMode;
        let _screen = PickerScreen;
        picked(
            read_detached(move || Select::new(question, choices).with_page_size(12).prompt())
                .await?,
        )
    }

    /// Ask for a line of text, offering `default` for an empty answer. `None` means Esc.
    pub async fn text(
        self,
        question: &'static str,
        default: &'static str,
    ) -> Result<Option<String>> {
        let _raw = RawMode;
        let _screen = PickerScreen;
        answered(read_detached(move || Text::new(question).with_default(default).prompt()).await?)
    }

    /// Read a secret without echo. The prompt goes to stderr, and raw mode hides the typed
    /// characters and turns a typed Ctrl-C into a key.
    pub async fn read_hidden(self, prompt: &'static str) -> Result<Secret> {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        // The detached read cannot be cancelled, and Ctrl-C does not wait for it. Raw mode is
        // turned on here, not on the reading thread, so this guard restores the terminal
        // whether the read completes or is cancelled.
        let raw = match RawMode::enable() {
            Ok(raw) => raw,
            Err(error) => return hidden(Err(error)),
        };
        let read = read_detached(read_hidden_keys).await?;
        drop(raw);
        eprintln!();
        hidden(read)
    }
}

/// What a hidden prompt's read means: the secret, Ctrl-C (`None`) as an interruption, and a
/// terminal that cannot be put in raw mode or read as a transport error.
fn hidden(read: std::io::Result<Option<Secret>>) -> Result<Secret> {
    match read {
        Ok(Some(secret)) => Ok(secret),
        Ok(None) => Err(Error::interrupted(
            "Interrupted before any resource change was submitted",
        )),
        Err(_) => Err(Error::transport(
            "Could not read the secret from the terminal",
        )),
    }
}

/// Keys typed at a hidden prompt, until Enter. `None` means Ctrl-C.
fn read_hidden_keys() -> std::io::Result<Option<Secret>> {
    let mut typed = Zeroizing::new(String::new());
    loop {
        if let Event::Key(key) = event::read()? {
            match type_hidden(&mut typed, key) {
                Hidden::Typing => {}
                Hidden::Entered => return Ok(Some(Secret::from(typed))),
                Hidden::Interrupted => return Ok(None),
            }
        }
    }
}

/// Where a hidden prompt stands after a key.
#[derive(Debug, PartialEq)]
enum Hidden {
    Typing,
    Entered,
    Interrupted,
}

/// Apply one key to the secret typed so far. Enter, or Ctrl-J for a pasted newline, ends the
/// read; Backspace removes a character; Ctrl-U clears the line. Releases and other control
/// keys change nothing.
fn type_hidden(typed: &mut String, key: KeyEvent) -> Hidden {
    if key.kind == KeyEventKind::Release {
        return Hidden::Typing;
    }
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if control => return Hidden::Interrupted,
        KeyCode::Enter => return Hidden::Entered,
        KeyCode::Char('j') if control => return Hidden::Entered,
        KeyCode::Char('u') if control => typed.clear(),
        KeyCode::Backspace => {
            typed.pop();
        }
        KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => typed.push(c),
        _ => {}
    }
    Hidden::Typing
}

fn picked(answer: std::result::Result<Choice, InquireError>) -> Result<Option<Uuid>> {
    Ok(answered(answer)?.map(|choice| choice.id))
}

/// What a prompt's answer means: the answer, Esc as `None`, Ctrl-C as an interruption, and
/// any other failure, such as a terminal that cannot be read, as a transport error.
fn answered<T>(answer: std::result::Result<T, InquireError>) -> Result<Option<T>> {
    match answer {
        Ok(answer) => Ok(Some(answer)),
        Err(InquireError::OperationCanceled) => Ok(None),
        Err(InquireError::OperationInterrupted) => Err(Error::interrupted(
            "Interrupted before any resource change was submitted",
        )),
        Err(_) => Err(Error::transport(
            "Could not read the selection from the terminal",
        )),
    }
}

/// The picker turns on bracketed paste and hides the cursor, and undoes both only when its
/// read returns. A signal that ends the command first leaves that to this guard.
struct PickerScreen;

impl Drop for PickerScreen {
    fn drop(&mut self) {
        let undo = format!("{}{}", ansi(DisableBracketedPaste), ansi(cursor::Show));
        let mut stderr = std::io::stderr();
        let _ = stderr.write_all(undo.as_bytes());
        let _ = stderr.flush();
    }
}

/// Raw mode, which turns off echo and terminal signals. crossterm saves the terminal's modes
/// when it turns raw mode on and restores them when it turns it off. A prompt's read restores
/// them only when it returns; a signal that ends the command first leaves that to this guard.
/// Dropping it when raw mode is already off does nothing.
struct RawMode;

impl RawMode {
    fn enable() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// A crossterm command's ANSI sequence, for output written as ordered bytes. crossterm's
/// `execute!` would instead call the Windows console API, out of order with buffered text.
pub fn ansi(command: impl Command) -> String {
    let mut sequence = String::new();
    let _ = command.write_ansi(&mut sequence);
    sequence
}

/// One resource a person can pick: its name, scrubbed because the API supplied it, and its ID.
pub struct Choice {
    id: Uuid,
    name: String,
    currency: Option<String>,
}

impl Choice {
    pub fn new(id: Uuid, name: &str) -> Self {
        Self {
            id,
            name: scrub_line(name),
            currency: None,
        }
    }

    /// Show a wallet's currency between its name and ID, where typing can filter on it.
    pub fn with_currency(mut self, currency: &str) -> Self {
        self.currency = Some(scrub_line(&currency.to_uppercase()));
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn id(&self) -> Uuid {
        self.id
    }
}

impl Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.currency {
            Some(currency) => write!(f, "{}  {currency}  {}", self.name, self.id),
            None => write!(f, "{}  {}", self.name, self.id),
        }
    }
}

/// The terminal's width in columns: `COLUMNS` when set, else the controlling terminal's
/// size while stdout is interactive. crossterm falls back to running `tput` when the size
/// query fails. `None` means output is not width-limited.
pub fn width() -> Option<usize> {
    width_when(std::io::stdout().is_terminal())
}

/// The terminal's height in rows while stdout is interactive, for a screen that redraws.
pub fn height() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    crossterm::terminal::size()
        .ok()
        .map(|(_, rows)| usize::from(rows))
        .filter(|rows| *rows > 0)
}

/// `width` for stderr, where status lines go.
pub fn stderr_width() -> Option<usize> {
    width_when(std::io::stderr().is_terminal())
}

fn width_when(interactive: bool) -> Option<usize> {
    if let Some(columns) = std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|columns| *columns > 0)
    {
        return Some(columns);
    }
    if !interactive {
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

/// How long a wait has run, for a status line that would otherwise look stuck: `42s`, or
/// `3m 05s` from a minute on.
pub fn elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

/// A spinner redraws its line; `TERM=dumb` cannot clear one, and `VOLTAGE_SPINNER=0` or
/// `false` asks for a still line, as screen readers read every redraw.
fn animates(var: impl Fn(&str) -> Option<String>) -> bool {
    let still = var("VOLTAGE_SPINNER")
        .is_some_and(|value| value == "0" || value.eq_ignore_ascii_case("false"));
    var("TERM").as_deref() != Some("dumb") && !still
}

/// A status line clears itself with control codes. A Windows console without ANSI support
/// would print them, so it gets no status line; crossterm turns ANSI on where it can.
fn draws_control_codes() -> bool {
    #[cfg(windows)]
    {
        crossterm::ansi_support::supports_ansi()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind as Exit;

    #[test]
    fn a_dumb_terminal_or_voltage_spinner_0_keeps_progress_still() {
        let animates_with = |vars: &[(&str, &str)]| {
            animates(|name| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            })
        };
        assert!(animates_with(&[]));
        assert!(animates_with(&[
            ("TERM", "xterm"),
            ("VOLTAGE_SPINNER", "1")
        ]));
        assert!(!animates_with(&[("TERM", "dumb")]));
        assert!(!animates_with(&[("VOLTAGE_SPINNER", "0")]));
        assert!(!animates_with(&[("VOLTAGE_SPINNER", "FALSE")]));
        assert!(animates_with(&[("VOLTAGE_SPINNER", "off")]));
    }

    #[test]
    fn a_picker_answer_becomes_a_choice_nothing_an_interruption_or_a_transport_error() {
        let id = Uuid::nil();
        assert_eq!(picked(Ok(Choice::new(id, "a"))).unwrap(), Some(id));
        assert_eq!(picked(Err(InquireError::OperationCanceled)).unwrap(), None);
        let interrupted = picked(Err(InquireError::OperationInterrupted)).unwrap_err();
        assert_eq!(interrupted.kind, Exit::Interrupted);
        // A terminal that goes away or is not one fails the read; no choice is assumed.
        for failure in [
            InquireError::NotTTY,
            InquireError::IO(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        ] {
            let error = picked(Err(failure)).unwrap_err();
            assert_eq!(error.kind, Exit::Transport);
            assert_eq!(
                error.message,
                "Could not read the selection from the terminal"
            );
        }
    }

    #[test]
    fn a_hidden_prompt_edits_the_secret_until_enter_or_ctrl_c() {
        let press = |code, modifiers| KeyEvent::new(code, modifiers);
        let none = KeyModifiers::NONE;
        let mut typed = String::new();
        for key in [
            press(KeyCode::Char('x'), none),
            press(KeyCode::Char('u'), KeyModifiers::CONTROL),
            press(KeyCode::Char('a'), none),
            press(KeyCode::Char('B'), KeyModifiers::SHIFT),
            press(KeyCode::Char('z'), none),
            press(KeyCode::Backspace, none),
            press(KeyCode::Char('c'), none),
            // Alt and control chords, other keys, and releases type nothing.
            press(KeyCode::Char('d'), KeyModifiers::ALT),
            press(KeyCode::Char('e'), KeyModifiers::CONTROL),
            press(KeyCode::Left, none),
            KeyEvent::new_with_kind(KeyCode::Char('f'), none, KeyEventKind::Release),
        ] {
            assert_eq!(type_hidden(&mut typed, key), Hidden::Typing, "{key:?}");
        }
        assert_eq!(typed, "aBc");
        assert_eq!(
            type_hidden(&mut typed, press(KeyCode::Enter, none)),
            Hidden::Entered
        );
        // A pasted newline reaches a raw terminal as Ctrl-J.
        assert_eq!(
            type_hidden(&mut typed, press(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Hidden::Entered
        );
        assert_eq!(
            type_hidden(&mut typed, press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Hidden::Interrupted
        );
        assert_eq!(typed, "aBc");
    }

    #[test]
    fn a_hidden_read_becomes_the_secret_an_interruption_or_a_transport_error() {
        assert_eq!(
            hidden(Ok(Some(Secret::new("k".into())))).unwrap().expose(),
            "k"
        );
        let interrupted = hidden(Ok(None)).unwrap_err();
        assert_eq!(interrupted.kind, Exit::Interrupted);
        assert_eq!(
            interrupted.message,
            "Interrupted before any resource change was submitted"
        );
        // No terminal to put in raw mode, or one that goes away mid-read; no secret is assumed.
        let error =
            hidden(Err(std::io::Error::from(std::io::ErrorKind::NotConnected))).unwrap_err();
        assert_eq!(error.kind, Exit::Transport);
        assert_eq!(error.message, "Could not read the secret from the terminal");
    }

    #[test]
    fn crossterm_commands_become_their_ansi_sequences() {
        assert_eq!(ansi(DisableBracketedPaste), "\x1b[?2004l");
        assert_eq!(ansi(cursor::Show), "\x1b[?25h");
        assert_eq!(
            ansi(terminal::Clear(terminal::ClearType::CurrentLine)),
            "\x1b[2K"
        );
    }

    #[test]
    fn elapsed_time_reads_in_seconds_then_minutes() {
        assert_eq!(elapsed(Duration::from_millis(900)), "0s");
        assert_eq!(elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(elapsed(Duration::from_secs(60)), "1m 00s");
        assert_eq!(elapsed(Duration::from_secs(185)), "3m 05s");
    }

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

//! Prefilled GitHub issue links, for `voltage feedback` and after a panic.
//!
//! A report carries only what helps triage and cannot identify an account: the build, the
//! platform, the command's words and flag names (never flag values), and a scrubbed panic.
//! Scrubbing replaces any word that looks like an ID, key, token, invoice, address, email, or
//! URL, so text that echoes the API or a flag value cannot leak a secret into a public issue.

use crate::{
    cli::{FeedbackFlags, outline},
    output::REDACTED,
    style::{Line, Role},
    terminal::scrub_line,
};
use serde::Serialize;
use std::{
    io::IsTerminal,
    panic::{Location, PanicHookInfo},
    path::Path,
};
use url::form_urlencoded;

const REPOSITORY: &str = "https://github.com/voltagecloud/voltage-cli";
/// GitHub answers longer `issues/new` URLs with an error, and terminals wrap them badly.
const MAX_URL: usize = 6000;
const MAX_TITLE: usize = 120;
const MAX_PROBLEM: usize = 500;
const MAX_COMMAND: usize = 300;
/// A shorter word with a digit is a count, a status, or a version, not a credential.
const MIN_SECRET_WITH_DIGIT: usize = 16;
/// A word this long is redacted even without a digit, such as a hex or base64 secret.
const MIN_SECRET: usize = 32;

/// Who is filing. Only a person at a terminal is a person; a process without a terminal or
/// one that asked for JSON is almost always an agent or a script, so its report says so.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reporter {
    Person,
    Agent,
}

impl Reporter {
    pub fn detect(interactive: bool) -> Self {
        if interactive {
            Self::Person
        } else {
            Self::Agent
        }
    }
}

/// `voltage feedback`'s result: the prefilled issue, and whether a browser opened it.
#[derive(Debug, Serialize)]
pub struct Feedback {
    #[serde(flatten)]
    pub report: IssueReport,
    pub opened: bool,
}

/// The report for `voltage feedback`. Only a person at a terminal who did not pass
/// `--no-browser` gets a browser; an agent or script gets the link alone.
pub fn run(flags: &FeedbackFlags, interactive: bool) -> Feedback {
    let report = Draft {
        command: flags.command_outline(),
        title: flags.title.clone(),
        panic: None,
        reporter: Reporter::detect(interactive),
    }
    .into_report();
    let opened = interactive && !flags.no_browser && webbrowser::open(&report.url).is_ok();
    Feedback { report, opened }
}

impl Feedback {
    /// On a terminal, what a person reads instead of the report's fields: where to search
    /// first, then the link that files the issue. The prefilled link wraps across many lines,
    /// so it is printed only when no browser opened it.
    pub fn lines(&self, step: &'static str) -> Vec<Line> {
        let file = Line::default().paint(Role::Hint, step);
        let file = if self.opened {
            file.text(" Opened the prefilled issue in your browser")
        } else {
            file.text(" File the issue: ").link(&self.report.url)
        };
        vec![
            Line::default()
                .paint(Role::Hint, step)
                .text(" Search existing issues first: ")
                .link(&self.report.search_url),
            file,
        ]
    }
}

/// What a report says before it becomes a link.
#[derive(Debug)]
pub struct Draft {
    /// The command's words and flag names, such as `voltage wallets get --json`.
    pub command: Option<String>,
    pub title: Option<String>,
    pub panic: Option<Panic>,
    pub reporter: Reporter,
}

/// Where the CLI panicked, and its message, which is scrubbed when the report is built.
#[derive(Debug)]
pub struct Panic {
    /// `file:line` in the CLI's source, or a dependency's file name.
    pub at: Option<String>,
    pub message: String,
}

impl Panic {
    /// The CLI's own files are relative paths. A dependency's is absolute and can name the
    /// builder's home directory, so only its file name is kept.
    fn new(location: Option<&Location<'_>>, message: &str) -> Self {
        Self {
            at: location
                .map(|location| format!("{}:{}", source_file(location.file()), location.line())),
            message: message.to_owned(),
        }
    }

    fn of(info: &PanicHookInfo<'_>) -> Self {
        Self::new(info.location(), info.payload_as_str().unwrap_or_default())
    }

    fn describe(&self) -> String {
        let message = scrub(&self.message, MAX_PROBLEM);
        match &self.at {
            Some(at) => format!("voltage panicked at {at}: {message}"),
            None => format!("voltage panicked: {message}"),
        }
    }
}

/// A prefilled issue: the link opens GitHub's new-issue form with these fields filled in.
#[derive(Debug, Serialize)]
pub struct IssueReport {
    pub title: String,
    pub labels: Vec<&'static str>,
    pub reporter: Reporter,
    pub body: String,
    pub url: String,
    /// Existing issues to search before filing another.
    pub search_url: String,
}

impl Draft {
    pub fn into_report(self) -> IssueReport {
        let title = self
            .title
            .as_deref()
            .map(|title| scrub(title, MAX_TITLE))
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "Describe the problem in one line".into());
        let labels = vec!["bug"];
        let search_url = format!("{REPOSITORY}/issues?q=is%3Aissue");
        let mut body = self.body(&search_url);
        let mut url = new_issue_url(&title, &body, &labels);
        // Every part is already capped, so this only trims a report that is still too long.
        while url.len() > MAX_URL {
            let keep = body.chars().count() * 9 / 10;
            body = format!("{}…", body.chars().take(keep).collect::<String>());
            url = new_issue_url(&title, &body, &labels);
        }
        IssueReport {
            title,
            labels,
            reporter: self.reporter,
            body,
            url,
            search_url,
        }
    }

    fn body(&self, search_url: &str) -> String {
        let problem = self.panic.as_ref().map_or_else(
            || "What you expected, and what happened instead.".into(),
            Panic::describe,
        );
        let command = self.command.as_deref().map_or_else(
            || "not given".into(),
            |command| format!("`{}`", scrub(command, MAX_COMMAND)),
        );
        // A person's link is printed on a terminal, so only an agent's report carries the
        // instructions a person already read there.
        let (agent, preface) = match self.reporter {
            Reporter::Agent => (
                "x",
                "<!-- Search existing issues first and add to a matching one instead of filing \
                 another. Agents: keep the box below checked, and give steps a person can run. \
                 Replace IDs, invoices, addresses, and keys with placeholders such as \
                 WALLET_ID. -->\n\n",
            ),
            Reporter::Person => (" ", ""),
        };
        format!(
            "{preface}### What happened

{problem}

### Steps to reproduce

The commands to run, with placeholders such as `WALLET_ID` for IDs.

### Environment

- Version: voltage {version}
- Platform: {os} {arch}
- Command: {command}

### Checks

- [ ] I searched existing issues: {search_url}
- [{agent}] An AI agent wrote this report
",
            version = env!("VOLTAGE_VERSION"),
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
        )
    }
}

fn new_issue_url(title: &str, body: &str, labels: &[&str]) -> String {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("title", title)
        .append_pair("body", body)
        .append_pair("labels", &labels.join(","))
        .finish();
    format!("{REPOSITORY}/issues/new?{query}")
}

/// Text for a public issue: one line, no word that could be a secret or identify an account,
/// and at most `limit` characters.
pub fn scrub(text: &str, limit: usize) -> String {
    let line = scrub_line(text);
    let mut scrubbed = String::with_capacity(line.len());
    let mut word = String::new();
    for c in line.chars() {
        if is_word(c) {
            word.push(c);
        } else {
            push_word(&mut scrubbed, &mut word);
            scrubbed.push(c);
        }
    }
    push_word(&mut scrubbed, &mut word);
    if scrubbed.chars().count() > limit {
        scrubbed = scrubbed.chars().take(limit).collect();
        scrubbed.push('…');
    }
    scrubbed
}

/// Characters that IDs, keys, invoices, addresses, emails, and URLs are made of.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || "-_./:+=@%~?&#".contains(c)
}

fn push_word(scrubbed: &mut String, word: &mut String) {
    let length = word.chars().count();
    let secret = word.contains('@')
        || word.contains("://")
        || length >= MIN_SECRET
        || (length >= MIN_SECRET_WITH_DIGIT && word.chars().any(|c| c.is_ascii_digit()));
    scrubbed.push_str(if secret { REDACTED } else { word });
    word.clear();
}

/// After the default panic report, print a link that files the panic as a bug. The command's
/// words are read when it panics, so a run that never panics never builds the command tree.
pub fn report_panics(reporter: Reporter) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let args: Vec<_> = std::env::args_os().collect();
        let report = Draft {
            command: Some(outline(&args)),
            title: Some("voltage panicked".into()),
            panic: Some(Panic::of(info)),
            reporter,
        }
        .into_report();
        eprintln!("This is a bug in voltage. Report it: {}", report.url);
    }));
}

fn source_file(file: &str) -> std::borrow::Cow<'_, str> {
    let path = Path::new(file);
    if path.is_absolute() {
        path.file_name().unwrap_or_default().to_string_lossy()
    } else {
        file.into()
    }
}

/// A panic's report marks an agent the same way `voltage feedback` does.
pub fn interactive(json: bool) -> bool {
    !json && std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn query(url: &str) -> HashMap<String, String> {
        let url = url::Url::parse(url).unwrap();
        url.query_pairs().into_owned().collect()
    }

    fn draft(message: Option<&str>, reporter: Reporter) -> Draft {
        Draft {
            command: Some("voltage wallets get --json".into()),
            title: Some("Balance looks wrong".into()),
            panic: message.map(|message| Panic::new(None, message)),
            reporter,
        }
    }

    #[test]
    fn a_report_links_to_a_prefilled_new_issue() {
        let report = draft(Some("index out of bounds"), Reporter::Person).into_report();
        assert!(
            report
                .url
                .starts_with("https://github.com/voltagecloud/voltage-cli/issues/new?")
        );
        let fields = query(&report.url);
        assert_eq!(fields["title"], "Balance looks wrong");
        assert_eq!(fields["labels"], "bug");
        assert_eq!(fields["body"], report.body);
        for expected in [
            "voltage panicked: index out of bounds",
            concat!("- Version: voltage ", env!("VOLTAGE_VERSION")),
            &format!(
                "- Platform: {} {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            "- Command: `voltage wallets get --json`",
            "- [ ] I searched existing issues: https://github.com/voltagecloud/voltage-cli/issues?q=is%3Aissue",
            "- [ ] An AI agent wrote this report",
        ] {
            assert!(
                report.body.contains(expected),
                "{expected}\n{}",
                report.body
            );
        }
        // A person read the instructions on the terminal, so the link leaves them out.
        assert!(!report.body.contains("<!--"), "{}", report.body);
        // The URL is fully encoded: no raw space, newline, or `#` that would end it early.
        assert!(!report.url.contains([' ', '\n', '#']), "{}", report.url);
    }

    #[test]
    fn an_agent_report_is_marked_and_asks_to_search_first() {
        let report = draft(None, Reporter::Agent).into_report();
        assert!(report.body.contains("- [x] An AI agent wrote this report"));
        assert!(report.body.contains("Search existing issues first"));
        assert!(report.body.contains("What you expected"));
        assert_eq!(report.reporter, Reporter::Agent);
        assert_eq!(
            serde_json::to_value(Reporter::Agent).unwrap(),
            serde_json::json!("agent")
        );
        assert_eq!(Reporter::detect(false), Reporter::Agent);
        assert_eq!(Reporter::detect(true), Reporter::Person);
    }

    #[test]
    fn a_report_without_a_title_or_command_asks_for_them() {
        let report = Draft {
            command: None,
            title: Some("  ".into()),
            panic: None,
            reporter: Reporter::Person,
        }
        .into_report();
        assert_eq!(report.title, "Describe the problem in one line");
        assert!(report.body.contains("- Command: not given"));
    }

    fn flags(no_browser: bool) -> FeedbackFlags {
        FeedbackFlags {
            title: Some("Broken".into()),
            no_browser,
            command: vec!["wallets".into(), "list".into(), "--json".into()],
        }
    }

    #[test]
    fn feedback_never_opens_a_browser_for_an_agent_or_with_no_browser() {
        let agent = run(&flags(false), false);
        assert!(!agent.opened);
        assert_eq!(agent.report.reporter, Reporter::Agent);
        assert!(
            agent
                .report
                .body
                .contains("- Command: `voltage wallets list --json`")
        );
        let person = run(&flags(true), true);
        assert!(!person.opened);
        assert_eq!(person.report.reporter, Reporter::Person);
        let json = serde_json::to_value(&person).unwrap();
        for field in [
            "title",
            "labels",
            "reporter",
            "body",
            "url",
            "search_url",
            "opened",
        ] {
            assert!(json.get(field).is_some(), "{field}: {json}");
        }
    }

    #[test]
    fn a_terminal_shows_where_to_search_then_where_to_file() {
        let mut filed = run(&flags(true), false);
        let lines = |filed: &Feedback| {
            filed
                .lines(">")
                .iter()
                .map(|line| anstream::adapter::strip_str(line.as_str()).to_string())
                .collect::<Vec<_>>()
        };
        let shown = lines(&filed);
        assert!(shown[0].starts_with("> Search existing issues first: https://"));
        assert!(shown[1].starts_with("> File the issue: https://"));
        filed.opened = true;
        assert_eq!(
            lines(&filed)[1],
            "> Opened the prefilled issue in your browser"
        );
    }

    #[test]
    fn scrubbing_removes_ids_keys_invoices_emails_and_urls() {
        let text = "wallet 6f1c2a9e-1b2d-4c3e-8f40-0a1b2c3d4e5f key vltg_sk_1234567890abcdef \
                    invoice lnbc2500n1pj9xyzabcdefghijklmnop from me@example.com at \
                    https://api.voltage.cloud/x token abcdefabcdefabcdefabcdefabcdefabcd";
        let scrubbed = scrub(text, 1000);
        assert_eq!(
            scrubbed,
            "wallet [REDACTED] key [REDACTED] invoice [REDACTED] from [REDACTED] at [REDACTED] \
             token [REDACTED]"
        );
        // Ordinary words, statuses, and short numbers stay readable.
        assert_eq!(
            scrub("HTTP 404: wallet not found (exit code 1)", 1000),
            "HTTP 404: wallet not found (exit code 1)"
        );
        // Control characters cannot reach the terminal or the issue.
        assert_eq!(scrub("a\nb\u{1b}[31mc", 1000), "a b [31mc");
    }

    #[test]
    fn every_part_of_a_report_is_capped() {
        assert_eq!(scrub(&"word ".repeat(100), 10), "word word …");
        let long = "too long ".repeat(10_000);
        let report = Draft {
            command: Some(long.clone()),
            title: Some(long.clone()),
            panic: Some(Panic::new(None, &long)),
            reporter: Reporter::Agent,
        }
        .into_report();
        assert!(report.url.len() <= MAX_URL, "{}", report.url.len());
        assert_eq!(report.title.chars().count(), MAX_TITLE + 1);
        // Within the caps the whole body still fits, checks and all.
        assert!(report.body.contains("An AI agent wrote this report"));
    }

    #[test]
    fn a_body_too_long_for_a_link_is_trimmed_until_it_fits() {
        // Multibyte text encodes to nine bytes a character, so the capped parts overflow.
        let report = Draft {
            command: Some("€".repeat(10_000)),
            title: Some("€".repeat(10_000)),
            panic: Some(Panic::new(None, &"€".repeat(10_000))),
            reporter: Reporter::Person,
        }
        .into_report();
        assert!(report.url.len() <= MAX_URL, "{}", report.url.len());
        assert!(report.body.ends_with('…'));
        assert_eq!(query(&report.url)["body"], report.body);
    }

    #[test]
    fn a_panic_names_its_source_line_and_a_scrubbed_message() {
        let location = Location::caller();
        let panic = Panic::new(
            Some(location),
            "called `Result::unwrap()` on wallet 6f1c2a9e-1b2d-4c3e-8f40-0a1b2c3d4e5f",
        );
        // The source line survives scrubbing, which would take it for an ID.
        assert_eq!(
            panic.describe(),
            format!(
                "voltage panicked at {}:{}: called `Result::unwrap()` on wallet [REDACTED]",
                location.file(),
                location.line()
            )
        );
        assert_eq!(
            Panic::new(None, "boom").describe(),
            "voltage panicked: boom"
        );
        // A dependency's absolute path keeps only its file name, never the home directory.
        let home = std::env::temp_dir()
            .join("someone")
            .join("registry")
            .join("lib.rs");
        assert_eq!(source_file(&home.to_string_lossy()), "lib.rs");
        assert_eq!(source_file("src/api.rs"), "src/api.rs");
    }
}

//! What bare `voltage` shows on a terminal: the wordmark, who is signed in, the saved scope,
//! and a few commands to start with. It reads only the configuration, never the network. Off
//! a terminal, bare `voltage` still prints its usage and fails, as scripts expect.

use crate::{
    Error, Result, auth,
    cli::GlobalFlags,
    config::{self, Settings},
    context,
    style::{Glyphs, Line, Role},
    wordmark,
};
use serde_json::Value;
use std::{ffi::OsString, io::IsTerminal};

/// `voltage` with no command, perhaps with global flags such as `--profile`, on an interactive
/// stdout and stderr and without machine-readable output: those flags, for the greeting.
pub fn wanted(args: &[OsString]) -> Option<GlobalFlags> {
    if !std::io::stdout().is_terminal() || !std::io::stderr().is_terminal() {
        return None;
    }
    GlobalFlags::without_command(args)
        .filter(|global| !global.output_format().is_machine_readable())
}

pub fn show(global: &GlobalFlags) -> Result<()> {
    let terminal = global.terminal();
    let settings = Settings::open(config::directory(global.config_dir.clone())?)?;
    let context = serde_json::to_value(context::inspect(&settings, global)?)?;
    let email = context
        .pointer("/account/value")
        .and_then(Value::as_str)
        .and_then(|name| settings.config.accounts.get(name))
        .and_then(|account| account.email.clone());
    // Why commands would reject the credential, in the words and next step they would use.
    let (scope, _) = settings.sourced_scope(global.scope_selection())?;
    let problem = auth::select(&settings, &scope, global)
        .err()
        .map(|rejected| rejected.problem.error(&scope, &settings));
    wordmark::show(terminal);
    for line in lines(
        &context,
        email.as_deref(),
        problem.as_ref(),
        terminal.glyphs(),
    ) {
        terminal.important_line(line);
    }
    Ok(())
}

/// Who is signed in, the saved scope, and what to try, from `voltage context`'s data. A
/// credential `problem` leads instead, with its own next step first.
fn lines(
    context: &Value,
    email: Option<&str>,
    problem: Option<&Error>,
    glyphs: Glyphs,
) -> Vec<Line> {
    let setting = |name: &str| {
        context
            .pointer(&format!("/{name}/value"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let mut lines = Vec::new();
    lines.push(match (problem, setting("account"), email) {
        (Some(problem), ..) => Line::default()
            .paint(Role::Caution, "!")
            .text(&format!(" {}", problem.message)),
        (None, Some(account), Some(email)) => Line::default()
            .paint(Role::Success, glyphs.done)
            .text(" Signed in as ")
            .styled(Role::Emphasis, email)
            .text(&format!(" (account: {account})")),
        (None, Some(account), None) => Line::default()
            .paint(Role::Success, glyphs.done)
            .text(" Signed in with account ")
            .styled(Role::Emphasis, &account),
        (None, None, _) => Line::default()
            .paint(Role::Success, glyphs.done)
            .text(" Using the API key in VOLTAGE_API_KEY"),
    });
    for (label, name) in [
        ("Profile       ", "profile"),
        ("Organization  ", "organization_id"),
        ("Wallet        ", "wallet_id"),
    ] {
        if let Some(value) = setting(name) {
            lines.push(Line::default().paint(Role::Muted, label).text(&value));
        }
    }
    lines.push(Line::default());
    lines.push(Line::default().styled(Role::Emphasis, "Try"));
    let steps: Vec<String> = match problem {
        Some(problem) => problem
            .hint
            .iter()
            .map(|hint| hint.to_string())
            .chain(["voltage context".into(), "voltage --help".into()])
            .collect(),
        None => [
            "voltage wallets list",
            "voltage payments list --watch",
            "voltage --help",
        ]
        .map(str::to_owned)
        .into(),
    };
    for step in steps {
        lines.push(
            Line::default()
                .paint(Role::Hint, glyphs.step)
                .text(&format!(" {step}")),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CredentialProblem, Scope};
    use serde_json::json;

    fn shown(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| anstream::adapter::strip_str(line.as_str()).to_string())
            .collect()
    }

    #[test]
    fn a_signed_in_person_sees_who_and_where_then_what_to_try() {
        let context = json!({
            "account": {"value": "me", "source": "flag"},
            "credential": {"kind": "user", "location": "file"},
            "credential_problem": null,
            "profile": {"value": "staging", "source": "default"},
            "organization_id": {"value": "org-1", "source": "profile"},
            "wallet_id": {"value": null, "source": "unset"}
        });
        assert_eq!(
            shown(&lines(
                &context,
                Some("you@example.com"),
                None,
                Glyphs::UNICODE
            )),
            [
                "✓ Signed in as you@example.com (account: me)",
                "Profile       staging",
                "Organization  org-1",
                "",
                "Try",
                "› voltage wallets list",
                "› voltage payments list --watch",
                "› voltage --help",
            ]
        );
        let unnamed = shown(&lines(&context, None, None, Glyphs::ASCII));
        assert_eq!(unnamed[0], "ok Signed in with account me");
    }

    fn problem(problem: CredentialProblem) -> Error {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::open(dir.path().to_path_buf()).unwrap();
        problem.error(&Scope::default(), &settings)
    }

    #[test]
    fn a_credential_problem_leads_with_its_own_message_and_next_step() {
        let context = json!({"account": {"value": null}, "credential": null});
        let none = shown(&lines(
            &context,
            None,
            Some(&problem(CredentialProblem::NoCredential)),
            Glyphs::UNICODE,
        ));
        assert_eq!(none[0], "! No credential is available");
        assert!(
            none[3].starts_with("› Sign in with `voltage login`"),
            "{none:?}"
        );
        // A blank VOLTAGE_API_KEY is fixed by unsetting it, which `voltage login` cannot do.
        let empty = shown(&lines(
            &context,
            None,
            Some(&problem(CredentialProblem::EmptyApiKey)),
            Glyphs::UNICODE,
        ));
        assert_eq!(empty[0], "! VOLTAGE_API_KEY is empty");
        assert_eq!(
            empty[3],
            "› Unset VOLTAGE_API_KEY to use a saved credential, or set it to a key."
        );
        // Several saved credentials and no default: choose one, not sign in again.
        let several = shown(&lines(
            &context,
            None,
            Some(&problem(CredentialProblem::MultipleCredentials)),
            Glyphs::UNICODE,
        ));
        assert_eq!(several[0], "! Multiple credentials are saved");
        assert!(
            several[3].contains("--account NAME or --profile NAME"),
            "{several:?}"
        );
        assert_eq!(&several[4..], ["› voltage context", "› voltage --help"]);
        let key = json!({"account": {"value": null}, "credential": {"kind": "api_key"}});
        assert_eq!(
            shown(&lines(&key, None, None, Glyphs::UNICODE))[0],
            "✓ Using the API key in VOLTAGE_API_KEY"
        );
    }

    #[test]
    fn every_suggestion_is_a_valid_command() {
        for command in [
            "voltage wallets list",
            "voltage login",
            "voltage payments list --watch",
        ] {
            let args: Vec<&str> = command.split(' ').collect();
            assert!(
                crate::cli::command().try_get_matches_from(args).is_ok(),
                "{command}"
            );
        }
    }
}

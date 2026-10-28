//! `voltage context`: the effective scope, credential selection, and configuration location,
//! each with where it came from.
//!
//! Inspection reads `config.toml` and the environment only. It never reads a saved secret,
//! makes a network request, or creates the configuration directory.

use crate::{
    Result,
    auth::{self, Rejected, Selected},
    cli::GlobalFlags,
    config::{
        self, API_URL, AUTH_URL, AccountKind, CredentialProblem, CredentialStore, IgnoredVariable,
        Precedence, ScopeSources, Settings, Source,
    },
    style::{Line, Role},
};
use serde::Serialize;
use uuid::Uuid;

/// One effective setting and its source.
#[derive(Serialize)]
struct Setting<T> {
    value: T,
    source: Source,
}

/// Where the selected credential's secret would be read from.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum CredentialLocation {
    Keychain,
    File,
    /// `VOLTAGE_API_KEY`.
    Environment,
}

impl From<CredentialStore> for CredentialLocation {
    fn from(store: CredentialStore) -> Self {
        match store {
            CredentialStore::Keychain => Self::Keychain,
            CredentialStore::File => Self::File,
        }
    }
}

/// Non-secret facts about the credential a command would use.
#[derive(Serialize)]
struct CredentialSummary {
    kind: AccountKind,
    location: CredentialLocation,
}

impl From<&Selected<'_>> for CredentialSummary {
    fn from(selected: &Selected<'_>) -> Self {
        match selected {
            Selected::Ambient(_) => Self {
                kind: AccountKind::ApiKey,
                location: CredentialLocation::Environment,
            },
            Selected::Saved { account, .. } => Self {
                kind: account.kind,
                location: account.store.into(),
            },
        }
    }
}

#[derive(Serialize)]
struct ConfigFile {
    path: String,
    exists: bool,
}

/// What `voltage context` reports.
#[derive(Serialize)]
pub struct Context {
    profile: Setting<Option<String>>,
    account: Setting<Option<String>>,
    /// `None` when no credential is selected or the selected name is not saved.
    credential: Option<CredentialSummary>,
    /// Why commands would reject the selection, if they would.
    credential_problem: Option<CredentialProblem>,
    organization_id: Setting<Option<Uuid>>,
    environment_ids: Setting<Vec<Uuid>>,
    wallet_id: Setting<Option<Uuid>>,
    webhook_id: Setting<Option<Uuid>>,
    api_url: Setting<String>,
    /// Where a browser login signs in and refreshes. A selected browser login always uses the
    /// URL it was saved with (sourced like the credential); a different `--auth-url` is the
    /// `auth_url_mismatch` credential problem. An API key never uses the auth service, so
    /// otherwise it is the normalized flag, else the built-in default.
    auth_url: Setting<String>,
    config_dir: Setting<String>,
    config_file: ConfigFile,
    /// Whether changes are sent without `--execute`, from `VOLTAGE_EXECUTE`, so an exported
    /// variable cannot silently replace the describe-by-default behavior.
    execute_changes: Setting<bool>,
    /// `VOLTAGE_*` variables that are set but supply nothing, because a flag or the selected
    /// profile takes precedence.
    ignored_variables: Vec<&'static str>,
}

/// Every `VOLTAGE_*` variable that is set but supplies nothing, because a flag or the selected
/// profile takes precedence. Commands warn with this list and `voltage context` reports it, so
/// the two cannot disagree.
pub(crate) fn ignored_variables(
    global: &GlobalFlags,
    sources: &ScopeSources,
    config_dir: Source,
) -> Vec<IgnoredVariable> {
    let mut ignored = config::ignored_variables(sources, config_dir);
    ignored.extend(auth::ignored_api_key(global));
    ignored
}

/// Warn on stderr about each ignored variable. Another tool can read the same variables with
/// the opposite precedence, so a silent override can send two tools in one shell to different
/// wallets. `--quiet` keeps the warning for that reason; stdout is unchanged.
pub(crate) fn warn_ignored(global: &GlobalFlags, ignored: &[IgnoredVariable]) {
    let terminal = global.terminal();
    for variable in ignored {
        terminal.important_line(ignored_warning(variable, global.profile.as_deref()));
    }
}

/// `warning: VOLTAGE_WALLET_ID is set but ignored because --profile staging is selected`.
fn ignored_warning(variable: &IgnoredVariable, profile: Option<&str>) -> Line {
    let line = Line::default()
        .paint(Role::Caution, "warning:")
        .text(&format!(" {} is set but ignored because ", variable.name));
    match (variable.precedence, profile) {
        (Precedence::Profile, Some(profile)) => {
            line.text("--profile ").text(profile).text(" is selected")
        }
        // Only a selected profile yields `Profile`; this keeps the sentence whole regardless.
        (Precedence::Profile, None) => line.text("a profile is selected"),
        (Precedence::Flag(flag), _) => line.text(flag).text(" is given"),
    }
}

/// Resolve the context with the same precedence every command uses.
pub fn inspect(settings: &Settings, global: &GlobalFlags) -> Result<Context> {
    let (scope, sources) = settings.sourced_scope(global.scope_selection())?;
    let (_, config_dir_source) = config::sourced_directory(global.config_dir.clone())?;
    let ignored_variables = ignored_variables(global, &sources, config_dir_source)
        .into_iter()
        .map(|ignored| ignored.name)
        .collect();
    let (selected, credential_problem) = match auth::select(settings, &scope, global) {
        Ok(selected) => (Some(selected), None),
        Err(Rejected { problem, selected }) => (selected, Some(problem)),
    };
    let account = match &selected {
        Some(Selected::Ambient(_)) => Setting {
            value: None,
            source: Source::Environment,
        },
        Some(Selected::Saved { name, .. }) => Setting {
            value: Some(name.clone()),
            source: if scope.account.is_some() {
                sources.account
            } else {
                Source::Default
            },
        },
        // An unknown name was still selected; report it with its source.
        None => Setting {
            value: scope.account.clone(),
            source: sources.account,
        },
    };
    let credential = selected.as_ref().map(CredentialSummary::from);
    let flag = global.auth_url.as_deref().map(auth::base_url).transpose()?;
    let auth_url = match (&selected, flag) {
        (Some(Selected::Saved { account: saved, .. }), _) if saved.kind == AccountKind::User => {
            Setting {
                value: saved.auth_url.clone(),
                source: account.source,
            }
        }
        (_, Some(url)) => Setting {
            value: url,
            source: Source::Flag,
        },
        (_, None) => Setting {
            value: AUTH_URL.to_owned(),
            source: Source::Default,
        },
    };
    let config_file = settings.config_file();
    Ok(Context {
        profile: Setting {
            source: if global.profile.is_some() {
                Source::Flag
            } else {
                Source::Unset
            },
            value: global.profile.clone(),
        },
        account,
        credential,
        credential_problem,
        organization_id: Setting {
            value: scope.org,
            source: sources.org,
        },
        environment_ids: Setting {
            value: scope.envs,
            source: sources.envs,
        },
        wallet_id: Setting {
            value: scope.wallet,
            source: sources.wallet,
        },
        webhook_id: Setting {
            value: scope.webhook,
            source: sources.webhook,
        },
        api_url: match &global.api_url {
            Some(url) => Setting {
                value: url.clone(),
                source: Source::Flag,
            },
            None => Setting {
                value: API_URL.to_owned(),
                source: Source::Default,
            },
        },
        auth_url,
        config_dir: Setting {
            value: settings.dir.display().to_string(),
            source: config_dir_source,
        },
        config_file: ConfigFile {
            exists: config_file.exists(),
            path: config_file.display().to_string(),
        },
        execute_changes: match config::execute_variable()? {
            Some(value) => Setting {
                value,
                source: Source::Environment,
            },
            None => Setting {
                value: false,
                source: Source::Default,
            },
        },
        ignored_variables,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(name: &'static str, precedence: Precedence, profile: Option<&str>) -> String {
        let ignored = IgnoredVariable { name, precedence };
        anstream::adapter::strip_str(ignored_warning(&ignored, profile).as_str()).to_string()
    }

    #[test]
    fn a_warning_names_the_variable_and_what_took_its_place() {
        assert_eq!(
            shown("VOLTAGE_WALLET_ID", Precedence::Profile, Some("staging")),
            "warning: VOLTAGE_WALLET_ID is set but ignored because --profile staging is selected"
        );
        assert_eq!(
            shown("VOLTAGE_WALLET_ID", Precedence::Flag("--wallet"), None),
            "warning: VOLTAGE_WALLET_ID is set but ignored because --wallet is given"
        );
        assert_eq!(
            shown("VOLTAGE_API_KEY", Precedence::Flag("--account"), None),
            "warning: VOLTAGE_API_KEY is set but ignored because --account is given"
        );
    }

    #[test]
    fn a_profile_warning_without_a_profile_name_stays_a_sentence() {
        assert_eq!(
            shown("VOLTAGE_API_KEY", Precedence::Profile, None),
            "warning: VOLTAGE_API_KEY is set but ignored because a profile is selected"
        );
    }

    #[test]
    fn a_profile_name_cannot_write_control_characters_to_the_terminal() {
        let ignored = IgnoredVariable {
            name: "VOLTAGE_WALLET_ID",
            precedence: Precedence::Profile,
        };
        // Unstripped, so an escape the name smuggled in would still be here.
        let line = ignored_warning(&ignored, Some("evil\u{1b}]0;x\u{7}"));
        assert!(
            line.as_str()
                .ends_with(" is set but ignored because --profile evil ]0;x  is selected"),
            "{:?}",
            line.as_str()
        );
    }
}

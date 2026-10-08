//! `voltage context`: the effective scope, credential selection, and configuration location,
//! each with where it came from.
//!
//! Inspection reads `config.toml` and the environment only. It never reads a saved secret,
//! makes a network request, or creates the configuration directory.

use crate::{
    Result,
    auth::{self, Rejected, Selected},
    cli::GlobalFlags,
    config::{self, API_URL, AccountKind, CredentialProblem, CredentialStore, Settings, Source},
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
    config_dir: Setting<String>,
    config_file: ConfigFile,
    /// `VOLTAGE_*` variables that are set but supply nothing, because a flag or the selected
    /// profile takes precedence.
    ignored_variables: Vec<&'static str>,
}

/// Resolve the context with the same precedence every command uses.
pub fn inspect(settings: &Settings, global: &GlobalFlags) -> Result<Context> {
    let (scope, sources) = settings.sourced_scope(global.scope_selection())?;
    let (_, config_dir_source) = config::sourced_directory(global.config_dir.clone())?;
    let mut ignored_variables = config::ignored_variables(&sources, config_dir_source);
    if auth::ignores_ambient_api_key(global) {
        ignored_variables.push("VOLTAGE_API_KEY");
    }
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
        config_dir: Setting {
            value: settings.dir.display().to_string(),
            source: config_dir_source,
        },
        config_file: ConfigFile {
            exists: config_file.exists(),
            path: config_file.display().to_string(),
        },
        ignored_variables,
    })
}

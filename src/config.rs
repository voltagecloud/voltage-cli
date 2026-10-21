//! Settings, saved credentials, and the private files under the configuration directory.
//!
//! Every file the CLI writes here is owner-only and written atomically. Credentials default
//! to the operating system store; file storage is an explicit choice with the same
//! ownership and permission checks on every read.

use crate::{
    Error, Result,
    secret::Secret,
    terminal::{Terminal, read_detached},
};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;
use zeroize::Zeroizing;

pub const API_URL: &str = "https://voltageapi.com/v1";
pub const AUTH_URL: &str = "https://auth.voltage.cloud/api/v1";

const CONFIG_FILE: &str = "config.toml";
const CREDENTIALS_DIR: &str = "credentials";
const LOCK_FILE: &str = "credentials.lock";
const KEYRING_SERVICE: &str = "voltage-cli";
pub(crate) const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_RETRY: Duration = Duration::from_millis(100);

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub accounts: BTreeMap<String, Account>,
}

/// A named organization, environment, and credential selected together.
#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub organization_id: Uuid,
    pub environment_id: Uuid,
    pub account: String,
}

/// How a saved credential was obtained.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    /// A browser login whose tokens refresh and can be revoked.
    User,
    /// An environment API key imported with its scope binding.
    ApiKey,
}

/// Where a saved credential's secret lives.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum CredentialStore {
    /// The operating system credential store.
    Keychain,
    /// An owner-only file under the configuration directory.
    File,
}

impl CredentialStore {
    /// The store's name in a message.
    fn label(self) -> &'static str {
        match self {
            Self::Keychain => "OS credential store",
            Self::File => "credentials directory",
        }
    }
}

/// Non-secret facts about one saved credential.
#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub store: CredentialStore,
    pub kind: AccountKind,
    pub email: Option<String>,
    pub organization_id: Option<Uuid>,
    pub environment_id: Option<Uuid>,
    pub auth_url: String,
}

impl Account {
    /// An API key bound to a different organization or environment than the scope selects.
    pub(crate) fn bound_elsewhere(&self, scope: &Scope) -> bool {
        let organization_mismatch = scope
            .org
            .zip(self.organization_id)
            .is_some_and(|(selected, bound)| selected != bound);
        let environment_mismatch = self
            .environment_id
            .is_some_and(|bound| scope.envs.iter().any(|env| *env != bound));
        self.kind == AccountKind::ApiKey && (organization_mismatch || environment_mismatch)
    }
}

/// Why commands would reject the credential selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CredentialProblem {
    /// `VOLTAGE_API_KEY` is set but blank, and still shadows saved credentials.
    EmptyApiKey,
    /// Nothing is saved and no key is in the environment.
    NoCredential,
    /// More than one credential is saved and none is selected.
    MultipleCredentials,
    /// The selected credential name is not saved.
    UnknownCredential,
    /// The saved API key is bound to a different organization or environment.
    BoundElsewhere,
    /// `--auth-url` names a different auth service than the one the browser login was saved
    /// with, and a login refreshes only at its recorded origin.
    AuthUrlMismatch,
}

impl CredentialProblem {
    pub(crate) fn error(self, scope: &Scope, settings: &Settings) -> Error {
        let saved = joined(settings.config.accounts.keys());
        match self {
            Self::EmptyApiKey => Error::auth("VOLTAGE_API_KEY is empty")
                .with_hint("Unset VOLTAGE_API_KEY to use a saved credential, or set it to a key."),
            Self::NoCredential => Error::auth("No credential is available").with_hint(
                "Sign in with `voltage login`, select one with --account NAME, or provide VOLTAGE_API_KEY.",
            ),
            Self::MultipleCredentials => Error::usage("Multiple credentials are saved").with_hint(
                format!(
                    "Saved credentials: {}. Select one with --account NAME or --profile NAME.",
                    saved.unwrap_or_default()
                ),
            ),
            Self::UnknownCredential => Error::auth(format!(
                "Unknown credential {}",
                scope.account.as_deref().unwrap_or_default()
            ))
            .with_hint(match saved {
                Some(saved) => format!("Saved credentials: {saved}."),
                None => "No credentials are saved; sign in with `voltage login` or import a key with `voltage auth import-key`.".into(),
            }),
            Self::BoundElsewhere => Error::usage(
                "API key is bound to a different organization or environment; select a matching credential",
            )
            .with_hint(format!(
                "Saved credentials: {}. Select a key for this scope or a browser login with --account NAME.",
                saved.unwrap_or_default()
            )),
            Self::AuthUrlMismatch => Error::usage(
                "Saved credentials cannot be refreshed at a different auth URL",
            )
            .with_hint(
                "Omit --auth-url to use the URL the login was saved with (`voltage context` shows it). To move the login to this URL, run `voltage logout --account NAME`, then `voltage login --account NAME --auth-url URL`.",
            ),
        }
    }
}

/// `VOLTAGE_*` variables that are set but supply nothing, because a flag or the selected
/// profile takes precedence. Kept beside the precedence rules that decide it.
pub(crate) fn ignored_variables(sources: &ScopeSources, config_dir: Source) -> Vec<&'static str> {
    [
        ("VOLTAGE_ORGANIZATION_ID", sources.org),
        ("VOLTAGE_ENVIRONMENT_ID", sources.envs),
        ("VOLTAGE_WALLET_ID", sources.wallet),
        ("VOLTAGE_CONFIG_DIR", config_dir),
    ]
    .into_iter()
    .filter(|(name, source)| std::env::var_os(name).is_some() && *source != Source::Environment)
    .map(|(name, _)| name)
    .collect()
}

/// A browser login's tokens and identity.
pub struct Login {
    pub access_token: Secret,
    pub refresh_token: Secret,
    /// Unix seconds after which the access token needs a refresh.
    pub expires_at: u64,
    pub user_id: Option<String>,
    pub email: Option<String>,
}

/// A saved credential in its typed form.
pub enum Credential {
    ApiKey(Secret),
    Login(Login),
}

/// Storage form of a credential, read from the store and validated into `Credential`.
#[derive(Deserialize)]
struct StoredCredential {
    api_key: Option<Zeroizing<String>>,
    access_token: Option<Zeroizing<String>>,
    refresh_token: Option<Zeroizing<String>>,
    expires_at: Option<u64>,
    user_id: Option<String>,
    email: Option<String>,
}

impl TryFrom<StoredCredential> for Credential {
    type Error = Error;

    fn try_from(stored: StoredCredential) -> Result<Self> {
        let invalid = || Error::auth("Invalid saved credential");
        match (stored.api_key, stored.access_token, stored.refresh_token) {
            (Some(api_key), None, None) => Ok(Self::ApiKey(api_key.into())),
            (None, Some(access_token), Some(refresh_token)) => Ok(Self::Login(Login {
                access_token: access_token.into(),
                refresh_token: refresh_token.into(),
                expires_at: stored.expires_at.unwrap_or(0),
                user_id: stored.user_id,
                email: stored.email,
            })),
            _ => Err(invalid()),
        }
    }
}

/// Storage form of a credential borrowed for writing; the same fields as `StoredCredential`.
#[derive(Serialize)]
struct StoredCredentialRef<'a> {
    api_key: Option<&'a str>,
    access_token: Option<&'a str>,
    refresh_token: Option<&'a str>,
    expires_at: Option<u64>,
    user_id: Option<&'a str>,
    email: Option<&'a str>,
}

impl<'a> From<&'a Credential> for StoredCredentialRef<'a> {
    fn from(credential: &'a Credential) -> Self {
        match credential {
            Credential::ApiKey(key) => Self {
                api_key: Some(key.expose()),
                access_token: None,
                refresh_token: None,
                expires_at: None,
                user_id: None,
                email: None,
            },
            Credential::Login(login) => Self {
                api_key: None,
                access_token: Some(login.access_token.expose()),
                refresh_token: Some(login.refresh_token.expose()),
                expires_at: Some(login.expires_at),
                user_id: login.user_id.as_deref(),
                email: login.email.as_deref(),
            },
        }
    }
}

/// The organization, environments, and resources a command acts on.
#[derive(Default)]
pub struct Scope {
    pub org: Option<Uuid>,
    pub envs: Vec<Uuid>,
    pub wallet: Option<Uuid>,
    pub webhook: Option<Uuid>,
    pub account: Option<String>,
}

/// How to find and supply each scope value a command can require.
pub(crate) const ORGANIZATION_HINT: &str = "Find your organization ID with `voltage organizations list` (browser login only), then pass --org ID, set VOLTAGE_ORGANIZATION_ID, or save it in a profile with `voltage profiles create`.";
pub(crate) const ENVIRONMENT_HINT: &str = "List environments with `voltage environments list --org ORG_ID` (browser login only), then pass one --env ID, set VOLTAGE_ENVIRONMENT_ID, or save it in a profile with `voltage profiles create`.";
pub(crate) const WALLET_HINT: &str = "List wallets with `voltage wallets list`, or create one with `voltage wallets create`, then pass --wallet ID. VOLTAGE_WALLET_ID is read only when no --profile is selected.";
pub(crate) const WEBHOOK_HINT: &str =
    "List webhooks with `voltage webhooks list`, then pass --webhook ID.";

impl Scope {
    pub fn require_org(&self) -> Result<Uuid> {
        self.org
            .ok_or_else(|| Error::usage("--org is required").with_hint(ORGANIZATION_HINT))
    }

    /// Operations bound to one environment refuse an ambiguous list.
    pub fn single_env(&self) -> Result<Uuid> {
        match self.envs.as_slice() {
            [env] => Ok(*env),
            _ => Err(
                Error::usage("Exactly one --env is required for this operation")
                    .with_hint(ENVIRONMENT_HINT),
            ),
        }
    }
}

/// Where an effective setting came from.
#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    /// A command-line flag.
    Flag,
    /// The selected profile.
    Profile,
    /// A `VOLTAGE_*` environment variable.
    Environment,
    /// The built-in default, or the only saved credential.
    Default,
    /// Nothing supplied it.
    Unset,
}

/// Where each part of a resolved `Scope` came from.
pub(crate) struct ScopeSources {
    pub(crate) org: Source,
    pub(crate) envs: Source,
    pub(crate) wallet: Source,
    pub(crate) webhook: Source,
    pub(crate) account: Source,
}

/// Scope-affecting inputs resolved from flags, a profile, or ambient variables.
pub struct ScopeSelection {
    pub profile: Option<String>,
    pub account: Option<String>,
    pub org: Option<Uuid>,
    pub envs: Vec<Uuid>,
    pub wallet: Option<Uuid>,
    pub webhook: Option<Uuid>,
}

/// The configuration directory: `--config-dir`, then `VOLTAGE_CONFIG_DIR`, then XDG.
pub fn directory(explicit: Option<PathBuf>) -> Result<PathBuf> {
    sourced_directory(explicit).map(|(dir, _)| dir)
}

/// `directory` and which of its sources supplied it.
pub(crate) fn sourced_directory(explicit: Option<PathBuf>) -> Result<(PathBuf, Source)> {
    let (dir, source) = match explicit {
        Some(dir) => (dir, Source::Flag),
        None => match std::env::var_os("VOLTAGE_CONFIG_DIR") {
            Some(dir) => (PathBuf::from(dir), Source::Environment),
            None => (
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                    })
                    .join("voltage"),
                Source::Default,
            ),
        },
    };
    if !dir.is_absolute() {
        return Err(Error::usage(
            "The configuration directory must be an absolute path",
        ));
    }
    Ok((dir, source))
}

pub struct Settings {
    pub dir: PathBuf,
    pub config: Config,
}

impl Settings {
    /// Read `config.toml` from an absolute directory, or start empty when it does not exist.
    pub fn open(dir: PathBuf) -> Result<Self> {
        let config = read_config(&dir)?;
        Ok(Self { dir, config })
    }

    /// Where `config.toml` lives, whether or not it exists yet.
    pub(crate) fn config_file(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    /// Re-read the configuration after taking the lock, so concurrent edits are not lost.
    pub fn reload(&mut self) -> Result<()> {
        self.config = read_config(&self.dir)?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        private_dir(&self.dir)?;
        let text = toml::to_string_pretty(&self.config)
            .map_err(|error| Error::transport(error.to_string()))?;
        atomic_write(&self.dir.join(CONFIG_FILE), text.as_bytes())
    }

    /// A profile selects everything together and ignores ambient variables; otherwise
    /// explicit flags override `VOLTAGE_*` variables.
    pub fn scope(&self, selection: ScopeSelection) -> Result<Scope> {
        self.sourced_scope(selection).map(|(scope, _)| scope)
    }

    /// `scope` and where each of its values came from.
    pub(crate) fn sourced_scope(&self, selection: ScopeSelection) -> Result<(Scope, ScopeSources)> {
        let profile = selection
            .profile
            .as_deref()
            .map(|name| {
                self.config
                    .profiles
                    .get(name)
                    .ok_or_else(|| self.unknown_profile(name))
            })
            .transpose()?;
        let ambient = |name: &str| -> Result<(Option<Uuid>, Source)> {
            if profile.is_some() {
                return Ok((None, Source::Unset));
            }
            let value = std::env::var(name)
                .ok()
                .map(|value| {
                    Uuid::parse_str(&value)
                        .map_err(|_| Error::usage(format!("Expected UUID, got {value}")))
                })
                .transpose()?;
            Ok(sourced(value, Source::Environment))
        };
        let (org, org_source) = match selection.org {
            Some(org) => (Some(org), Source::Flag),
            None => match profile {
                Some(profile) => (Some(profile.organization_id), Source::Profile),
                None => ambient("VOLTAGE_ORGANIZATION_ID")?,
            },
        };
        let (envs, envs_source) = if !selection.envs.is_empty() {
            (selection.envs, Source::Flag)
        } else {
            match profile {
                Some(profile) => (vec![profile.environment_id], Source::Profile),
                None => {
                    let (env, source) = ambient("VOLTAGE_ENVIRONMENT_ID")?;
                    (env.into_iter().collect(), source)
                }
            }
        };
        let (wallet, wallet_source) = match selection.wallet {
            Some(wallet) => (Some(wallet), Source::Flag),
            None => ambient("VOLTAGE_WALLET_ID")?,
        };
        let (webhook, webhook_source) = sourced(selection.webhook, Source::Flag);
        let (account, account_source) = match selection.account {
            Some(account) => (Some(account), Source::Flag),
            None => sourced(
                profile.map(|profile| profile.account.clone()),
                Source::Profile,
            ),
        };
        Ok((
            Scope {
                org,
                envs,
                wallet,
                webhook,
                account,
            },
            ScopeSources {
                org: org_source,
                envs: envs_source,
                wallet: wallet_source,
                webhook: webhook_source,
                account: account_source,
            },
        ))
    }

    /// The saved credential a command uses: the selected one, or the only one.
    pub fn account_name(&self, scope: &Scope) -> Result<String> {
        self.select_account(scope)
            .map_err(|problem| problem.error(scope, self))
    }

    /// The saved credential a scope selects: the named one, or the only one saved.
    pub(crate) fn select_account(
        &self,
        scope: &Scope,
    ) -> std::result::Result<String, CredentialProblem> {
        if let Some(name) = &scope.account {
            if !self.config.accounts.contains_key(name) {
                return Err(CredentialProblem::UnknownCredential);
            }
            return Ok(name.clone());
        }
        let mut names = self.config.accounts.keys();
        match (names.next(), names.next()) {
            (None, _) => Err(CredentialProblem::NoCredential),
            (Some(name), None) => Ok(name.clone()),
            (Some(_), Some(_)) => Err(CredentialProblem::MultipleCredentials),
        }
    }

    /// An unknown profile name, with the saved names so the next attempt can pick one.
    pub(crate) fn unknown_profile(&self, name: &str) -> Error {
        Error::usage(format!("Unknown profile {name}")).with_hint(
            match joined(self.config.profiles.keys()) {
                Some(saved) => format!("Saved profiles: {saved}."),
                None => "No profiles are saved; create one with `voltage profiles create NAME --org ID --env ID`.".into(),
            },
        )
    }

    pub fn account(&self, name: &str) -> Result<&Account> {
        self.config
            .accounts
            .get(name)
            .ok_or_else(|| Error::auth("Unknown credential"))
    }

    fn credential_path(&self, name: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        self.dir.join(CREDENTIALS_DIR).join(format!(
            "{}.json",
            hex::encode(Sha256::digest(name.as_bytes()))
        ))
    }

    /// The keyring entry is bound to the configuration directory so two directories
    /// never share a saved credential.
    fn keyring_entry(&self, name: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, &format!("{}:{name}", self.dir.display()))
            .map_err(|_| Error::auth("Cannot access OS credential store"))
    }

    pub fn read_credential(&self, name: &str) -> Result<Credential> {
        let raw = self.read_secret(name, self.account(name)?.store)?;
        let stored: StoredCredential =
            serde_json::from_str(&raw).map_err(|_| Error::auth("Invalid saved credential"))?;
        Credential::try_from(stored)
    }

    /// The secret saved for `name` in `store`, as it was written.
    fn read_secret(&self, name: &str, store: CredentialStore) -> Result<Zeroizing<String>> {
        match store {
            CredentialStore::File => read_private(&self.credential_path(name)),
            CredentialStore::Keychain => {
                Ok(Zeroizing::new(self.keyring_entry(name)?.get_password().map_err(|_| {
                    Error::auth(
                        "Cannot read OS credential store; unlock it or log in with --credential-store file",
                    )
                })?))
            }
        }
    }

    pub fn write_credential(
        &self,
        name: &str,
        store: CredentialStore,
        credential: &Credential,
    ) -> Result<()> {
        let raw = serialized(credential)?;
        match store {
            CredentialStore::File => {
                private_dir(&self.dir)?;
                private_dir(&self.dir.join(CREDENTIALS_DIR))?;
                atomic_write(&self.credential_path(name), raw.as_bytes())
            }
            CredentialStore::Keychain => self
                .keyring_entry(name)?
                .set_password(&raw)
                .map_err(|_| {
                    Error::auth(
                        "Cannot write OS credential store; explicitly select --credential-store file if needed",
                    )
                }),
        }
    }

    pub fn delete_credential(&self, name: &str) -> Result<()> {
        self.delete_secret(name, self.account(name)?.store)
    }

    /// Remove the secret saved for `name` in `store`, whatever store its account names. A
    /// secret that is already gone is not an error, so `logout --local` can remove its account.
    fn delete_secret(&self, name: &str, store: CredentialStore) -> Result<()> {
        match store {
            CredentialStore::File => {
                fs::remove_file(self.credential_path(name)).or_else(|error| match error.kind() {
                    std::io::ErrorKind::NotFound => Ok(()),
                    _ => Err(error.into()),
                })
            }
            CredentialStore::Keychain => {
                self.keyring_entry(name)?
                    .delete_credential()
                    .or_else(|error| match error {
                        keyring::Error::NoEntry => Ok(()),
                        _ => Err(Error::auth("Cannot remove saved credential")),
                    })
            }
        }
    }

    /// Remove the secret saved for `name`, as `delete_credential` does, but warn instead of
    /// failing when it cannot be removed.
    pub(crate) fn discard_credential(&self, name: &str, terminal: Terminal) -> Result<()> {
        self.discard_secret(name, self.account(name)?.store, terminal);
        Ok(())
    }

    /// Remove a secret that no account reaches any more. A failure leaves the secret behind
    /// but does not fail the command, so it is shown as a warning.
    fn discard_secret(&self, name: &str, store: CredentialStore, terminal: Terminal) {
        if self.delete_secret(name, store).is_err() {
            terminal.important(format!(
                "Could not remove the saved secret for {name} from the {}; remove it there yourself.",
                store.label()
            ));
        }
    }

    /// Save a new account and its secret under the credential lock. A name already in use is
    /// refused. When the configuration cannot be saved, the secret is removed again, so no
    /// stored secret is left that no account entry reaches.
    pub async fn add_account(
        &mut self,
        name: &str,
        account: Account,
        credential: &Credential,
        terminal: Terminal,
    ) -> Result<()> {
        self.put_account(name, account, credential, terminal, |_| false, |_| false)
            .await
            .map(drop)
    }

    /// Save an account and its secret under the credential lock, as `add_account` does. A
    /// saved account of that name is replaced only when `replaceable` accepts it and then
    /// `same` accepts its credential; the replaced credential is returned so its session can
    /// be revoked. The saved secret is read only for a replaceable account, and a secret that
    /// cannot be read refuses the name with a hint to remove it. When the secret or the
    /// configuration cannot be saved, the saved account and secret are restored.
    pub async fn put_account(
        &mut self,
        name: &str,
        account: Account,
        credential: &Credential,
        terminal: Terminal,
        replaceable: impl FnOnce(&Account) -> bool,
        same: impl FnOnce(&Credential) -> bool,
    ) -> Result<Option<Credential>> {
        let _lock = self.lock(terminal).await?;
        self.reload()?;
        let saved = self
            .config
            .accounts
            .get(name)
            .cloned()
            .map(|saved| {
                if !replaceable(&saved) {
                    return Err(name_in_use(name));
                }
                let old = self
                    .read_credential(name)
                    .map_err(|_| unreadable_name(name, saved.store))?;
                same(&old)
                    .then_some((saved, old))
                    .ok_or_else(|| name_in_use(name))
            })
            .transpose()?;
        let store = account.store;
        // A failed write may already have replaced the saved secret, as when the directory
        // sync after the rename fails, so every failure restores it.
        let written = self
            .write_credential(name, store, credential)
            .and_then(|()| {
                self.config.accounts.insert(name.to_owned(), account);
                self.save()
            });
        if let Err(error) = written {
            return Err(self.restore(name, store, saved, terminal, error));
        }
        // A store change is not exercised by tests: they cannot use an OS keychain.
        if let Some((saved, _)) = &saved
            && saved.store != store
        {
            self.discard_secret(name, saved.store, terminal);
        }
        Ok(saved.map(|(_, old)| old))
    }

    /// Undo a `put_account` that failed with `error`: put back the replaced account and
    /// secret, or remove the new secret when nothing was replaced, and save the configuration
    /// again, since a save that failed after its rename has already written it. The returned
    /// error says so when the saved login could not be put back.
    fn restore(
        &mut self,
        name: &str,
        store: CredentialStore,
        saved: Option<(Account, Credential)>,
        terminal: Terminal,
        error: Error,
    ) -> Error {
        // The new secret goes unless the old one is written back over it in the same store.
        // As in `put_account`, a store change is not exercised by tests.
        if saved.as_ref().is_none_or(|(saved, _)| saved.store != store) {
            self.discard_secret(name, store, terminal);
        }
        let secret_restored = match saved {
            Some((saved, old)) => {
                let restored = self.write_credential(name, saved.store, &old).is_ok()
                    || self.holds_secret(name, saved.store, &old);
                self.config.accounts.insert(name.to_owned(), saved);
                restored
            }
            None => {
                self.config.accounts.remove(name);
                true
            }
        };
        let name_word = shell_word(name);
        if !(self.save().is_ok() || self.saved_store_matches(name)) {
            return error.with_hint(format!(
                "The saved configuration for {name} could not be restored. Remove it with `voltage logout --local --account {name_word}`, then sign in again."
            ));
        }
        if secret_restored {
            return error;
        }
        error.with_hint(format!(
            "The replaced login for {name} could not be restored. Sign in again with `voltage login --account {name_word}`."
        ))
    }

    /// Whether `store` holds `credential` for `name`, as when a failed write never replaced it.
    fn holds_secret(&self, name: &str, store: CredentialStore, credential: &Credential) -> bool {
        let held = self.read_secret(name, store).and_then(|raw| {
            let stored: StoredCredential = serde_json::from_str(&raw)?;
            serialized(&Credential::try_from(stored)?)
        });
        match (held, serialized(credential)) {
            (Ok(held), Ok(expected)) => held == expected,
            _ => false,
        }
    }

    /// Whether the configuration on disk names the same store for `name` as the one in
    /// memory, or lacks `name` just as it does, as when a failed save never replaced it.
    fn saved_store_matches(&self, name: &str) -> bool {
        let store = |config: &Config| config.accounts.get(name).map(|account| account.store);
        read_config(&self.dir).is_ok_and(|disk| store(&disk) == store(&self.config))
    }

    /// Exclusive process lock for credential and configuration writes.
    pub async fn lock(&self, terminal: Terminal) -> Result<File> {
        private_dir(&self.dir)?;
        let path = self.dir.join(LOCK_FILE);
        if path.exists() {
            check_private(&path)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        check_owner(&file.metadata()?)?;
        let deadline = tokio::time::Instant::now() + LOCK_TIMEOUT;
        let mut progress = None;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => {
                    progress
                        .get_or_insert_with(|| terminal.progress("Waiting for credential lock..."));
                    if tokio::time::Instant::now() >= deadline {
                        return Err(Error::transport("Timed out waiting for credential lock"));
                    }
                    tokio::time::sleep(LOCK_RETRY).await;
                }
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }
}

/// `source` when a value is present, otherwise `Unset`.
/// Sends every change in a script or chain of commands when set to `1` or `true`, as an
/// exported `MAKEFLAGS=-n` reaches every sub-make.
pub const EXECUTE_VARIABLE: &str = "VOLTAGE_EXECUTE";

/// `VOLTAGE_EXECUTE`, validated for every API command so a misspelling fails before any
/// change instead of being read as either mode. `None` when it is not set.
pub fn execute_variable() -> Result<Option<bool>> {
    parse_execute(std::env::var_os(EXECUTE_VARIABLE).as_deref())
}

fn parse_execute(value: Option<&OsStr>) -> Result<Option<bool>> {
    match value.map(OsStr::to_str) {
        None => Ok(None),
        Some(Some("" | "0" | "false")) => Ok(Some(false)),
        Some(Some("1" | "true")) => Ok(Some(true)),
        Some(_) => Err(Error::usage(format!(
            "{EXECUTE_VARIABLE} must be 1, true, 0, or false"
        ))),
    }
}

fn sourced<T>(value: Option<T>, source: Source) -> (Option<T>, Source) {
    let source = if value.is_some() {
        source
    } else {
        Source::Unset
    };
    (value, source)
}

/// `credential` as `write_credential` saves it.
fn serialized(credential: &Credential) -> Result<Zeroizing<String>> {
    Ok(Zeroizing::new(serde_json::to_string(
        &StoredCredentialRef::from(credential),
    )?))
}

/// A new account named like a saved one; `login` and `auth import-key` refuse it alike.
pub(crate) fn name_in_use(name: &str) -> Error {
    Error::usage(format!(
        "Credential {name} already exists; log it out first or use a different --account name"
    ))
}

/// A saved name whose secret cannot be read, so it cannot be shown to be replaceable. Only
/// removing the account frees the name, because a new login cannot overwrite it.
pub(crate) fn unreadable_name(name: &str, store: CredentialStore) -> Error {
    let unlock = match store {
        CredentialStore::Keychain => "Unlock the OS credential store, or remove",
        CredentialStore::File => "Remove",
    };
    name_in_use(name).with_hint(format!(
        "Its saved secret cannot be read. {unlock} it with `voltage logout --local --account {}`.",
        shell_word(name)
    ))
}

/// `value` as one word a POSIX shell reads back unchanged, for a hint's copyable command.
pub(crate) fn shell_word(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+,-./:=_".contains(c));
    if plain {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Saved names, comma-separated, or `None` when there are none.
fn joined<'a>(names: impl Iterator<Item = &'a String>) -> Option<String> {
    let names: Vec<&str> = names.map(String::as_str).collect();
    (!names.is_empty()).then(|| names.join(", "))
}

fn read_config(dir: &Path) -> Result<Config> {
    let path = dir.join(CONFIG_FILE);
    if !path.exists() {
        return Ok(Config::default());
    }
    private_dir(dir)?;
    // Read through the same no-follow, re-verified path as credentials; a plain open would
    // be a check-then-use race against a same-user attacker swapping in a symlink.
    let raw = read_private(&path)?;
    let config: Config = toml::from_str(&raw).map_err(|_| Error::usage("Invalid config.toml"))?;
    // A config is attacker-writable in the threat model above, so its auth URLs must pass
    // the same HTTPS-or-loopback validation as a URL given on the command line.
    for account in config.accounts.values() {
        crate::auth::base_url(&account.auth_url)?;
    }
    Ok(config)
}

/// Create or verify an owner-only directory that is not a symlink.
pub fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    let meta = fs::symlink_metadata(path)?;
    check_owner(&meta)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(Error::usage("Private directory must be a real directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::usage(format!(
                "{} must have mode 700",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Verify an existing owner-only regular file that is not a symlink.
pub fn check_private(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    check_owner(&meta)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::usage(
            "Credential files must be regular files, not symlinks",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::usage(format!(
                "{} must be readable only by its owner",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Credentials and config are small documents; a larger file is not one.
const MAX_PRIVATE_FILE_BYTES: u64 = 1024 * 1024;

/// Read at most `limit` bytes into zeroized memory, refusing anything larger.
fn read_bounded(mut reader: impl std::io::Read, limit: u64) -> Result<Zeroizing<String>> {
    let mut raw = Zeroizing::new(String::new());
    reader
        .by_ref()
        .take(limit.saturating_add(1))
        .read_to_string(&mut raw)?;
    if raw.len() as u64 > limit {
        return Err(Error::usage("Private file exceeds 1 MiB"));
    }
    Ok(raw)
}

/// Read a private file into zeroized memory, rechecking the opened file's ownership and mode.
pub fn read_private(path: &Path) -> Result<Zeroizing<String>> {
    check_private(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    check_owner(&meta)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::usage("Credential file is not private"));
        }
    }
    read_bounded(file, MAX_PRIVATE_FILE_BYTES)
}

fn check_owner(meta: &fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid takes no arguments, has no side effects, and cannot fail.
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return Err(Error::usage(
                "Private storage must be owned by the current user",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    Ok(())
}

/// Reserve a new owner-only file; an existing path is an error so secrets never overwrite.
pub fn new_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| Error::usage(format!("Cannot reserve output {}: {error}", path.display())))
}

/// Write through a private temporary file and rename, so readers never see a partial file.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if path.exists() {
        check_private(path)?;
    }
    let temp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = new_private(&temp)?;
        file.write_all(data)?;
        file.sync_all()?;
        #[cfg(test)]
        let fault = write_faults::next();
        #[cfg(test)]
        write_faults::inject(fault, write_faults::Fault::BeforeRename)?;
        fs::rename(&temp, path)?;
        #[cfg(test)]
        write_faults::inject(fault, write_faults::Fault::AfterRename)?;
        #[cfg(unix)]
        File::open(path.parent().unwrap_or(Path::new("/")))?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Failures that a test makes `atomic_write` report, one per call in order, before or after
/// the rename that replaces the file.
#[cfg(test)]
pub(crate) mod write_faults {
    use std::{cell::RefCell, collections::VecDeque};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum Fault {
        BeforeRename,
        AfterRename,
    }

    thread_local! {
        static FAULTS: RefCell<VecDeque<Option<Fault>>> = const { RefCell::new(VecDeque::new()) };
    }

    /// Fail the next `atomic_write` calls in this thread; `None` lets a call succeed.
    pub(crate) fn set(faults: impl IntoIterator<Item = Option<Fault>>) {
        FAULTS.with(|queue| *queue.borrow_mut() = faults.into_iter().collect());
    }

    pub(super) fn next() -> Option<Fault> {
        FAULTS.with(|queue| queue.borrow_mut().pop_front().flatten())
    }

    pub(super) fn inject(fault: Option<Fault>, stage: Fault) -> std::io::Result<()> {
        if fault == Some(stage) {
            return Err(std::io::Error::other("injected write failure"));
        }
        Ok(())
    }
}

/// Where a command reads a payload or credential from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputSource {
    Stdin,
    File(PathBuf),
}

/// A credential from stdin or a private file, trimmed and never empty. Stdin or a FIFO can
/// block indefinitely, so the read is detached and Ctrl-C still ends the command.
pub async fn read_secret(source: &InputSource) -> Result<Secret> {
    let source = source.clone();
    let raw = read_detached(move || match &source {
        InputSource::Stdin => read_bounded(std::io::stdin(), MAX_PRIVATE_FILE_BYTES),
        InputSource::File(path) => read_private(path),
    })
    .await??;
    let value = raw.trim();
    if value.is_empty() {
        return Err(Error::usage("Credential is empty"));
    }
    Ok(Secret::new(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(dir: PathBuf) -> Settings {
        Settings {
            dir,
            config: Config::default(),
        }
    }

    #[test]
    fn private_files_refuse_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        new_private(&path).unwrap();
        assert!(new_private(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn private_reads_refuse_oversized_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        fs::write(&path, vec![b'x'; (MAX_PRIVATE_FILE_BYTES + 1) as usize]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let error = read_private(&path).unwrap_err();
        assert_eq!(error.message, "Private file exceeds 1 MiB");
    }

    #[cfg(unix)]
    #[test]
    fn configs_with_insecure_auth_urls_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path()).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(
            &path,
            "[accounts.evil]\nstore = \"file\"\nkind = \"api_key\"\nauth_url = \"http://attacker.example\"\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_config(dir.path()).is_err());
    }

    #[test]
    fn execute_variable_accepts_only_explicit_values() {
        assert_eq!(parse_execute(None).unwrap(), None);
        for value in ["1", "true"] {
            assert_eq!(parse_execute(Some(OsStr::new(value))).unwrap(), Some(true));
        }
        for value in ["", "0", "false"] {
            assert_eq!(parse_execute(Some(OsStr::new(value))).unwrap(), Some(false));
        }
        for value in ["yes", "TRUE", "2"] {
            assert_eq!(
                parse_execute(Some(OsStr::new(value))).unwrap_err().message,
                "VOLTAGE_EXECUTE must be 1, true, 0, or false"
            );
        }
    }

    #[test]
    fn profiles_supply_scope_and_flags_still_win() {
        // Any value distinct from the profile's IDs, standing in for `--webhook`.
        let flag_webhook = Uuid::from_u128(1);
        let mut settings = settings(PathBuf::from("/tmp/test"));
        settings.config.profiles.insert(
            "stage".into(),
            Profile {
                organization_id: Uuid::nil(),
                environment_id: Uuid::nil(),
                account: "user".into(),
            },
        );
        let (scope, sources) = settings
            .sourced_scope(ScopeSelection {
                profile: Some("stage".into()),
                account: None,
                org: None,
                envs: Vec::new(),
                wallet: None,
                webhook: Some(flag_webhook),
            })
            .unwrap();
        assert_eq!(scope.account.as_deref(), Some("user"));
        assert_eq!(scope.org, Some(Uuid::nil()));
        assert_eq!(scope.envs, [Uuid::nil()]);
        assert_eq!(scope.webhook, Some(flag_webhook));
        assert_eq!(sources.account, Source::Profile);
        assert_eq!(sources.org, Source::Profile);
        assert_eq!(sources.envs, Source::Profile);
        assert_eq!(sources.wallet, Source::Unset);
        assert_eq!(sources.webhook, Source::Flag);
    }

    #[test]
    fn stored_credentials_round_trip_and_reject_mixed_records() {
        let login = Credential::Login(Login {
            access_token: Secret::new("access".into()),
            refresh_token: Secret::new("refresh".into()),
            expires_at: 7,
            user_id: Some("user".into()),
            email: None,
        });
        let text = serde_json::to_string(&StoredCredentialRef::from(&login)).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({
                "api_key": null, "access_token": "access", "refresh_token": "refresh",
                "expires_at": 7, "user_id": "user", "email": null
            })
        );
        let stored: StoredCredential = serde_json::from_str(&text).unwrap();
        let Credential::Login(restored) = Credential::try_from(stored).unwrap() else {
            panic!("expected a login");
        };
        assert_eq!(restored.expires_at, 7);
        assert_eq!(restored.refresh_token.expose(), "refresh");
        let mixed: StoredCredential =
            serde_json::from_str(r#"{"api_key":"k","access_token":"a","refresh_token":"r"}"#)
                .unwrap();
        assert!(Credential::try_from(mixed).is_err());
    }

    #[test]
    fn account_records_keep_their_config_spelling() {
        let account = Account {
            store: CredentialStore::File,
            kind: AccountKind::ApiKey,
            email: None,
            organization_id: None,
            environment_id: None,
            auth_url: AUTH_URL.into(),
        };
        let text = toml::to_string(&account).unwrap();
        assert!(text.contains("store = \"file\""), "{text}");
        assert!(text.contains("kind = \"api_key\""), "{text}");
    }

    #[test]
    fn file_credentials_round_trip_through_private_storage() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut settings = settings(dir.path().to_path_buf());
        settings.config.accounts.insert(
            "key".into(),
            Account {
                store: CredentialStore::File,
                kind: AccountKind::ApiKey,
                email: None,
                organization_id: None,
                environment_id: None,
                auth_url: AUTH_URL.into(),
            },
        );
        settings
            .write_credential(
                "key",
                CredentialStore::File,
                &Credential::ApiKey(Secret::new("api-key".into())),
            )
            .unwrap();
        let Credential::ApiKey(key) = settings.read_credential("key").unwrap() else {
            panic!("expected an API key");
        };
        assert_eq!(key.expose(), "api-key");
        settings.delete_credential("key").unwrap();
        assert!(settings.read_credential("key").is_err());
    }

    /// A configuration directory that is read-only to its owner refuses the new config.toml,
    /// while the existing credentials directory and lock file stay writable.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_account_that_cannot_be_saved_leaves_no_secret_behind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mode = |path: &Path, mode| {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        };
        mode(dir.path(), 0o700);
        let credentials = dir.path().join(CREDENTIALS_DIR);
        fs::create_dir(&credentials).unwrap();
        mode(&credentials, 0o700);
        new_private(&dir.path().join(LOCK_FILE)).unwrap();
        mode(dir.path(), 0o500);
        // The owner's permissions do not bind root, so there is nothing to test.
        if new_private(&dir.path().join("probe")).is_ok() {
            mode(dir.path(), 0o700);
            return;
        }
        let mut settings = settings(dir.path().to_path_buf());
        let account = Account {
            store: CredentialStore::File,
            kind: AccountKind::ApiKey,
            email: None,
            organization_id: None,
            environment_id: None,
            auth_url: AUTH_URL.into(),
        };
        let key = Credential::ApiKey(Secret::new("api-key".into()));
        let added = settings
            .add_account("key", account, &key, Terminal::new(true, true))
            .await;
        mode(dir.path(), 0o700);
        assert!(added.is_err());
        assert!(!settings.config.accounts.contains_key("key"));
        assert!(!settings.credential_path("key").exists());
    }

    fn login(refresh: &str) -> Credential {
        Credential::Login(Login {
            access_token: Secret::new("access".into()),
            refresh_token: Secret::new(refresh.into()),
            expires_at: 0,
            user_id: Some("user".into()),
            email: None,
        })
    }

    fn refresh_token(settings: &Settings, name: &str) -> String {
        match settings.read_credential(name).unwrap() {
            Credential::Login(login) => login.refresh_token.expose().to_owned(),
            Credential::ApiKey(_) => panic!("expected a login"),
        }
    }

    fn user_account() -> Account {
        Account {
            store: CredentialStore::File,
            kind: AccountKind::User,
            email: None,
            organization_id: None,
            environment_id: None,
            auth_url: AUTH_URL.into(),
        }
    }

    /// A configuration directory with the private mode that `Settings` requires.
    fn private_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn a_saved_account_is_replaced_only_when_accepted() {
        let dir = private_tempdir();
        let mut settings = settings(dir.path().to_path_buf());
        let terminal = Terminal::new(true, true);
        settings
            .add_account("me", user_account(), &login("old"), terminal)
            .await
            .unwrap();
        let refused = settings
            .put_account(
                "me",
                user_account(),
                &login("new"),
                terminal,
                |_| true,
                |_| false,
            )
            .await;
        assert!(refused.is_err());
        assert_eq!(refresh_token(&settings, "me"), "old");
        let replaced = settings
            .put_account(
                "me",
                user_account(),
                &login("new"),
                terminal,
                |_| true,
                |_| true,
            )
            .await
            .unwrap();
        assert!(
            matches!(replaced, Some(Credential::Login(old)) if old.refresh_token.expose() == "old")
        );
        assert_eq!(refresh_token(&settings, "me"), "new");
    }

    /// A saved name whose secret is gone is refused as in use, never as a read error. An
    /// account that cannot be replaced is refused without reading its secret, and a
    /// replaceable one names the command that removes it.
    #[tokio::test]
    async fn a_saved_name_whose_secret_cannot_be_read_is_in_use() {
        let dir = private_tempdir();
        let mut settings = settings(dir.path().to_path_buf());
        let terminal = Terminal::new(true, true);
        settings
            .add_account("me", user_account(), &login("old"), terminal)
            .await
            .unwrap();
        fs::remove_file(settings.credential_path("me")).unwrap();
        let in_use = name_in_use("me").to_string();
        let Err(added) = settings
            .add_account("me", user_account(), &login("new"), terminal)
            .await
        else {
            panic!("a saved name was added again");
        };
        assert_eq!(added.to_string(), in_use);
        assert!(added.hint.is_none());
        let Err(replaced) = settings
            .put_account(
                "me",
                user_account(),
                &login("new"),
                terminal,
                |_| true,
                |_| true,
            )
            .await
        else {
            panic!("an unreadable secret was replaced");
        };
        assert_eq!(replaced.to_string(), in_use);
        assert_eq!(
            replaced.hint.as_deref(),
            Some(
                "Its saved secret cannot be read. Remove it with `voltage logout --local --account me`."
            )
        );
        assert!(!settings.credential_path("me").exists());
        settings.delete_credential("me").unwrap();
        assert_eq!(
            unreadable_name("me", CredentialStore::Keychain)
                .hint
                .as_deref(),
            Some(
                "Its saved secret cannot be read. Unlock the OS credential store, or remove it with `voltage logout --local --account me`."
            )
        );
    }

    /// A replacement that fails returns an error, so `login` revokes the new session. Each
    /// failure, even one after a rename already replaced a file, leaves the replaced login
    /// saved, so the session that stays live is the one that is stored.
    #[tokio::test]
    async fn a_replacement_that_fails_after_a_rename_restores_the_saved_login() {
        use write_faults::Fault::{AfterRename, BeforeRename};
        // The secret, then the configuration: each fails before or after its rename, and
        // the rollback write of the old secret fails too when nothing had replaced it.
        for faults in [
            vec![Some(AfterRename)],
            vec![Some(BeforeRename), Some(BeforeRename)],
            vec![None, Some(AfterRename)],
        ] {
            let dir = private_tempdir();
            let mut settings = settings(dir.path().to_path_buf());
            let terminal = Terminal::new(true, true);
            settings
                .add_account("me", user_account(), &login("old"), terminal)
                .await
                .unwrap();
            let mut account = user_account();
            account.email = Some("me@example.test".into());
            write_faults::set(faults.clone());
            let Err(error) = settings
                .put_account("me", account, &login("new"), terminal, |_| true, |_| true)
                .await
            else {
                panic!("{faults:?}: a failed replacement succeeded");
            };
            write_faults::set([]);
            assert_eq!(error.to_string(), "injected write failure", "{faults:?}");
            assert!(error.hint.is_none(), "{faults:?}");
            settings.reload().unwrap();
            assert_eq!(settings.config.accounts["me"].email, None, "{faults:?}");
            assert_eq!(refresh_token(&settings, "me"), "old", "{faults:?}");
        }
    }

    /// When the old secret cannot be written back over the new one, the error says the
    /// replaced login is lost and how to sign in again.
    #[tokio::test]
    async fn a_failed_rollback_names_the_lost_login() {
        use write_faults::Fault::BeforeRename;
        let dir = private_tempdir();
        let mut settings = settings(dir.path().to_path_buf());
        let terminal = Terminal::new(true, true);
        settings
            .add_account("me", user_account(), &login("old"), terminal)
            .await
            .unwrap();
        write_faults::set([None, Some(BeforeRename), Some(BeforeRename)]);
        let replaced = settings
            .put_account(
                "me",
                user_account(),
                &login("new"),
                terminal,
                |_| true,
                |_| true,
            )
            .await;
        write_faults::set([]);
        let Err(error) = replaced else {
            panic!("a failed replacement succeeded");
        };
        assert_eq!(
            error.hint.as_deref(),
            Some(
                "The replaced login for me could not be restored. Sign in again with `voltage login --account me`."
            )
        );
        assert_eq!(refresh_token(&settings, "me"), "new");
    }

    /// A new account whose secret or configuration write fails after its rename leaves no
    /// secret and no account. When the configuration cannot be saved again, the error names
    /// the command that removes the account it lists.
    #[tokio::test]
    async fn a_new_account_that_fails_after_a_rename_leaves_nothing_saved() {
        use write_faults::Fault::{AfterRename, BeforeRename};
        for faults in [vec![Some(AfterRename)], vec![None, Some(AfterRename)]] {
            let dir = private_tempdir();
            let mut settings = settings(dir.path().to_path_buf());
            let terminal = Terminal::new(true, true);
            write_faults::set(faults.clone());
            let added = settings
                .add_account("me", user_account(), &login("new"), terminal)
                .await;
            write_faults::set([]);
            let Err(error) = added else {
                panic!("{faults:?}: a failed account was added");
            };
            assert!(error.hint.is_none(), "{faults:?}");
            settings.reload().unwrap();
            assert!(!settings.config.accounts.contains_key("me"), "{faults:?}");
            assert!(!settings.credential_path("me").exists(), "{faults:?}");
        }
        let dir = private_tempdir();
        let mut settings = settings(dir.path().to_path_buf());
        let terminal = Terminal::new(true, true);
        write_faults::set([None, Some(AfterRename), Some(BeforeRename)]);
        let added = settings
            .add_account("me", user_account(), &login("new"), terminal)
            .await;
        write_faults::set([]);
        let Err(error) = added else {
            panic!("a failed account was added");
        };
        assert_eq!(
            error.hint.as_deref(),
            Some(
                "The saved configuration for me could not be restored. Remove it with `voltage logout --local --account me`, then sign in again."
            )
        );
    }

    /// A secret that cannot be removed is only a warning for `discard_credential`, which
    /// `logout --local` uses.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_secret_that_cannot_be_removed_is_discarded_with_a_warning() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_tempdir();
        let mut settings = settings(dir.path().to_path_buf());
        let terminal = Terminal::new(true, true);
        settings
            .add_account("me", user_account(), &login("old"), terminal)
            .await
            .unwrap();
        let credentials = dir.path().join(CREDENTIALS_DIR);
        let mode = |mode| {
            fs::set_permissions(&credentials, fs::Permissions::from_mode(mode)).unwrap();
        };
        mode(0o500);
        let deleted = settings.delete_credential("me");
        let discarded = settings.discard_credential("me", terminal);
        let kept = settings.credential_path("me").exists();
        mode(0o700);
        // The owner's permissions do not bind root, so there is nothing to test.
        if deleted.is_ok() {
            return;
        }
        assert!(discarded.is_ok());
        assert!(kept);
    }

    /// When the configuration cannot be saved, the replaced account and secret are put back.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_replacement_that_cannot_be_saved_restores_the_saved_login() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_tempdir();
        let mut settings = settings(dir.path().to_path_buf());
        let terminal = Terminal::new(true, true);
        settings
            .add_account("me", user_account(), &login("old"), terminal)
            .await
            .unwrap();
        let mode = |mode| {
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(mode)).unwrap();
        };
        mode(0o500);
        // The owner's permissions do not bind root, so there is nothing to test.
        if new_private(&dir.path().join("probe")).is_ok() {
            mode(0o700);
            return;
        }
        let mut account = user_account();
        account.email = Some("me@example.test".into());
        let replaced = settings
            .put_account("me", account, &login("new"), terminal, |_| true, |_| true)
            .await;
        mode(0o700);
        assert!(replaced.is_err());
        assert_eq!(settings.config.accounts["me"].email, None);
        assert_eq!(refresh_token(&settings, "me"), "old");
    }
}

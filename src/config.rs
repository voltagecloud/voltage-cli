//! Settings, saved credentials, and the private files under the configuration directory.
//!
//! Every file the CLI writes here is owner-only and written atomically. Credentials default
//! to the operating system store; file storage is an explicit choice with the same
//! ownership and permission checks on every read.

use crate::{Error, Result, secret::Secret};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
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
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
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

impl Scope {
    pub fn require_org(&self) -> Result<Uuid> {
        self.org.ok_or_else(|| {
            Error::usage("--org is required")
                .with_hint("Pass --org UUID or select a profile with --profile NAME.")
        })
    }

    /// Operations bound to one environment refuse an ambiguous list.
    pub fn single_env(&self) -> Result<Uuid> {
        match self.envs.as_slice() {
            [env] => Ok(*env),
            _ => Err(
                Error::usage("Exactly one --env is required for this operation")
                    .with_hint("Pass one --env UUID or select a profile with --profile NAME."),
            ),
        }
    }
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
    let dir = explicit
        .or_else(|| std::env::var_os("VOLTAGE_CONFIG_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                })
                .join("voltage")
        });
    if !dir.is_absolute() {
        return Err(Error::usage(
            "The configuration directory must be an absolute path",
        ));
    }
    Ok(dir)
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
        let profile = selection
            .profile
            .as_deref()
            .map(|name| {
                self.config
                    .profiles
                    .get(name)
                    .ok_or_else(|| Error::usage(format!("Unknown profile {name}")))
            })
            .transpose()?;
        let ambient = |name: &str| -> Result<Option<Uuid>> {
            if profile.is_some() {
                return Ok(None);
            }
            std::env::var(name)
                .ok()
                .map(|value| {
                    Uuid::parse_str(&value)
                        .map_err(|_| Error::usage(format!("Expected UUID, got {value}")))
                })
                .transpose()
        };
        let org = match selection.org {
            Some(org) => Some(org),
            None => match profile {
                Some(profile) => Some(profile.organization_id),
                None => ambient("VOLTAGE_ORGANIZATION_ID")?,
            },
        };
        let envs = if !selection.envs.is_empty() {
            selection.envs
        } else {
            match profile {
                Some(profile) => vec![profile.environment_id],
                None => ambient("VOLTAGE_ENVIRONMENT_ID")?.into_iter().collect(),
            }
        };
        let wallet = match selection.wallet {
            Some(wallet) => Some(wallet),
            None => ambient("VOLTAGE_WALLET_ID")?,
        };
        Ok(Scope {
            org,
            envs,
            wallet,
            webhook: selection.webhook,
            account: selection
                .account
                .or_else(|| profile.map(|profile| profile.account.clone())),
        })
    }

    /// The saved credential a command uses: the selected one, or the only one.
    pub fn account_name(&self, scope: &Scope) -> Result<String> {
        if let Some(name) = &scope.account {
            if !self.config.accounts.contains_key(name) {
                return Err(Error::auth(format!("Unknown credential {name}")));
            }
            return Ok(name.clone());
        }
        let mut names = self.config.accounts.keys();
        match (names.next(), names.next()) {
            (None, _) => Err(Error::auth("No credential is available")
                .with_hint("Run voltage login, select --account, or provide VOLTAGE_API_KEY.")),
            (Some(name), None) => Ok(name.clone()),
            (Some(_), Some(_)) => Err(Error::usage("Multiple credentials are saved")
                .with_hint("Select one with --account NAME or --profile NAME.")),
        }
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
        let raw: Zeroizing<String> = match self.account(name)?.store {
            CredentialStore::File => read_private(&self.credential_path(name))?,
            CredentialStore::Keychain => {
                Zeroizing::new(self.keyring_entry(name)?.get_password().map_err(|_| {
                    Error::auth(
                        "Cannot read OS credential store; unlock it or log in with --credential-store file",
                    )
                })?)
            }
        };
        let stored: StoredCredential =
            serde_json::from_str(&raw).map_err(|_| Error::auth("Invalid saved credential"))?;
        Credential::try_from(stored)
    }

    pub fn write_credential(
        &self,
        name: &str,
        store: CredentialStore,
        credential: &Credential,
    ) -> Result<()> {
        let raw = Zeroizing::new(serde_json::to_string(&StoredCredentialRef::from(
            credential,
        ))?);
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
        match self.account(name)?.store {
            CredentialStore::File => fs::remove_file(self.credential_path(name))?,
            CredentialStore::Keychain => self
                .keyring_entry(name)?
                .delete_credential()
                .map_err(|_| Error::auth("Cannot remove saved credential"))?,
        }
        Ok(())
    }

    /// Exclusive process lock for credential and configuration writes.
    pub async fn lock(&self) -> Result<File> {
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
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => {
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
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        File::open(path.parent().unwrap_or(Path::new("/")))?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Where a command reads a payload or credential from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputSource {
    Stdin,
    File(PathBuf),
}

/// A credential from stdin or a private file, trimmed and never empty.
pub fn read_secret(source: &InputSource) -> Result<Secret> {
    let raw = match source {
        InputSource::Stdin => read_bounded(std::io::stdin(), MAX_PRIVATE_FILE_BYTES)?,
        InputSource::File(path) => read_private(path)?,
    };
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
    fn profiles_ignore_ambient_scope() {
        let mut settings = settings(PathBuf::from("/tmp/test"));
        settings.config.profiles.insert(
            "stage".into(),
            Profile {
                organization_id: Uuid::nil(),
                environment_id: Uuid::nil(),
                account: "user".into(),
            },
        );
        let scope = settings
            .scope(ScopeSelection {
                profile: Some("stage".into()),
                account: None,
                org: None,
                envs: Vec::new(),
                wallet: None,
                webhook: None,
            })
            .unwrap();
        assert_eq!(scope.account.as_deref(), Some("user"));
        assert_eq!(scope.org, Some(Uuid::nil()));
        assert_eq!(scope.envs, [Uuid::nil()]);
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
}

use crate::{Error, Result, cli};
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const API_URL: &str = "https://voltageapi.com/v1";
pub const AUTH_URL: &str = "https://auth.voltage.cloud/api/v1";

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub accounts: BTreeMap<String, Account>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub organization_id: String,
    pub environment_id: String,
    pub account: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub store: String,
    pub kind: String,
    pub email: Option<String>,
    pub organization_id: Option<String>,
    pub environment_id: Option<String>,
    pub auth_url: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Credential {
    pub api_key: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
    pub user_id: Option<String>,
    pub email: Option<String>,
}
impl Drop for Credential {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        for v in [
            &mut self.api_key,
            &mut self.access_token,
            &mut self.refresh_token,
        ]
        .into_iter()
        .flatten()
        {
            v.zeroize();
        }
    }
}
pub struct Settings {
    pub dir: PathBuf,
    pub config: Config,
}
#[derive(Default)]
pub struct Scope {
    pub org: Option<String>,
    pub envs: Vec<String>,
    pub wallet: Option<String>,
    pub webhook: Option<String>,
    pub account: Option<String>,
}

impl Settings {
    pub fn load(m: &ArgMatches) -> Result<Self> {
        let dir = cli::value(m, "config-dir")
            .map(PathBuf::from)
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
        let path = dir.join("config.toml");
        let config = if path.exists() {
            private_dir(&dir)?;
            check_private(&path)?;
            toml::from_str(&fs::read_to_string(path)?)
                .map_err(|_| Error::usage("Invalid config.toml"))?
        } else {
            Config::default()
        };
        Ok(Self { dir, config })
    }
    pub fn save(&self) -> Result<()> {
        private_dir(&self.dir)?;
        atomic_write(
            &self.dir.join("config.toml"),
            toml::to_string_pretty(&self.config)
                .map_err(Error::io)?
                .as_bytes(),
        )
    }
    pub fn scope(&self, m: &ArgMatches) -> Result<Scope> {
        let profile = cli::value(m, "profile")
            .map(|name| {
                self.config
                    .profiles
                    .get(&name)
                    .ok_or_else(|| Error::usage(format!("Unknown profile {name}")))
            })
            .transpose()?;
        let from_env = |name: &str| {
            if profile.is_none() {
                std::env::var(name).ok()
            } else {
                None
            }
        };
        let org = cli::value(m, "org")
            .or_else(|| profile.map(|p| p.organization_id.clone()))
            .or_else(|| from_env("VOLTAGE_ORGANIZATION_ID"));
        let explicit_envs = cli::values(m, "env");
        let envs = if !explicit_envs.is_empty() {
            explicit_envs
        } else {
            profile
                .map(|p| vec![p.environment_id.clone()])
                .or_else(|| from_env("VOLTAGE_ENVIRONMENT_ID").map(|e| vec![e]))
                .unwrap_or_default()
        };
        let scope = Scope {
            org,
            envs,
            wallet: cli::value(m, "wallet"),
            webhook: cli::value(m, "webhook"),
            account: cli::value(m, "account").or_else(|| profile.map(|p| p.account.clone())),
        };
        for id in scope
            .org
            .iter()
            .chain(scope.envs.iter())
            .chain(scope.wallet.iter())
            .chain(scope.webhook.iter())
        {
            validate_uuid(id)?;
        }
        Ok(scope)
    }
    pub fn account_name(&self, scope: &Scope) -> Result<String> {
        if let Some(name) = &scope.account {
            if !self.config.accounts.contains_key(name) {
                return Err(Error::auth(format!("Unknown credential {name}")));
            }
            return Ok(name.clone());
        }
        match self.config.accounts.len() {
            0 => Err(Error::auth("Run voltage login or provide VOLTAGE_API_KEY")),
            1 => Ok(self.config.accounts.keys().next().unwrap().clone()),
            _ => Err(Error::usage(
                "Multiple credentials are saved; select --account or --profile",
            )),
        }
    }
    fn credential_path(&self, name: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        self.dir.join("credentials").join(format!(
            "{}.json",
            hex::encode(Sha256::digest(name.as_bytes()))
        ))
    }
    pub fn read_credential(&self, name: &str) -> Result<Credential> {
        let account = self
            .config
            .accounts
            .get(name)
            .ok_or_else(|| Error::auth("Unknown credential"))?;
        let mut raw = if account.store == "file" {
            read_private(&self.credential_path(name))?
        } else {
            keyring::Entry::new("voltage-cli",&format!("{}:{name}",self.dir.display())).map_err(|_|Error::auth("Cannot access OS credential store"))?
                .get_password().map_err(|_|Error::auth("Cannot read OS credential store; unlock it or log in with --credential-store file"))?
        };
        let result =
            serde_json::from_str(&raw).map_err(|_| Error::auth("Invalid saved credential"));
        use zeroize::Zeroize;
        raw.zeroize();
        result
    }
    pub fn write_credential(&self, name: &str, store: &str, credential: &Credential) -> Result<()> {
        let mut raw = serde_json::to_string(credential)?;
        let result = if store == "file" {
            private_dir(&self.dir)?;
            private_dir(&self.dir.join("credentials"))?;
            atomic_write(&self.credential_path(name), raw.as_bytes())
        } else {
            keyring::Entry::new("voltage-cli",&format!("{}:{name}",self.dir.display())).map_err(|_|Error::auth("Cannot access OS credential store"))?
            .set_password(&raw).map_err(|_|Error::auth("Cannot write OS credential store; explicitly select --credential-store file if needed"))
        };
        use zeroize::Zeroize;
        raw.zeroize();
        result
    }
    pub fn delete_credential(&self, name: &str) -> Result<()> {
        let account = self
            .config
            .accounts
            .get(name)
            .ok_or_else(|| Error::auth("Unknown credential"))?;
        if account.store == "file" {
            fs::remove_file(self.credential_path(name))?;
        } else {
            keyring::Entry::new("voltage-cli", &format!("{}:{name}", self.dir.display()))
                .map_err(|_| Error::auth("Cannot access OS credential store"))?
                .delete_credential()
                .map_err(|_| Error::auth("Cannot remove saved credential"))?;
        }
        Ok(())
    }
    pub async fn lock(&self) -> Result<File> {
        private_dir(&self.dir)?;
        let path = self.dir.join("credentials.lock");
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
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(Error::io("Timed out waiting for credential lock"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

pub fn validate_uuid(value: &str) -> Result<()> {
    uuid::Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| Error::usage(format!("Expected UUID, got {value}")))
}
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
pub fn read_private(path: &Path) -> Result<String> {
    check_private(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let meta = file.metadata()?;
    check_owner(&meta)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::usage("Credential file is not private"));
        }
    }
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    Ok(raw)
}
fn check_owner(meta: &fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // geteuid has no arguments, no side effects, and cannot fail.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(Error::usage(
                "Private storage must be owned by the current user",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    Ok(())
}
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
        .map_err(|e| Error::usage(format!("Cannot reserve output {}: {e}", path.display())))
}
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if path.exists() {
        check_private(path)?;
    }
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = new_private(&temp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn read_secret(source: &str) -> Result<String> {
    let mut value = String::new();
    if source == "-" {
        std::io::stdin().read_to_string(&mut value)?;
    } else {
        value = read_private(Path::new(source))?;
    }
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(Error::usage("Credential is empty"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_files_refuse_overwrite() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("secret");
        new_private(&p).unwrap();
        assert!(new_private(&p).is_err());
    }
    #[test]
    fn profiles_ignore_ambient_scope() {
        let mut s = Settings {
            dir: PathBuf::from("/tmp/test"),
            config: Config::default(),
        };
        s.config.profiles.insert(
            "stage".into(),
            Profile {
                organization_id: uuid::Uuid::nil().to_string(),
                environment_id: uuid::Uuid::nil().to_string(),
                account: "user".into(),
            },
        );
        let m = cli::command()
            .try_get_matches_from(["voltage", "wallets", "list", "--profile", "stage"])
            .unwrap();
        assert_eq!(
            s.scope(cli::leaf(&m).1).unwrap().account.as_deref(),
            Some("user")
        );
    }
}

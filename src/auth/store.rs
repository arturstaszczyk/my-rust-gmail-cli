//! Local state: the OAuth client file, the account name, and the refresh token
//! in the OS keyring (Secret Service), or, opt-in for headless machines, in a
//! 0600 `refresh_token` file.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// Keyring service name. Entries are keyed by the account's email address.
pub const KEYRING_SERVICE: &str = "my-google-cli";
const APP_DIR: &str = "my-google-cli";
const CLIENT_FILE: &str = "client.json";
const CONFIG_FILE: &str = "config.json";
const TOKEN_FILE: &str = "refresh_token";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default)]
    pub token_store: TokenStore,
}

/// Where the refresh token lives. Absent in older configs, meaning `Keyring`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenStore {
    #[default]
    Keyring,
    File,
}

impl TokenStore {
    pub fn name(self) -> &'static str {
        match self {
            TokenStore::Keyring => "keyring",
            TokenStore::File => "file",
        }
    }
}

/// `$XDG_CONFIG_HOME/my-google-cli`, or `~/.config/my-google-cli`.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && Path::new(&xdg).is_absolute()
    {
        return Ok(PathBuf::from(xdg).join(APP_DIR));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or_else(|| Error::usage("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".config").join(APP_DIR))
}

fn ensure_dir() -> Result<PathBuf> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| Error::io(format!("setting permissions on {}", dir.display()), e))?;
    Ok(dir)
}

/// Writes a file readable only by the owner (0600), replacing it atomically.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| Error::io(format!("writing {}", tmp.display()), e))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| Error::io(format!("writing {}", tmp.display()), e))?;
    fs::rename(&tmp, path).map_err(|e| Error::io(format!("writing {}", path.display()), e))
}

/// Reads the Desktop-app client JSON downloaded from Google Cloud console.
pub fn parse_client_json(text: &str) -> Result<Client> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| Error::usage(format!("client JSON is not valid JSON: {e}")))?;
    let Some(installed) = value.get("installed") else {
        let hint = if value.get("web").is_some() {
            "this is a Web application client; create an OAuth client of type Desktop app"
        } else {
            "download the JSON of an OAuth client ID of type Desktop app"
        };
        return Err(Error::usage("client JSON has no \"installed\" section").hint(hint));
    };
    let field = |name: &str| {
        installed
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| Error::usage(format!("client JSON is missing installed.{name}")))
    };
    let client = Client {
        client_id: field("client_id")?,
        client_secret: field("client_secret")?,
    };
    if !client.client_id.ends_with(".apps.googleusercontent.com") {
        return Err(Error::usage("client_id doesn't look like a Google OAuth client id"));
    }
    Ok(client)
}

/// `mg auth setup`: validates the downloaded file and stores id + secret.
pub fn import_client(src: &Path) -> Result<PathBuf> {
    let mut text = String::new();
    fs::File::open(src)
        .and_then(|f| f.take(64 * 1024).read_to_string(&mut text))
        .map_err(|e| Error::io(format!("reading {}", src.display()), e))?;
    let client = parse_client_json(&text)?;
    let path = ensure_dir()?.join(CLIENT_FILE);
    let body = serde_json::to_vec_pretty(&client).expect("client serializes");
    write_private(&path, &body)?;
    Ok(path)
}

pub fn client_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CLIENT_FILE))
}

pub fn load_client() -> Result<Client> {
    let path = client_path()?;
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::auth("no OAuth client configured")
                .hint("run `mg auth setup --client-json <path>`"));
        }
        Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
    };
    serde_json::from_str(&text).map_err(|e| {
        Error::auth(format!("{} is corrupt: {e}", path.display()))
            .hint("run `mg auth setup --client-json <path>` again")
    })
}

pub fn load_config() -> Result<Config> {
    let path = config_dir()?.join(CONFIG_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| Error::usage(format!("{} is corrupt: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(Error::io(format!("reading {}", path.display()), e)),
    }
}

pub fn save_config(config: &Config) -> Result<()> {
    let path = ensure_dir()?.join(CONFIG_FILE);
    let body = serde_json::to_vec_pretty(config).expect("config serializes");
    write_private(&path, &body)
}

fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, account).map_err(keyring_error)
}

fn keyring_error(err: keyring::Error) -> Error {
    Error::auth(format!("keyring: {err}")).hint(
        "is a Secret Service provider (gnome-keyring, KeePassXC) running and unlocked? \
         On a headless machine, use `mg auth login --no-browser --token-file`",
    )
}

/// The keyring is keyed by `account`; the file is a single fixed path.
pub fn set_refresh_token(store: TokenStore, account: &str, token: &str) -> Result<()> {
    match store {
        TokenStore::Keyring => entry(account)?.set_password(token).map_err(keyring_error),
        TokenStore::File => write_private(&ensure_dir()?.join(TOKEN_FILE), token.as_bytes()),
    }
}

pub fn get_refresh_token(store: TokenStore, account: &str) -> Result<Option<String>> {
    match store {
        TokenStore::Keyring => match entry(account)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keyring_error(e)),
        },
        TokenStore::File => read_token_file(&config_dir()?.join(TOKEN_FILE)),
    }
}

/// Returns whether a token existed.
pub fn delete_refresh_token(store: TokenStore, account: &str) -> Result<bool> {
    match store {
        TokenStore::Keyring => match entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(keyring_error(e)),
        },
        TokenStore::File => {
            let path = config_dir()?.join(TOKEN_FILE);
            match fs::remove_file(&path) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(Error::io(format!("removing {}", path.display()), e)),
            }
        }
    }
}

/// Reads the token file only if it is a regular file (not a symlink), owned
/// by us, with no group/other access, like `ssh` does with private keys.
fn read_token_file(path: &Path) -> Result<Option<String>> {
    let refuse = |why: &str| {
        Error::auth(format!("refusing {}: {why}", path.display()))
            .hint("run `mg auth login --token-file` to write it again")
    };
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
    };
    if !meta.file_type().is_file() {
        return Err(refuse("not a regular file"));
    }
    // /proc/self is owned by our effective uid; std has no getuid().
    let uid = fs::metadata("/proc/self").map_err(|e| Error::io("reading /proc/self", e))?.uid();
    if meta.uid() != uid {
        return Err(refuse("not owned by the current user"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refuse("readable by group or others (chmod 600)"));
    }
    let file = fs::File::open(path)
        .map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
    // The path must not have been swapped between the check and the open.
    let opened = file.metadata().map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
    if (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
        return Err(refuse("changed while being read"));
    }
    let mut text = String::new();
    file.take(4096)
        .read_to_string(&mut text)
        .map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
    let token = text.trim();
    if token.is_empty() {
        return Err(refuse("empty"));
    }
    Ok(Some(token.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-test-{}", crate::auth::oauth::random_b64(8)));
        fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn token_file_round_trip_and_checks() {
        let dir = tmp_dir();
        let path = dir.join(TOKEN_FILE);
        assert_eq!(read_token_file(&path).unwrap(), None);

        write_private(&path, b"1//refresh").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(read_token_file(&path).unwrap().as_deref(), Some("1//refresh"));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token_file(&path).unwrap_err().message.contains("group or others"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let link = dir.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_token_file(&link).unwrap_err().message.contains("not a regular file"));

        fs::write(&path, "  \n").unwrap();
        assert!(read_token_file(&path).unwrap_err().message.contains("empty"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_without_token_store_means_keyring() {
        let c: Config = serde_json::from_str(r#"{"account":"a@b.c"}"#).unwrap();
        assert_eq!(c.token_store, TokenStore::Keyring);
        let c: Config = serde_json::from_str(r#"{"account":"a@b.c","token_store":"file"}"#).unwrap();
        assert_eq!(c.token_store, TokenStore::File);
    }

    #[test]
    fn parses_desktop_client_json() {
        let c = parse_client_json(
            r#"{"installed":{"client_id":"123-abc.apps.googleusercontent.com","client_secret":"GOCSPX-x","redirect_uris":["http://localhost"]}}"#,
        )
        .unwrap();
        assert_eq!(c.client_id, "123-abc.apps.googleusercontent.com");
        assert_eq!(c.client_secret, "GOCSPX-x");
    }

    #[test]
    fn rejects_web_client_and_garbage() {
        let err = parse_client_json(r#"{"web":{"client_id":"x"}}"#).unwrap_err();
        assert!(err.hint.unwrap().contains("Desktop app"));
        assert!(parse_client_json("nope").is_err());
        assert!(
            parse_client_json(r#"{"installed":{"client_id":"evil","client_secret":"s"}}"#)
                .is_err()
        );
    }
}

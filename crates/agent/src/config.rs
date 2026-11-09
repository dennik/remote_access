//! On-disk agent state: TLS identity, authorized viewer keys and the connection log.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::SystemTime;

pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint: String,
}

/// `dir` holds the identity and log. `keys_dir` holds the authorized keys, which
/// on macOS live in a root-owned folder the agent's user can read but not change.
#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
    keys_dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { keys_dir: dir.clone(), dir }
    }

    #[cfg_attr(windows, allow(dead_code))]
    pub fn with_dirs(dir: PathBuf, keys_dir: PathBuf) -> Self {
        Self { dir, keys_dir }
    }

    /// The installed location. `install` restricts this directory to SYSTEM and Administrators.
    #[cfg(windows)]
    pub fn system() -> Self {
        let root = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        Self::new(PathBuf::from(root).join("RemoteAccess"))
    }

    /// The running agent's store, for the user launchd started it as.
    #[cfg(target_os = "macos")]
    pub fn mac_user() -> Result<Self> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(Self::with_dirs(mac_user_dir(std::path::Path::new(&home)), mac_shared_dir()))
    }

    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// Loads the TLS identity, creating it on first use.
    pub fn identity(&self) -> Result<Identity> {
        let cert_path = self.dir.join("agent.crt");
        let key_path = self.dir.join("agent.key");
        if !(cert_path.exists() && key_path.exists()) {
            fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
            let cert = rcgen::generate_simple_self_signed(vec![rd_common::tls::SERVER_NAME.to_string()])?;
            fs::write(&key_path, cert.signing_key.serialize_der())?;
            fs::write(&cert_path, cert.cert.der())?;
        }
        let cert_der = fs::read(&cert_path)?;
        let key_der = fs::read(&key_path)?;
        let fingerprint = rd_common::tls::fingerprint(&cert_der);
        Ok(Identity { cert_der, key_der, fingerprint })
    }

    pub fn authorized_keys(&self) -> Result<Vec<[u8; 32]>> {
        let path = self.keys_dir.join("authorized_keys");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        text.lines().map(str::trim).filter(|l| !l.is_empty()).map(parse_key).collect()
    }

    /// Adds keys to the authorized list, ignoring any already present.
    pub fn add_authorized_keys(&self, keys: &[String]) -> Result<()> {
        fs::create_dir_all(&self.keys_dir)?;
        let mut all = self.authorized_keys()?;
        for key in keys {
            let key = parse_key(key.trim())?;
            if !all.contains(&key) {
                all.push(key);
            }
        }
        let text: String = all.iter().map(|k| hex::encode(k) + "\n").collect();
        fs::write(self.keys_dir.join("authorized_keys"), text)?;
        Ok(())
    }

    /// Appends to the connection log. Failure to log never blocks a session.
    pub fn log(&self, line: &str) {
        let stamp = humantime::format_rfc3339_seconds(SystemTime::now());
        let path = self.dir.join("connections.log");
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{stamp} {line}");
        }
    }
}

pub fn parse_key(text: &str) -> Result<[u8; 32]> {
    let Ok(bytes) = hex::decode(text) else { bail!("admin key is not valid hex: {text}") };
    let Ok(key) = <[u8; 32]>::try_from(bytes) else { bail!("admin key must be 64 hex characters: {text}") };
    Ok(key)
}

#[cfg(windows)]
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
}

/// The name shown in Finder and AirDrop, which is what people recognise.
#[cfg(target_os = "macos")]
pub fn hostname() -> String {
    std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Program and authorized keys, owned by root.
#[cfg(target_os = "macos")]
pub fn mac_shared_dir() -> PathBuf {
    PathBuf::from("/Library/Application Support/RemoteAccess")
}

/// Identity and connection log, private to the user the agent runs as.
#[cfg(target_os = "macos")]
pub fn mac_user_dir(home: &std::path::Path) -> PathBuf {
    home.join("Library/Application Support/RemoteAccess")
}

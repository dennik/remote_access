//! The viewer's private key and saved computers, under %APPDATA%\RemoteAccess on
//! Windows and ~/Library/Application Support/RemoteAccess on macOS.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Host {
    pub name: String,
    pub addr: String,
    pub fingerprint: String,
    /// Network card address, learned while the computer is online, for Wake-on-LAN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

#[cfg(windows)]
fn data_root() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("APPDATA").ok_or_else(|| anyhow!("APPDATA is not set"))?))
}

#[cfg(target_os = "macos")]
fn data_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join("Library").join("Application Support"))
}

fn dir() -> Result<PathBuf> {
    let dir = data_root()?.join("RemoteAccess");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Loads the login key, creating it on first run. It controls every computer that
/// trusts this viewer, so it's encrypted with DPAPI to this Windows account.
#[cfg(windows)]
pub fn load_or_create_key() -> Result<[u8; 32]> {
    let path = dir()?.join("viewer.key");
    if path.exists() {
        let plain = dpapi(&fs::read(&path)?, false)?;
        let Ok(seed) = <[u8; 32]>::try_from(plain.as_slice()) else { bail!("{} is corrupt", path.display()) };
        return Ok(seed);
    }
    let seed = rd_common::auth::generate_seed()?;
    fs::write(&path, dpapi(&seed, true)?)?;
    Ok(seed)
}

#[cfg(windows)]
fn dpapi(data: &[u8], protect: bool) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        if protect {
            CryptProtectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)?;
        } else {
            CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)?;
        }
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(result)
    }
}

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "RemoteAccess viewer key";
#[cfg(target_os = "macos")]
const KEYCHAIN_ACCOUNT: &str = "viewer";
/// errSecItemNotFound
#[cfg(target_os = "macos")]
const NOT_FOUND: i32 = -25300;

/// Loads the login key, creating it on first run. It controls every computer that
/// trusts this viewer, so it lives in the login keychain rather than in a file.
#[cfg(target_os = "macos")]
pub fn load_or_create_key() -> Result<[u8; 32]> {
    use security_framework::passwords::{get_generic_password, set_generic_password};
    match get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
        Ok(stored) => {
            let seed = hex::decode(&stored).ok().and_then(|b| <[u8; 32]>::try_from(b).ok());
            seed.ok_or_else(|| anyhow!("the \"{KEYCHAIN_SERVICE}\" keychain item is corrupt"))
        }
        // Any other failure, such as a denied keychain prompt, must not replace the key.
        Err(e) if e.code() != NOT_FOUND => bail!("couldn't read the viewer key from the keychain: {e}"),
        Err(_) => {
            let seed = rd_common::auth::generate_seed()?;
            set_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, hex::encode(seed).as_bytes())
                .map_err(|e| anyhow!("couldn't save the viewer key to the keychain: {e}"))?;
            Ok(seed)
        }
    }
}

pub fn load_hosts() -> Vec<Host> {
    dir()
        .ok()
        .and_then(|d| fs::read(d.join("hosts.json")).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save_hosts(hosts: &[Host]) -> Result<()> {
    fs::write(dir()?.join("hosts.json"), serde_json::to_vec_pretty(hosts)?)?;
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trips_and_does_not_store_plaintext() {
        let secret = [42u8; 32];
        let sealed = dpapi(&secret, true).unwrap();
        assert!(!sealed.windows(32).any(|w| w == secret));
        assert_eq!(dpapi(&sealed, false).unwrap(), secret);
    }
}

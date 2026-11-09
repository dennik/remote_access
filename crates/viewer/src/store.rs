//! The viewer's private key and saved computers, under %APPDATA%\RemoteAccess.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Host {
    pub name: String,
    pub addr: String,
    pub fingerprint: String,
}

fn dir() -> Result<PathBuf> {
    let root = std::env::var_os("APPDATA").ok_or_else(|| anyhow!("APPDATA is not set"))?;
    let dir = PathBuf::from(root).join("RemoteAccess");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Loads the login key, creating it on first run. It controls every computer that
/// trusts this viewer, so it's encrypted with DPAPI to this Windows account.
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

fn dpapi(data: &[u8], protect: bool) -> Result<Vec<u8>> {
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

#[cfg(test)]
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

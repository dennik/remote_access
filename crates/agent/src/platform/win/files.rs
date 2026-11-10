//! Starting places for the file browser: the logged-in user's folders, then drives.

use rd_common::proto::DirEntry;
use std::ffi::OsStr;
use std::fs::Metadata;
use std::os::windows::fs::MetadataExt;
use windows::core::GUID;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::GetLogicalDrives;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Profile, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
};

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

pub fn is_hidden(_name: &OsStr, meta: &Metadata) -> bool {
    meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

/// The console user's token. The helper runs as SYSTEM, whose own folders would be
/// the wrong ones. Fails in console mode, where the current user is the right one.
fn console_user_token() -> Option<HANDLE> {
    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(WTSGetActiveConsoleSessionId(), &mut token) }.ok()?;
    Some(token)
}

fn known_folder(id: &GUID, token: Option<HANDLE>) -> Option<String> {
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, token).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text
    }
}

fn place(name: impl Into<String>, path: String) -> DirEntry {
    DirEntry { name: name.into(), path, is_dir: true, size: 0 }
}

pub fn places() -> Vec<DirEntry> {
    let mut list = Vec::new();
    let token = console_user_token();
    // Follows redirection, such as a Desktop moved into OneDrive.
    for (label, id) in [
        ("Desktop", &FOLDERID_Desktop),
        ("Documents", &FOLDERID_Documents),
        ("Downloads", &FOLDERID_Downloads),
        ("Home", &FOLDERID_Profile),
    ] {
        if let Some(path) = known_folder(id, token) {
            list.push(place(label, path));
        }
    }
    if let Some(token) = token {
        let _ = unsafe { CloseHandle(token) };
    }

    let drives = unsafe { GetLogicalDrives() };
    for (i, letter) in ('A'..='Z').enumerate() {
        if drives & (1 << i) != 0 {
            list.push(place(format!("Drive {letter}:"), format!("{letter}:\\")));
        }
    }
    list
}

//! Starting places for the file browser: the user's folders, then mounted volumes.

use rd_common::proto::DirEntry;
use std::ffi::OsStr;
use std::fs::Metadata;
use std::path::PathBuf;

pub fn is_hidden(name: &OsStr, _meta: &Metadata) -> bool {
    name.to_string_lossy().starts_with('.')
}

fn place(name: impl Into<String>, path: PathBuf) -> DirEntry {
    DirEntry { name: name.into(), path: path.display().to_string(), is_dir: true, size: 0 }
}

pub fn places() -> Vec<DirEntry> {
    let mut list = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for folder in ["Desktop", "Documents", "Downloads"] {
            let path = home.join(folder);
            if path.is_dir() {
                list.push(place(folder, path));
            }
        }
        list.push(place("Home", home));
    }
    // The startup disk and anything plugged in, such as USB drives.
    if let Ok(volumes) = std::fs::read_dir("/Volumes") {
        let mut names: Vec<_> = volumes.filter_map(|v| v.ok()).map(|v| v.file_name()).collect();
        names.sort();
        for name in names {
            list.push(place(name.to_string_lossy().to_string(), PathBuf::from("/Volumes").join(&name)));
        }
    }
    list
}

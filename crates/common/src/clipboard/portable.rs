//! Clipboard for macOS and other non-Windows systems, through arboard.
//!
//! There's no portable change counter, so `sequence_number` hashes the current text.
//! It only changes when the text does, which is all the callers need.

use super::MAX_CHARS;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

fn clipboard() -> Option<arboard::Clipboard> {
    arboard::Clipboard::new().ok()
}

pub fn sequence_number() -> u32 {
    let mut hasher = DefaultHasher::new();
    get_text().hash(&mut hasher);
    hasher.finish() as u32
}

pub fn get_text() -> Option<String> {
    let text = clipboard()?.get_text().ok()?;
    (text.chars().count() <= MAX_CHARS).then_some(text)
}

pub fn set_text(text: &str) -> bool {
    clipboard().is_some_and(|mut c| c.set_text(text.to_string()).is_ok())
}

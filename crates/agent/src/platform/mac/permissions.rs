//! The two privacy permissions the agent needs. macOS only lets a person grant
//! them, in System Settings → Privacy & Security.

use crate::config::Store;
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

pub fn request(store: &Store) {
    unsafe {
        if !CGPreflightScreenCaptureAccess() {
            CGRequestScreenCaptureAccess();
            store.log("Screen Recording permission is missing. Viewers will see a blank or wallpaper-only screen until it's granted.");
        }
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
        if !AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) {
            store.log("Accessibility permission is missing. Viewers can watch but not control until it's granted.");
        }
    }
}

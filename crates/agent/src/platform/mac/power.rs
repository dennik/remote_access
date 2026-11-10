//! Keeps the Mac reachable. Without this, a Mac set to sleep when idle stops
//! answering viewers until someone touches it, even though the agent is running.

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(kind: CFStringRef, level: u32, name: CFStringRef, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
    fn IOPMAssertionDeclareUserActivity(name: CFStringRef, user_type: u32, id: *mut u32) -> i32;
}

const LEVEL_ON: u32 = 255;
const USER_ACTIVE_LOCAL: u32 = 0;

/// A power assertion, released when dropped.
pub struct Assertion(u32);

impl Assertion {
    fn create(kind: &str, reason: &str) -> Option<Assertion> {
        let (kind, reason) = (CFString::new(kind), CFString::new(reason));
        let mut id = 0;
        let status = unsafe {
            IOPMAssertionCreateWithName(kind.as_concrete_TypeRef(), LEVEL_ON, reason.as_concrete_TypeRef(), &mut id)
        };
        (status == 0).then_some(Assertion(id))
    }

    /// Stops idle system sleep for as long as the agent runs. The display can
    /// still turn off, and closing a laptop lid still sleeps it.
    pub fn keep_awake() -> Option<Assertion> {
        Assertion::create("PreventUserIdleSystemSleep", "Remote access agent waiting for viewers")
    }

    /// Turns the display on, as a key press would, and keeps it on while a viewer is connected.
    pub fn viewer_connected() -> Option<Assertion> {
        let reason = CFString::new("Remote access viewer connected");
        let mut activity = 0;
        unsafe { IOPMAssertionDeclareUserActivity(reason.as_concrete_TypeRef(), USER_ACTIVE_LOCAL, &mut activity) };
        // The activity assertion times out by itself; this one holds for the session.
        let held = Assertion::create("PreventUserIdleDisplaySleep", "Remote access viewer connected");
        unsafe { IOPMAssertionRelease(activity) };
        held
    }
}

impl Drop for Assertion {
    fn drop(&mut self) {
        unsafe { IOPMAssertionRelease(self.0) };
    }
}

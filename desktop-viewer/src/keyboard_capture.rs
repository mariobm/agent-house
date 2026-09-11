//! Main-thread, app-scoped symbolic-hotkey suppression. No global event tap.
#[derive(Default)]
pub struct KeyboardCapture {
    #[cfg(target_os = "macos")]
    token: Option<std::ptr::NonNull<std::ffi::c_void>>,
}
impl KeyboardCapture {
    /// Returns true only when macOS permission allows native suppression.
    pub fn enable(&mut self) -> bool {
        self.release();
        #[cfg(target_os = "macos")]
        unsafe {
            if !AXIsProcessTrusted() {
                return false;
            }
            // Preserve accessibility shortcuts, even during capture.
            self.token = std::ptr::NonNull::new(PushSymbolicHotKeyMode(1 << 1));
            self.token.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        false
    }
    pub fn release(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(token) = self.token.take() {
            // Called on the same main event-loop thread as enable().
            unsafe { PopSymbolicHotKeyMode(token.as_ptr()) };
        }
    }
}
impl Drop for KeyboardCapture {
    fn drop(&mut self) {
        self.release();
    }
}
#[cfg(target_os = "macos")]
#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn PushSymbolicHotKeyMode(options: u32) -> *mut std::ffi::c_void;
    fn PopSymbolicHotKeyMode(token: *mut std::ffi::c_void);
}
#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

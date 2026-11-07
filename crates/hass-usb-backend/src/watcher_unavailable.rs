use std::path::PathBuf;

use crate::{WatcherError, WatcherStartError};

/// The pinned watcher is inotify-only; Darwin uses Home Assistant's polling
/// fallback after this exact startup condition.
pub struct NativeSerialPortWatcher {
    _root: PathBuf,
}

impl NativeSerialPortWatcher {
    pub fn new() -> Self {
        Self::with_root("/dev/bus/usb")
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { _root: root.into() }
    }

    pub fn start_watching(
        &self,
        _callback: crate::UsbPortChangeCallback,
    ) -> Result<(), WatcherStartError> {
        Err(WatcherStartError::InotifyNotAvailable)
    }

    pub fn stop_and_join(&self) -> Result<(), WatcherError> {
        Ok(())
    }
}

impl Default for NativeSerialPortWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NativeSerialPortWatcher {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

use std::{fmt, io};

#[derive(Debug)]
pub enum UsbBackendError {
    Io { path: String, source: io::Error },
    InvalidUtf8 { path: String },
    InvalidSysfsValue { path: String, value: String },
    UnsupportedPlatform(&'static str),
    Native(String),
}

impl fmt::Display for UsbBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{path}: {source}"),
            Self::InvalidUtf8 { path } => write!(f, "{path}: sysfs value is not UTF-8"),
            Self::InvalidSysfsValue { path, value } => {
                write!(f, "{path}: invalid sysfs value {value:?}")
            }
            Self::UnsupportedPlatform(message) => f.write_str(message),
            Self::Native(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for UsbBackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum WatcherStartError {
    /// The platform cannot provide the pinned inotify watcher. The application
    /// may select its documented five-second polling fallback for this error.
    InotifyNotAvailable,
    Io(io::Error),
    AlreadyStarted,
}

impl fmt::Display for WatcherStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InotifyNotAvailable => f.write_str("inotify is not available on this platform"),
            Self::Io(error) => write!(f, "failed to start USB filesystem watcher: {error}"),
            Self::AlreadyStarted => f.write_str("USB filesystem watcher is already started"),
        }
    }
}

impl std::error::Error for WatcherStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum WatcherError {
    Io(io::Error),
    WorkerPanicked,
}

impl fmt::Display for WatcherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "USB filesystem watcher failed: {error}"),
            Self::WorkerPanicked => f.write_str("USB filesystem watcher worker panicked"),
        }
    }
}

impl std::error::Error for WatcherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::WorkerPanicked => None,
        }
    }
}

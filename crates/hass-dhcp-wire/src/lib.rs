//! Safe DHCP packet decoding and an owned passive capture boundary.
//!
//! The pinned Python watcher installs the BPF filter `udp and (port 67 or
//! 68)`. `pnet_datalink` does not expose a BPF-filter API, so this provider
//! receives broader frames and applies the same UDP-port admission in the
//! decoder before it produces an observation. Captures are non-promiscuous.
//! No raw frames are logged or retained after decoding.

#![forbid(unsafe_code)]

mod capture;
mod decode;

pub use capture::{
    CaptureBackend, CaptureConfig, CaptureHandle, CaptureInterface, CaptureShutdownError,
    CaptureSource, CaptureStartError, CaptureStatus, CaptureWorkerError, CaptureWorkerErrorKind,
    InterfaceSelector, PnetCaptureBackend, WeakCaptureHandle, start_capture,
};
pub use decode::{DecodeError, WireObservation, decode_ethernet_frame};

//! Shared Home Assistant Zeroconf discovery manager SDK.
//!
//! The SDK owns discovery resolution, matcher dispatch, and listener lifetimes.
//! A runtime adapter supplies cache and asynchronous resolution operations; the
//! native mDNS transport is owned separately by the `backend` adapter.

#![forbid(unsafe_code)]

pub mod backend;
pub mod homekit;
pub mod manager;
pub mod matcher;
pub mod models;

pub use homekit::{
    FlowSource, HOMEKIT_SERVICE_TYPES, HomeKitDecision, HomeKitFlowDecision, HomeKitRegistry,
    OrdinaryFlowDecision, classify_homekit,
};
pub use manager::{
    DiscoveryAction, DiscoveryActionSink, DiscoverySource, HOMEKIT_SOURCE, ListenerGuard,
    REQUEST_TIMEOUT, RawServiceRemovedListener, RawServiceUpdateListener, ResolverFuture,
    ServiceStateChange, ZEROCONF_SOURCE, ZeroconfBackend, ZeroconfIntegration, ZeroconfManager,
};
pub use matcher::{MatcherIndex, ZeroconfMatcher};
pub use models::{
    RawResolvedServiceInfo, RawTxtEntry, ScopedIpAddress, ZeroconfProperties, ZeroconfProperty,
    ZeroconfServiceInfo,
};

/// Task-facing name for the raw, completed DNS-SD record model.
pub type RawZeroconfServiceInfo = RawResolvedServiceInfo;

/// Home Assistant's own service type, browsed even when no ordinary matcher exists.
pub const HOME_ASSISTANT_SERVICE_TYPE: &str = "_home-assistant._tcp.local.";

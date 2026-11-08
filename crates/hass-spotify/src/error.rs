use std::time::Duration;

use thiserror::Error;

use crate::AuthorizationError;

/// Sanitized reason for a Spotify `403` response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForbiddenReason {
    /// Spotify explicitly described a Premium-only operation.
    PremiumRequired,
    /// Spotify explicitly described a missing OAuth scope.
    MissingScope,
    /// Spotify explicitly described an app-user or development-mode restriction.
    AppAccessRestricted,
    /// The response did not provide a recognized reason.
    Unknown,
}

/// Sanitized reason for a Spotify `429` response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateLimitReason {
    /// The response identifies the developer-account quota restriction.
    QuotaExceeded,
    /// The response is a rate limit without a recognized quota reason.
    Unknown,
}

/// Spotify transport, authorization, and protocol failures.
///
/// Variants never retain provider response text, request URLs, endpoint query
/// values, or bearer tokens.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SpotifyError {
    #[error("Spotify authorization requires reauthentication")]
    Unauthorized,
    #[error("Spotify denied the request: {reason:?}")]
    Forbidden { reason: ForbiddenReason },
    #[error("Spotify resource was not found")]
    NotFound,
    #[error("Spotify rate limit reached ({reason:?}); retry after {retry_after:?}")]
    RateLimited {
        retry_after: Option<Duration>,
        reason: RateLimitReason,
    },
    #[error("Spotify temporarily failed the request (HTTP {status})")]
    Transient { status: u16 },
    #[error("Spotify transport is temporarily unavailable")]
    Transport,
    #[error("Spotify request timed out")]
    Timeout,
    #[error("Spotify returned an invalid payload")]
    InvalidPayload,
    #[error("Spotify rejected the request as invalid")]
    InvalidRequest,
    #[error("Spotify returned an unexpected HTTP status ({status})")]
    UnexpectedStatus { status: u16 },
    #[error("Spotify response exceeded the configured size bound")]
    ResponseTooLarge,
    #[error("Spotify request body exceeded the configured size bound")]
    RequestTooLarge,
    #[error("Spotify API base URL is invalid or unsafe")]
    InvalidBaseUrl,
    #[error("Spotify pagination URL left the configured API origin")]
    UnsafePaginationUrl,
    #[error("Spotify pagination points to a previously fetched page")]
    PaginationLoop,
    #[error("Spotify pagination ended before all advertised results were fetched")]
    PaginationIncomplete,
    #[error("Spotify pagination exceeded the configured page or item bound")]
    PaginationLimitExceeded,
    #[error("Spotify returned a redirect, which is not followed for credentialed requests")]
    RedirectBlocked,
    #[error(transparent)]
    Authorization(#[from] AuthorizationError),
}

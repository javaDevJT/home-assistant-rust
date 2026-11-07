//! Callback origin selected from saved core settings or the actual local listener.

use hass_config::CoreConfig;
use hass_core::{Error, Result};
use std::net::SocketAddr;
use url::{Host, Url};

pub(crate) fn callback_url(core: &CoreConfig, bound: SocketAddr) -> Result<Option<Url>> {
    let configured = core
        .external_url
        .as_deref()
        .or(core.internal_url.as_deref());
    let mut base = if let Some(configured) = configured {
        if configured
            .split_once("://")
            .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
            .is_some_and(|authority| authority.contains('@'))
        {
            return Err(invalid_origin());
        }
        Url::parse(configured).map_err(|_| invalid_origin())?
    } else if bound.ip().is_loopback() {
        Url::parse(&format!("http://{bound}")).map_err(|_| invalid_origin())?
    } else {
        return Ok(None);
    };
    let loopback_http = base.scheme() == "http"
        && match base.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
    if base.host().is_none()
        || (!loopback_http && base.scheme() != "https")
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(invalid_origin());
    }
    let path = format!(
        "{}/auth/external/callback",
        base.path().trim_end_matches('/')
    );
    base.set_path(&path);
    Ok(Some(base))
}

fn invalid_origin() -> Error {
    Error::InvalidInput("OAuth callback origin must be HTTPS or literal loopback HTTP, without user information, query or fragment".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_actual_loopback_listener_without_inventing_a_remote_origin() {
        let core = CoreConfig::default();
        assert_eq!(
            callback_url(&core, "127.0.0.1:39123".parse().unwrap())
                .unwrap()
                .unwrap()
                .as_str(),
            "http://127.0.0.1:39123/auth/external/callback"
        );
        assert_eq!(
            callback_url(&core, "[::1]:39124".parse().unwrap())
                .unwrap()
                .unwrap()
                .as_str(),
            "http://[::1]:39124/auth/external/callback"
        );
        assert!(
            callback_url(&core, "0.0.0.0:8123".parse().unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn configured_origin_preserves_prefix_and_external_precedence() {
        let mut core = CoreConfig {
            external_url: Some("https://ha.example.test/base/".into()),
            internal_url: Some("https://internal.example.test/".into()),
            ..CoreConfig::default()
        };
        let bound = "127.0.0.1:8123".parse().unwrap();
        assert_eq!(
            callback_url(&core, bound).unwrap().unwrap().as_str(),
            "https://ha.example.test/base/auth/external/callback"
        );
        core.external_url = None;
        assert_eq!(
            callback_url(&core, bound).unwrap().unwrap().host_str(),
            Some("internal.example.test")
        );
    }

    #[test]
    fn rejects_unsafe_configured_origins_without_echoing_the_input() {
        for value in [
            "http://remote.example.test/",
            "https://user:sentinel-secret@ha.example.test/",
            "https://@ha.example.test/",
            "https://ha.example.test/?key=sentinel-secret",
            "https://ha.example.test/#sentinel-secret",
        ] {
            let core = CoreConfig {
                external_url: Some(value.into()),
                ..CoreConfig::default()
            };
            let error = callback_url(&core, "127.0.0.1:8123".parse().unwrap()).unwrap_err();
            assert!(!error.to_string().contains("sentinel-secret"));
        }
    }
}

//! WebSocket connection.
//! SPDX-License-Identifier: Apache-2.0 OR GPL-3.0-or-later

use crate::proto_version::PROTOCOL_VERSION;
use http::header::{HeaderName, HeaderValue};
use rustls::{
    client::{ServerCertVerified, ServerCertVerifier},
    ClientConfig, RootCertStore,
};
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::{client_async_with_config, Connector, MaybeTlsStream, WebSocketStream};
use tracing::debug;
use tungstenite::{client::IntoClientRequest, handshake::client::Request};
use url::Url;

/// Error type for WebSocket connection.
#[derive(Error, Debug)]
pub enum Error {
    #[error("failed to read CA store: {0}")]
    CaStoreIO(#[from] std::io::Error),
    #[error("failed to parse CA store: {0}")]
    CaStoreParse(#[from] webpki::Error),
    #[error("failed to connect to server: {0}")]
    Connect(tokio::io::Error),
    #[error("rustls error: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("tungstenite error: {0}")]
    Tungstenite(#[from] tungstenite::error::Error),
    #[error("failed to parse URL: {0}")]
    UrlParse(#[from] url::ParseError),
    #[error("incorrect scheme: {0}")]
    IncorrectScheme(String),
    #[error("invalid header value or hostname: {0}")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("invalid header name: {0}")]
    InvalidHeaderName(#[from] http::header::InvalidHeaderName),
    #[error("invalid header: {0}")]
    InvalidHeaderFormat(String),
}

/// Types of proxies.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProxyType {
    /// No proxy.
    None,
    /// HTTP proxy.
    Http,
    /// HTTPS proxy (i.e. TLS-encrypted HTTP proxy instead of proxy for HTTPS).
    Https,
    /// SOCKS5 proxy.
    Socks,
}

pub struct TlsEmptyVerifier {}

impl ServerCertVerifier for TlsEmptyVerifier {
    fn verify_server_cert(
        &self,
        _: &rustls::Certificate,
        _: &[rustls::Certificate],
        _: &rustls::client::ServerName,
        _: &mut dyn Iterator<Item = &[u8]>,
        _: &[u8],
        _: std::time::SystemTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
}

/// Load system certificates
#[cfg(all(feature = "rustls-native-roots", not(feature = "rustls-webpki-roots")))]
fn get_system_certs() -> Result<RootCertStore, Error> {
    let mut roots = RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs()? {
        roots.add(&rustls::Certificate(cert.0))?;
    }
    Ok(roots)
}
#[cfg(feature = "rustls-webpki-roots")]
fn get_system_certs() -> Result<RootCertStore, Error> {
    let mut roots = RootCertStore::empty();
    roots.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
        rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
            ta.subject,
            ta.spki,
            ta.name_constraints,
        )
    }));
    Ok(roots)
}

/// Load system certificates or a custom CA store.
fn generate_rustls_rootcertstore(custom_ca: Option<&str>) -> Result<RootCertStore, Error> {
    // Whether to use a custom CA store.
    if let Some(ca) = custom_ca {
        let mut roots = RootCertStore::empty();
        let mut reader = std::io::BufReader::new(std::fs::File::open(ca)?);
        for cert in rustls_pemfile::certs(&mut reader)? {
            roots.add(&rustls::Certificate(cert))?;
        }
        Ok(roots)
    } else {
        get_system_certs()
    }
}

/// Load client certificate if provided.
fn try_load_client_certificate(
    tls_key: Option<&str>,
    tls_cert: Option<&str>,
) -> Result<Option<(Vec<rustls::Certificate>, rustls::PrivateKey)>, Error> {
    if let (Some(key), Some(cert)) = (tls_key, tls_cert) {
        let mut reader = std::io::BufReader::new(std::fs::File::open(cert)?);
        let certs = rustls_pemfile::certs(&mut reader)?
            .into_iter()
            .map(rustls::Certificate)
            .collect();
        let mut reader = std::io::BufReader::new(std::fs::File::open(key)?);
        let keys = rustls_pemfile::rsa_private_keys(&mut reader)?;
        Ok(Some((certs, rustls::PrivateKey(keys[0].clone()))))
    } else {
        Ok(None)
    }
}

/// Sanitize the URL for WebSocket.
fn sanitize_url(url: &str) -> Result<Url, Error> {
    // Provide a default scheme if none is provided.
    let url = Url::parse(url).or_else(|e| {
        if e == url::ParseError::RelativeUrlWithoutBase {
            Url::parse(&format!("http://{url}"))
        } else {
            Err(e)
        }
    })?;
    // Convert to a `Url`.
    Ok(match url.scheme() {
        "wss" => url,
        "ws" => url,
        "https" => {
            let mut url = url;
            url.set_scheme("wss").unwrap();
            url
        }
        "http" => {
            let mut url = url;
            url.set_scheme("ws").unwrap();
            url
        }
        scheme => {
            return Err(Error::IncorrectScheme(scheme.to_string()));
        }
    })
}

/// Create a `Connector` for `WebSocketStream`.
fn get_connector(
    is_tls: bool,
    tls_ca: Option<&str>,
    tls_cert: Option<&str>,
    tls_key: Option<&str>,
    tls_insecure: bool,
) -> Result<Connector, Error> {
    if is_tls {
        let config_builder = ClientConfig::builder().with_safe_defaults();
        // Whether there is a custom CA store
        let roots = generate_rustls_rootcertstore(tls_ca)?;
        let client_certificate = try_load_client_certificate(tls_key, tls_cert)?;
        // Whether to skip TLS verification and whether there is a client certificate
        let config = match (tls_insecure, client_certificate) {
            (true, Some((cert_chain, key_der))) => config_builder
                .with_custom_certificate_verifier(Arc::new(TlsEmptyVerifier {}))
                .with_single_cert(cert_chain, key_der)?,
            (true, None) => config_builder
                .with_custom_certificate_verifier(Arc::new(TlsEmptyVerifier {}))
                .with_no_client_auth(),
            (false, Some((cert_chain, key_der))) => config_builder
                .with_root_certificates(roots)
                .with_single_cert(cert_chain, key_der)?,
            (false, None) => config_builder
                .with_root_certificates(roots)
                .with_no_client_auth(),
        };
        Ok(Connector::Rustls(config.into()))
    } else {
        // No TLS
        Ok(Connector::Plain)
    }
}

/// Find out which type of proxy we are using.
fn get_proxy_type(proxy: &Option<Url>) -> Result<ProxyType, Error> {
    if proxy.is_none() {
        return Ok(ProxyType::None);
    }
    match proxy.as_ref().unwrap().scheme() {
        "http" => Ok(ProxyType::Http),
        "https" => Ok(ProxyType::Https),
        "socks5" | "socks5h" | "socks4" | "socks4a" => Ok(ProxyType::Socks),
        scheme => Err(Error::IncorrectScheme(scheme.to_string())),
    }
}

/// Perform a WebSocket handshake.
/// Refactored from `client_main` and I know it's ugly.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(level = "debug", skip(extra_headers))]
pub async fn handshake(
    server_url: &str,
    proxy: &Option<Url>,
    ws_psk: Option<&str>,
    override_hostname: Option<&str>,
    extra_headers: Vec<String>,
    sni: Option<&str>,
    tls_ca: Option<&str>,
    tls_key: Option<&str>,
    tls_cert: Option<&str>,
    tls_insecure: bool,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, Error> {
    // Check the proxy scheme.
    let proxy_type = get_proxy_type(proxy)?;
    // Clean up the URL.
    let url = sanitize_url(server_url)?;
    // Find out the port to connect to.
    // `unwrap` is safe because we already checked the scheme.
    // and `Url` guarantees that they know the port.
    let port = url.port_or_known_default().unwrap();
    // We already sanitized https URLs to wss
    let is_tls = url.scheme() == "wss";

    // This host is used for `connect` and the user can override
    // the one used for the Host header and SNI.
    // `unwrap` is safe because we already checked the scheme.
    // XXX: Prove me wrong.
    let connect_host = url.domain().unwrap().to_string();
    let mut tls_domain = connect_host.clone();
    // Use a request to allow additional headers
    let mut req: Request = url.into_client_request()?;
    let req_headers = req.headers_mut();
    // Add protocol version
    req_headers.insert(
        "sec-websocket-protocol",
        HeaderValue::from_str(PROTOCOL_VERSION)?,
    );
    // Add PSK
    if let Some(ws_psk) = ws_psk {
        req_headers.insert("x-penguin-psk", HeaderValue::from_str(ws_psk)?);
    }
    // Potentially add custom hostname
    if let Some(hostname) = override_hostname {
        req_headers.insert("host", HeaderValue::from_str(hostname)?);
        tls_domain = hostname.to_string();
    }
    // Potentially set custom SNI
    if let Some(sni) = sni {
        tls_domain = sni.to_string();
    }
    // Now add custom headers
    for header in &extra_headers {
        let (name, value) = header
            .split_once(':')
            .ok_or(Error::InvalidHeaderFormat(header.to_string()))?;
        req_headers.insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(value.trim())?,
        );
    }

    let connector = get_connector(is_tls, tls_ca, tls_cert, tls_key, tls_insecure)?;
    // Connect the TCP socket
    match proxy_type {
        ProxyType::None => {}
        ProxyType::Http => {
        }
        ProxyType::Https => {
        }
        ProxyType::Socks => {
        }
    }
    let addr = format!("{}:{}", connect_host, port);
    let try_socket = TcpStream::connect(addr).await;
    let socket = try_socket.map_err(Error::Connect)?;
    let (ws_stream, _resp) = match connector {
        Connector::Rustls(conn) => {
            let connector = ClientConnection
        }
        Connector::Plain => client_async_with_config(req, socket, None).await,
        _ => unreachable!("Should have been handled by `get_connector`"),
    };
    // We don't need to check the response now...
    debug!("WebSocket handshake succeeded");
    Ok(ws_stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::generate_simple_self_signed;
    use tempfile::tempdir;

    #[test]
    fn test_sanitize_url() {
        assert_eq!(
            sanitize_url("wss://example.com").unwrap().as_str(),
            "wss://example.com/"
        );
        assert_eq!(
            sanitize_url("ws://example.com").unwrap().as_str(),
            "ws://example.com/"
        );
        assert_eq!(
            sanitize_url("https://example.com").unwrap().as_str(),
            "wss://example.com/"
        );
        assert_eq!(
            sanitize_url("http://example.com").unwrap().as_str(),
            "ws://example.com/"
        );
        assert!(sanitize_url("ftp://example.com").is_err());
    }

    #[test]
    fn test_generate_rustls_rootcertstore() {
        // No custom CA store
        let sys_root = generate_rustls_rootcertstore(None);
        assert!(sys_root.is_ok());
        assert!(!sys_root.unwrap().is_empty());
        // Custom CA store
        let tmpdir = tempdir().unwrap();
        let ca_path = tmpdir.path().join("ca.pem");
        let custom_ca = generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        std::fs::write(&ca_path, custom_ca.serialize_pem().unwrap()).unwrap();
        let custom_root = generate_rustls_rootcertstore(Some(ca_path.to_str().unwrap()));
        assert!(custom_root.is_ok());
        assert_eq!(custom_root.unwrap().len(), 1);
    }
}

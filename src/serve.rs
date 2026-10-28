//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Same-port HTTP/HTTPS dispatch on the TLS listener (ANT-652).
//!
//! The server runs a single TLS listener. To bounce plain-HTTP clients
//! (e.g. `http://host:443`) to the working HTTPS URL instead of a TLS
//! handshake error, each new TCP connection is sniffed on its first byte
//! before dispatch (peeked, not consumed):
//!
//! - the first byte of a TLS ClientHello is a handshake record (`0x16`)
//!   → rustls handshake, then the main app;
//! - any other first byte → treat the stream as plain HTTP/1.1: read the
//!   request line and the `Host` header, answer `301 https://{Host}{uri}`.
//!
//! This is the same one-port HTTP+HTTPS technique Caddy, h2o, and nginx
//! (`stream` + `ssl_preread`) use. The TLS branch draws a fresh
//! `TlsAcceptor` from the shared [`RustlsConfig`] (`Arc<ArcSwap
//! <ServerConfig>>`) at handshake time, so the ACME hot-reload path
//! (`acme_orchestrator::reload_served_cert`) swaps the served cert exactly
//! as before.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use axum::http::Request;
use axum::response::Response;
use axum_server::tls_rustls::RustlsConfig;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsAcceptor;
use tower::Service;
use tracing::{debug, trace, warn};

/// The first byte of a TLS `handshake` record — what a ClientHello is.
pub const TLS_HANDSHAKE_BYTE: u8 = 0x16;

/// Cap on the header bytes read from a plain-HTTP connection before
/// giving up on it (a stalled client is reaped by the per-read timeout).
const PLAINTEXT_HEAD_CAP: usize = 16 * 1024;

/// Max wait for the first byte, the TLS handshake, or the plain-HTTP
/// request head. Matches the 10 s handshake timeout axum-server applied.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(10);

/// Classify a connection from its first byte (peeked, not consumed): a
/// TLS ClientHello begins with a handshake record; anything else is
/// treated as plain HTTP.
pub fn is_tls_first_byte(byte: u8) -> bool {
    byte == TLS_HANDSHAKE_BYTE
}

/// The `Location` value for the redirect: `https://{host}{uri}`. The
/// `Host` header carries the externally visible host:port (including a
/// non-443 host port mapped onto the container's 443), so the target
/// stays correct behind a port mapping.
pub fn redirect_location(host: &str, uri: &str) -> String {
    format!("https://{host}{uri}")
}

/// The full raw HTTP/1.1 `301` response answered to a plain-HTTP
/// connection (the connection is closed once the client has read it).
/// HSTS carries the same value as the security-headers middleware so a
/// client that stores it keeps using HTTPS.
pub fn redirect_response(host: &str, uri: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 301 Moved Permanently\r\n\
         Location: {}\r\n\
         Strict-Transport-Security: max-age=63072000; includeSubDomains\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n",
        redirect_location(host, uri)
    )
    .into_bytes()
}

/// Parse a plain-HTTP request head (up to the blank line) into
/// `(method, target, host)`. `host` is the `Host` header value or
/// `None` when the head has none; `target` falls back to `/` when the
/// request line has no second token.
pub fn parse_http_head(head: &str) -> (String, String, Option<String>) {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or("/");
    let mut host = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("host") && !value.trim().is_empty() {
                host = Some(value.trim().to_string());
                break;
            }
        }
    }
    (method.to_string(), target.to_string(), host)
}

/// Handle one accepted connection. The first byte is peeked (not
/// consumed): a TLS ClientHello goes through the rustls handshake and is
/// served by the main app; when `redirect_enabled`, any other first byte
/// is answered with a 301 redirect to HTTPS. When the flag is off, every
/// connection goes to the handshake — today's behavior.
pub async fn handle_connection<S>(
    mut socket: TcpStream,
    peer: SocketAddr,
    service: S,
    tls_config: RustlsConfig,
    redirect_enabled: bool,
) where
    S: Service<Request<Incoming>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut first = [0u8; 1];
    let peeked = tokio::time::timeout(FIRST_BYTE_TIMEOUT, socket.peek(&mut first)).await;
    match peeked {
        Ok(Ok(n)) if n > 0 => {}
        Ok(Ok(_)) => return,
        Ok(Err(e)) => {
            trace!("first-byte peek from {peer} failed: {e}");
            return;
        }
        Err(_) => {
            trace!("first-byte peek from {peer} timed out");
            return;
        }
    }

    if redirect_enabled && !is_tls_first_byte(first[0]) {
        serve_plain_http(&mut socket, peer).await;
        return;
    }

    // TLS branch (also the fallback for every byte when the redirect is
    // off). A fresh acceptor is drawn from the *current* config, so an
    // ACME hot-reload takes effect on the next handshake.
    let server_config = tls_config.get_inner();
    let acceptor = TlsAcceptor::from(server_config);
    match tokio::time::timeout(FIRST_BYTE_TIMEOUT, acceptor.accept(socket)).await {
        Ok(Ok(stream)) => {
            let io = TokioIo::new(stream);
            let hyper_service = TowerToHyperService::new(service);
            let builder = Builder::new(TokioExecutor::new());
            if let Err(e) = builder
                .serve_connection_with_upgrades(io, hyper_service)
                .await
            {
                debug!("connection {peer} closed: {e}");
            }
        }
        Ok(Err(e)) => debug!("TLS handshake from {peer} failed: {e}"),
        Err(_) => debug!("TLS handshake from {peer} timed out"),
    }
}

/// The plain-HTTP branch: read the request head, answer
/// `301 https://{Host}{uri}`, close the connection. The response body is
/// empty and `Connection: close` is set, so no body is drained.
async fn serve_plain_http(socket: &mut TcpStream, peer: SocketAddr) {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head: String = loop {
        match tokio::time::timeout(FIRST_BYTE_TIMEOUT, socket.read(&mut chunk)).await {
            Ok(Ok(0)) => break String::from_utf8_lossy(&buf).into_owned(),
            Ok(Ok(n)) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= PLAINTEXT_HEAD_CAP {
                    break String::from_utf8_lossy(&buf).into_owned();
                }
            }
            Ok(Err(e)) => {
                trace!("plain-HTTP head from {peer} failed: {e}");
                break String::from_utf8_lossy(&buf).into_owned();
            }
            Err(_) => {
                trace!("plain-HTTP head from {peer} timed out");
                break String::from_utf8_lossy(&buf).into_owned();
            }
        }
    };

    let (method, target, host) = parse_http_head(&head);
    let host = host.unwrap_or_else(|| peer.to_string());
    debug!("plain-HTTP {method} {target} from {peer}; redirecting to https://{host}{target}");

    let response = redirect_response(&host, &target);
    if socket.write_all(&response).await.is_err() || socket.shutdown().await.is_err() {
        trace!("failed to write the 301 redirect to {peer}");
    }
}

/// The accept loop behind the single TLS listener. Each accepted
/// connection is dispatched by [`handle_connection`] on a spawned task;
/// the loop itself never blocks on a client. Returns when the listener
/// errors (the process exits after a signal, as before).
pub async fn serve_loop<M, S>(
    listener: TcpListener,
    make_service: M,
    tls_config: RustlsConfig,
    redirect_enabled: bool,
) -> anyhow::Result<()>
where
    M: Service<SocketAddr, Response = S, Error = Infallible>,
    S: Service<Request<Incoming>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut make_service = make_service;
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("accept failed: {e}");
                continue;
            }
        };
        let service = match make_service.call(peer).await {
            Ok(service) => service,
            Err(e) => {
                warn!("building the per-connection service failed: {e}");
                continue;
            }
        };
        tokio::spawn(handle_connection(
            socket,
            peer,
            service,
            tls_config.clone(),
            redirect_enabled,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};
    use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
    use std::sync::{Arc, Once};
    use std::time::Duration;
    use tokio_rustls::TlsConnector;

    /// Install the rustls crypto provider (ring) once per process; required
    /// before any `ServerConfig`/`ClientConfig` is built. Mirrors the install
    /// at the top of `main`.
    fn install_provider() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    fn test_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vreckan-serve-test-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("main")
                .replace(' ', "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A throwaway self-signed cert/key pair for a real TLS handshake.
    fn test_cert() -> (Vec<u8>, Vec<u8>) {
        install_provider();
        let dir = test_dir();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        crate::tls::generate_self_signed(&cert, &key, &dir.join("expiry")).unwrap();
        (std::fs::read(&cert).unwrap(), std::fs::read(&key).unwrap())
    }

    #[test]
    fn first_byte_classification() {
        assert!(is_tls_first_byte(TLS_HANDSHAKE_BYTE));
        assert!(is_tls_first_byte(0x16));
        for &method_first in b"GPDHOC" {
            assert!(!is_tls_first_byte(method_first), "{method_first:?}");
        }
        assert!(!is_tls_first_byte(0x00));
        assert!(!is_tls_first_byte(0xff));
    }

    #[test]
    fn redirect_location_preserves_host_and_uri() {
        assert_eq!(
            redirect_location("example.com", "/"),
            "https://example.com/"
        );
        assert_eq!(
            redirect_location("example.com:8443", "/apps/my-app?q=1"),
            "https://example.com:8443/apps/my-app?q=1"
        );
    }

    #[test]
    fn redirect_response_shape() {
        let response = String::from_utf8(redirect_response("example.com:8443", "/foo")).unwrap();
        let (head, _body) = response.split_once("\r\n\r\n").unwrap();
        let mut lines = head.lines();
        assert_eq!(lines.next().unwrap(), "HTTP/1.1 301 Moved Permanently");
        assert!(response.contains("Location: https://example.com:8443/foo"));
        assert!(response.contains("Strict-Transport-Security: max-age=63072000; includeSubDomains"));
        assert!(response.contains("Connection: close"));
        assert!(response.contains("Content-Length: 0"));
    }

    #[test]
    fn parse_http_head_full() {
        let head =
            "GET /api/health?x=1 HTTP/1.1\r\nHost: vreckan.example.com:8443\r\nAccept: */*\r\n\r\n";
        let (method, target, host) = parse_http_head(head);
        assert_eq!(method, "GET");
        assert_eq!(target, "/api/health?x=1");
        assert_eq!(host.as_deref(), Some("vreckan.example.com:8443"));
    }

    #[test]
    fn parse_http_head_lowercase_host_and_missing_host() {
        let head = "POST /submit HTTP/1.1\r\nhost: lan.local\r\n\r\n";
        let (method, target, host) = parse_http_head(head);
        assert_eq!(method, "POST");
        assert_eq!(target, "/submit");
        assert_eq!(host.as_deref(), Some("lan.local"));

        let head = "GET /nope HTTP/1.1\r\nAccept: */*\r\n\r\n";
        let (_, target, host) = parse_http_head(head);
        assert_eq!(target, "/nope");
        assert_eq!(host, None);

        let (method, target, _) = parse_http_head("garbage-without-spaces");
        assert_eq!(method, "garbage-without-spaces");
        assert_eq!(target, "/");
    }

    #[tokio::test]
    async fn plain_http_get_receives_301_to_https() {
        let (cert, key) = test_cert();
        let tls_config = RustlsConfig::from_pem(cert, key).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(serve_loop(
            listener,
            Router::new()
                .route("/hello", get(|| async { "hello from vreckan" }))
                .into_make_service_with_connect_info::<SocketAddr>(),
            tls_config,
            true,
        ));

        let mut socket = TcpStream::connect(addr).await.unwrap();
        socket
            .write_all(b"GET /hello HTTP/1.1\r\nHost: vreckan.example.com:8443\r\n\r\n")
            .await
            .unwrap();

        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match socket.read(&mut chunk).await.unwrap() {
                0 => break,
                n => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let response = String::from_utf8_lossy(&buf);
        assert!(response.starts_with("HTTP/1.1 301"), "{response}");
        assert!(
            response.contains("Location: https://vreckan.example.com:8443/hello"),
            "{response}"
        );
        assert!(response.contains("Strict-Transport-Security:"));
    }

    #[tokio::test]
    async fn tls_clienthello_serves_the_app() {
        let (cert, key) = test_cert();
        let tls_config = RustlsConfig::from_pem(cert, key).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(serve_loop(
            listener,
            Router::new()
                .route("/hello", get(|| async { "hello from vreckan" }))
                .into_make_service_with_connect_info::<SocketAddr>(),
            tls_config,
            true,
        ));

        // A real TLS ClientHello (first byte 0x16) must not be treated as
        // plain HTTP: it gets the app, not a 301.
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client_config = no_verify_client_config();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut tls_stream = TlsConnector::from(Arc::new(client_config))
            .connect(ServerName::try_from("localhost").unwrap(), stream)
            .await
            .unwrap();

        // `Connection: close` so the server releases the stream after the
        // response and the read-below-EOF loop terminates.
        tls_stream
            .write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match tls_stream.read(&mut chunk).await.unwrap() {
                0 => break,
                n => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let response = String::from_utf8_lossy(&buf);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            !response.contains("301"),
            "TLS traffic must not be redirected: {response}"
        );
        assert!(response.contains("hello from vreckan"), "{response}");
    }

    /// A TLS client that accepts any certificate (self-signed test cert).
    fn no_verify_client_config() -> ClientConfig {
        #[derive(Debug)]
        struct NoVerify;

        impl ServerCertVerifier for NoVerify {
            fn verify_server_cert(
                &self,
                _end_entity: &CertificateDer,
                _intermediates: &[CertificateDer],
                _server_name: &ServerName,
                _ocsp_response: &[u8],
                _now: UnixTime,
            ) -> Result<ServerCertVerified, rustls::Error> {
                Ok(ServerCertVerified::assertion())
            }

            fn verify_tls12_signature(
                &self,
                _message: &[u8],
                _cert: &CertificateDer<'_>,
                _dss: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, Error> {
                Ok(HandshakeSignatureValid::assertion())
            }

            fn verify_tls13_signature(
                &self,
                _message: &[u8],
                _cert: &CertificateDer<'_>,
                _dss: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, Error> {
                Ok(HandshakeSignatureValid::assertion())
            }

            fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                vec![
                    SignatureScheme::RSA_PKCS1_SHA256,
                    SignatureScheme::RSA_PKCS1_SHA384,
                    SignatureScheme::RSA_PKCS1_SHA512,
                    SignatureScheme::RSA_PSS_SHA256,
                    SignatureScheme::RSA_PSS_SHA384,
                    SignatureScheme::RSA_PSS_SHA512,
                    SignatureScheme::ECDSA_NISTP256_SHA256,
                    SignatureScheme::ECDSA_NISTP384_SHA384,
                    SignatureScheme::ECDSA_NISTP521_SHA512,
                ]
            }
        }

        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth()
    }

    #[tokio::test]
    async fn redirect_disabled_plain_http_fails_handshake() {
        let (cert, key) = test_cert();
        let tls_config = RustlsConfig::from_pem(cert, key).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(serve_loop(
            listener,
            Router::new()
                .route("/hello", get(|| async { "hello from vreckan" }))
                .into_make_service_with_connect_info::<SocketAddr>(),
            tls_config,
            false,
        ));

        // With the flag off, a plain-HTTP first byte goes to the TLS
        // handshake and fails (the pre-ANT-652 behavior): the client gets
        // a handshake alert, not a 301.
        let mut socket = TcpStream::connect(addr).await.unwrap();
        socket
            .write_all(b"GET /hello HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .unwrap();
        // The server answers with a TLS alert record (0x15), or the
        // handshake times out and closes the connection; either way the
        // first byte of the reply is not an ASCII status line.
        let mut first = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(15), socket.read(&mut first))
            .await
            .expect("server answered within the handshake timeout")
            .unwrap();
        assert!(n > 0);
        assert_ne!(first[0], b'H', "expected a TLS record, got a status line");
        assert_ne!(first[0], b'3');
    }
}

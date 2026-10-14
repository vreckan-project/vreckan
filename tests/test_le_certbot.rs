//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tests for the Let's Encrypt / certbot orchestration (`src/le_certbot.rs`)
//! and the admin Certificates endpoints.
//!
//! Ports `tests/test_le_certbot`. The Docker client is mocked (a fake
//! implementing the `CertbotDocker` trait) so the tests never need a Docker
//! daemon or the certbot image. Certificate validity is exercised with real
//! PEM certs generated via `rcgen` into a temp tree that stands in for the
//! shared `vreckan-le` volume.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::errors::Error;
use bollard::models::{
    ContainerCreateBody, ContainerCreateResponse, ContainerInspectResponse,
    ContainerWaitResponse, MountPoint,
};
use bollard::query_parameters::CreateContainerOptions;
use futures_util::{stream, Stream};
use http::StatusCode;
use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType, KeyPair};
use serde_json::{json, Value};
use time::OffsetDateTime;

use common::{build_state, SecureClient, TempDir};
use vreckan::le_certbot;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a self-signed cert + key; return (cert_pem, key_pem).
///
/// `days` is the lifetime from now; a negative value yields an already
/// expired cert (not_after in the past, not_before even earlier). Mirrors
/// `_make_cert`.
fn make_cert(cn: &str, days: i64) -> (String, String) {
    let key_pair = KeyPair::generate().expect("generate key");
    let now = OffsetDateTime::now_utc();
    let not_after = now + time::Duration::days(days);
    let not_before = not_after - time::Duration::days(90);

    let mut params = CertificateParams::default();
    params.not_before = not_before;
    params.not_after = not_after;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    params.distinguished_name = dn;

    let cert: Certificate = params.self_signed(&key_pair).expect("self-sign");
    (cert.pem(), key_pair.serialize_pem())
}

/// Write a cert+key into `base/live/<domain>/` (certbot's layout). Mirrors
/// `_write_cert_tree`.
fn write_cert_tree(base: &std::path::Path, domain: &str, days: i64) {
    let live = base.join("live").join(domain);
    std::fs::create_dir_all(&live).unwrap();
    let (cert_pem, key_pem) = make_cert(domain, days);
    std::fs::write(live.join("cert.pem"), cert_pem).unwrap();
    std::fs::write(live.join("privkey.pem"), key_pem).unwrap();
}

/// A fake certbot container: reports a fixed exit code + logs. Mirrors
/// `FakeContainer`.
struct FakeContainer {
    exit_code: i64,
    logs: String,
}

/// A fake `containers` collection: records `run` calls and models the app
/// container (resolved by hostname) for `/data/le` mount auto-detection.
/// Mirrors `FakeContainers`. `run_calls` uses interior mutability because the
/// trait method takes `&self`; the test reads it through the shared `Arc`.
struct FakeContainers {
    container: FakeContainer,
    run_calls: Arc<Mutex<Vec<(String, Vec<String>, Value)>>>,
    mounts: Option<Vec<MountPoint>>,
}

/// A fake Docker client implementing the `CertbotDocker` trait. Mirrors
/// `FakeClient`. The `removed` flag uses interior mutability (the trait
/// method takes `&self`) and is shared with the test via `removed_flag`.
struct FakeClient {
    containers: FakeContainers,
    removed_flag: Arc<Mutex<bool>>,
}

impl FakeClient {
    fn new(exit_code: i64, logs: &str, mounts: Option<Vec<MountPoint>>) -> Self {
        Self {
            containers: FakeContainers {
                container: FakeContainer {
                    exit_code,
                    logs: logs.to_string(),
                },
                run_calls: Arc::new(Mutex::new(Vec::new())),
                mounts,
            },
            removed_flag: Arc::new(Mutex::new(false)),
        }
    }
    /// Whether the container was removed (read after the call).
    fn removed(&self) -> bool {
        *self.removed_flag.lock().unwrap()
    }
    /// The recorded run calls (read after the call).
    fn run_calls(&self) -> Vec<(String, Vec<String>, Value)> {
        self.containers.run_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl le_certbot::CertbotDocker for FakeClient {
    async fn create_container(
        &self,
        _options: Option<CreateContainerOptions>,
        config: ContainerCreateBody,
    ) -> Result<ContainerCreateResponse, Error> {
        let image = config.image.clone().unwrap_or_default();
        let args = config.cmd.clone().unwrap_or_default();
        // Record the run call (image, args, kwargs). The kwargs carry the
        // volume mount spec (the `binds` host_config entry).
        let binds = config
            .host_config
            .as_ref()
            .and_then(|h| h.binds.clone())
            .unwrap_or_default();
        let mut volumes = serde_json::Map::new();
        for bind in &binds {
            // bind is "<source>:<target>:<mode>"
            let parts: Vec<&str> = bind.split(':').collect();
            if parts.len() >= 2 {
                volumes.insert(
                    parts[0].to_string(),
                    json!({ "bind": parts[1], "mode": parts.get(2).copied().unwrap_or("rw") }),
                );
            }
        }
        let kwargs = json!({ "volumes": Value::Object(volumes) });
        self.containers
            .run_calls
            .lock()
            .unwrap()
            .push((image, args, kwargs));
        Ok(ContainerCreateResponse {
            id: "fake-container-id".to_string(),
            warnings: Vec::new(),
        })
    }

    async fn start_container(&self, _name: &str) -> Result<(), Error> {
        Ok(())
    }

    fn wait_container(
        &self,
        _name: &str,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ContainerWaitResponse, Error>> + Send>> {
        let code = self.containers.container.exit_code;
        Box::pin(stream::iter(vec![Ok(ContainerWaitResponse {
            status_code: code,
            error: None,
        })]))
    }

    fn logs(
        &self,
        _name: &str,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<LogOutput, Error>> + Send>> {
        let logs = self.containers.container.logs.clone();
        Box::pin(stream::iter(vec![Ok(LogOutput::StdOut {
            message: axum::body::Bytes::from(logs),
        })]))
    }

    async fn remove_container(&self, _name: &str) -> Result<(), Error> {
        *self.removed_flag.lock().unwrap() = true;
        Ok(())
    }

    async fn inspect_container(
        &self,
        _name: &str,
    ) -> Result<ContainerInspectResponse, Error> {
        match &self.containers.mounts {
            Some(mounts) => Ok(ContainerInspectResponse {
                mounts: Some(mounts.clone()),
                ..Default::default()
            }),
            None => Err(Error::DockerResponseServerError {
                status_code: 404,
                message: "no such container".to_string(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Test fixtures / helpers
// ---------------------------------------------------------------------------

/// A process-wide lock serialising the tests that mutate the module's global
/// cert-dir statics (mirrors pytest's function-scoped `monkeypatch`, which
/// runs tests sequentially).
static LE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Point the module's cert dirs at a fresh temp tree (host + volume). Mirrors
/// the `le_paths` fixture. Holds `LE_TEST_LOCK` for its lifetime so the
/// global statics aren't clobbered by a concurrent test.
struct LePaths {
    host: String,
    volume: String,
    _guard: std::sync::MutexGuard<'static, ()>,
    _tmp: TempDir,
}

/// Acquire the LE test lock, recovering from a poisoned lock (a prior test
/// that panicked while holding it) so one failure doesn't cascade to every
/// other test that uses the global cert-dir statics.
fn lock_le() -> std::sync::MutexGuard<'static, ()> {
    LE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn le_paths() -> LePaths {
    let guard = lock_le();
    let tmp = TempDir::new();
    let host = tmp.path().join("host-le");
    let volume = tmp.path().join("volume-le");
    std::fs::create_dir_all(&host).unwrap();
    std::fs::create_dir_all(&volume).unwrap();
    *le_certbot::LE_HOST_CERT_DIR.write().unwrap() =
        Some(host.to_string_lossy().to_string());
    *le_certbot::LE_APP_CERT_DIR.write().unwrap() =
        Some(volume.to_string_lossy().to_string());
    LePaths {
        host: host.to_string_lossy().to_string(),
        volume: volume.to_string_lossy().to_string(),
        _guard: guard,
        _tmp: tmp,
    }
}

/// A default LE config (mirrors the Python `_cfg()` helper). Tests override
/// individual fields as needed.
fn default_cfg() -> le_certbot::LeConfig {
    le_certbot::LeConfig {
        le_enabled: true,
        le_domains: "example.com,www.example.com".to_string(),
        le_auth: "dns-cloudflare".to_string(),
        le_staging: false,
        le_cloudflare_email: "admin@example.com".to_string(),
        le_cloudflare_token: "cf-token-123".to_string(),
        le_auto_renew: true,
        le_renew_check_hours: 6,
        le_renew_before_days: 30,
    }
}

/// Build a `MountPoint` for the fake app-container mount list.
fn mount(typ: &str, source: Option<&str>, name: Option<&str>, destination: &str) -> MountPoint {
    MountPoint {
        typ: Some(typ.to_string()),
        source: source.map(|s| s.to_string()),
        name: name.map(|s| s.to_string()),
        destination: Some(destination.to_string()),
        ..Default::default()
    }
}

/// A `SecureClient` logged in as the bootstrap admin with the E2EE handshake
/// performed.
async fn admin_client(state: vreckan::state::AppState) -> SecureClient {
    let mut sc = SecureClient::new(state);
    let resp = sc.login("admin", "admin1234").await;
    assert_eq!(resp.status, StatusCode::OK, "login failed: {:?}", resp.body);
    sc.handshake().await;
    sc
}

// --- certbot_args ------------------------------------------------------------
#[tokio::test]
async fn test_certbot_args_dns_cloudflare() {
    let args = le_certbot::certbot_args("dns-cloudflare", &["a.com".into(), "b.com".into()], false, false);
    assert_eq!(args[0], "certonly");
    assert!(args.contains(&"--non-interactive".to_string()));
    assert!(args.contains(&"--agree-tos".to_string()));
    assert!(args.contains(&"--register-unsafely-without-email".to_string()));
    assert!(!args.contains(&"--staging".to_string()));
    assert!(args.contains(&"--dns-cloudflare".to_string()));
    assert!(args.contains(&format!("--dns-cloudflare-credentials={}", le_certbot::CERTBOT_CREDENTIALS)));
    assert!(args.contains(&"-d".to_string()));
    assert!(args.contains(&"a.com".to_string()));
    assert!(args.contains(&"b.com".to_string()));
}

#[tokio::test]
async fn test_certbot_args_dns_cloudflare_staging() {
    let args = le_certbot::certbot_args("dns-cloudflare", &["a.com".into()], true, false);
    assert!(args.contains(&"--staging".to_string()));
}

#[tokio::test]
async fn test_certbot_args_http01() {
    let args = le_certbot::certbot_args("http-01", &["a.com".into()], false, false);
    assert!(args.contains(&"--http-01".to_string()));
    assert!(args.contains(&format!("--webroot-root={}", le_certbot::CERTBOT_CERT_DIR)));
    assert!(!args.contains(&"--dns-cloudflare".to_string()));
}

#[tokio::test]
async fn test_certbot_args_renew() {
    let args = le_certbot::certbot_args("dns-cloudflare", &["a.com".into()], false, true);
    assert_eq!(args[0], "renew");
}

// --- needs_renewal -----------------------------------------------------------
#[tokio::test]
async fn test_needs_renewal() {
    assert!(le_certbot::needs_renewal(Some(10), 30));
    assert!(le_certbot::needs_renewal(Some(30), 30));
    assert!(!le_certbot::needs_renewal(Some(31), 30));
    assert!(le_certbot::needs_renewal(Some(-5), 30)); // already expired
    assert!(!le_certbot::needs_renewal(None, 30)); // no cert
}

// --- cert_status / cert_expiry -----------------------------------------------
#[tokio::test]
async fn test_cert_status_absent() {
    let paths = le_paths();
    let status = le_certbot::cert_status(&["example.com".into()]);
    assert_eq!(status["present"], false);
    assert_eq!(status["valid"], false);
    assert!(status["days_remaining"].is_null());
    let _ = paths;
}

#[tokio::test]
async fn test_cert_status_present_valid() {
    let paths = le_paths();
    write_cert_tree(std::path::Path::new(&paths.volume), "example.com", 90);
    let status = le_certbot::cert_status(&["example.com".into()]);
    assert_eq!(status["present"], true);
    assert_eq!(status["source"], "volume");
    assert_eq!(status["valid"], true);
    let days = status["days_remaining"].as_i64().expect("days_remaining");
    assert!(80 <= days && days <= 90, "days={days}");
    assert!(status["expiry"].is_string());
    let _ = paths;
}

#[tokio::test]
async fn test_cert_status_expired() {
    let paths = le_paths();
    write_cert_tree(std::path::Path::new(&paths.volume), "example.com", -5);
    let status = le_certbot::cert_status(&["example.com".into()]);
    assert_eq!(status["present"], true);
    assert_eq!(status["valid"], false);
    assert!(status["days_remaining"].as_i64().expect("days") < 0);
    let _ = paths;
}

#[tokio::test]
async fn test_cert_status_host_preferred() {
    let paths = le_paths();
    // A cert in both the host and volume paths: the host one wins.
    write_cert_tree(std::path::Path::new(&paths.host), "example.com", 90);
    write_cert_tree(std::path::Path::new(&paths.volume), "example.com", 90);
    let status = le_certbot::cert_status(&["example.com".into()]);
    assert_eq!(status["source"], "host");
    assert!(status["cert"].as_str().unwrap().starts_with(&paths.host));
    let _ = paths;
}

#[tokio::test]
async fn test_cert_expiry_parses() {
    let paths = le_paths();
    write_cert_tree(std::path::Path::new(&paths.volume), "example.com", 90);
    let path = std::path::Path::new(&paths.volume)
        .join("live")
        .join("example.com")
        .join("cert.pem");
    let exp = le_certbot::cert_expiry(path.to_string_lossy().as_ref());
    let exp = exp.expect("expiry");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let days = (exp - now) / 86400;
    assert!(80 <= days && days <= 90, "days={days}");
    let _ = paths;
}

#[tokio::test]
async fn test_cert_expiry_missing_file() {
    let paths = le_paths();
    let path = std::path::Path::new(&paths.volume).join("nope.pem");
    assert!(le_certbot::cert_expiry(path.to_string_lossy().as_ref()).is_none());
    let _ = paths;
}

// --- run_certbot (mocked Docker) ---------------------------------------------
#[tokio::test]
async fn test_run_certbot_success() {
    let _paths = le_paths();
    let client = FakeClient::new(0, "Certificate issued.", None);
    let args = le_certbot::certbot_args("dns-cloudflare", &["a.com".into()], false, false);
    let result = le_certbot::run_certbot(&client, &args, le_certbot::LE_VOLUME, 300).await;
    assert_eq!(result["ok"], true);
    assert_eq!(result["exit_code"], 0);
    assert!(result["logs"].as_str().unwrap().contains("Certificate issued."));
    // The container was removed afterwards.
    assert!(client.removed());
    // The volume was mounted at certbot's default path.
    let calls = client.run_calls();
    let kwargs = &calls[0].2;
    assert_eq!(
        kwargs["volumes"],
        json!({ le_certbot::LE_VOLUME: { "bind": le_certbot::CERTBOT_CERT_DIR, "mode": "rw" } })
    );
}

#[tokio::test]
async fn test_run_certbot_mounts_app_data_le_bind() {
    let _paths = le_paths();
    // `app_le_mount` resolves the app container by `HOSTNAME`; set it so the
    // fake app-container mount list is consulted (mirrors the Python test,
    // where `socket.gethostname()` always returns a value).
    std::env::set_var("HOSTNAME", "vreckan-test-app");
    // When the app's /data is a host bind mount, certbot must mount the
    // matching host sub-path (/data/le) so it shares the app's storage.
    let client = FakeClient::new(
        0,
        "ok",
        Some(vec![mount("bind", Some("/srv/vreckan-data"), None, "/data")]),
    );
    let args = le_certbot::certbot_args("dns-cloudflare", &["a.com".into()], false, false);
    let _ = le_certbot::run_certbot(&client, &args, le_certbot::LE_VOLUME, 300).await;
    let calls = client.run_calls();
    let kwargs = &calls[0].2;
    assert_eq!(
        kwargs["volumes"],
        json!({ "/srv/vreckan-data/le": { "bind": le_certbot::CERTBOT_CERT_DIR, "mode": "rw" } })
    );
}

#[tokio::test]
async fn test_run_certbot_mounts_app_data_le_volume() {
    let _paths = le_paths();
    // `app_le_mount` resolves the app container by `HOSTNAME`; set it so the
    // fake app-container mount list is consulted.
    std::env::set_var("HOSTNAME", "vreckan-test-app");
    // When the app's /data is a named volume, certbot mounts that same volume.
    let client = FakeClient::new(
        0,
        "ok",
        Some(vec![mount("volume", None, Some("vreckan-data"), "/data")]),
    );
    let args = le_certbot::certbot_args("dns-cloudflare", &["a.com".into()], false, false);
    let _ = le_certbot::run_certbot(&client, &args, le_certbot::LE_VOLUME, 300).await;
    let calls = client.run_calls();
    let kwargs = &calls[0].2;
    assert_eq!(
        kwargs["volumes"],
        json!({ "vreckan-data": { "bind": le_certbot::CERTBOT_CERT_DIR, "mode": "rw" } })
    );
}

#[tokio::test]
async fn test_run_certbot_failure() {
    let _paths = le_paths();
    let client = FakeClient::new(1, "DNS challenge failed", None);
    let args = le_certbot::certbot_args("dns-cloudflare", &["a.com".into()], false, false);
    let result = le_certbot::run_certbot(&client, &args, le_certbot::LE_VOLUME, 300).await;
    assert_eq!(result["ok"], false);
    assert_eq!(result["exit_code"], 1);
    assert!(result["logs"].as_str().unwrap().contains("DNS challenge failed"));
    assert!(client.removed());
}

#[tokio::test]
async fn test_run_certbot_writes_credentials() {
    let paths = le_paths();
    let path = le_certbot::write_cloudflare_credentials("admin@example.com", "cf-token-123");
    assert_eq!(path, format!("{}/cloudflare.ini", paths.volume));
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("dns_cloudflare_api_token = cf-token-123"));
    assert!(!content.contains("dns_cloudflare_email"));
    assert!(!content.contains("[dns_cloudflare]"));
    let _ = paths;
}

#[tokio::test]
async fn test_run_certbot_writes_credentials_email_only() {
    let paths = le_paths();
    // Without a token, the email line is written (legacy email + API-key method).
    let path = le_certbot::write_cloudflare_credentials("admin@example.com", "");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("dns_cloudflare_email = admin@example.com"));
    assert!(!content.contains("dns_cloudflare_api_token"));
    let _ = paths;
}

// --- ensure_le_cert / renew_le_cert (mocked Docker) --------------------------
#[tokio::test]
async fn test_ensure_le_cert_disabled() {
    let _paths = le_paths();
    let mut cfg = default_cfg();
    cfg.le_enabled = false;
    let client = Arc::new(FakeClient::new(0, "ok", None));
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = Some(client.clone());
    let result = le_certbot::ensure_le_cert(&cfg).await;
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = None;
    assert_eq!(result["enabled"], false);
    assert_eq!(result["issued"], false);
}

#[tokio::test]
async fn test_ensure_le_cert_no_domains() {
    let _paths = le_paths();
    let mut cfg = default_cfg();
    cfg.le_domains = "".to_string();
    let client = FakeClient::new(0, "ok", None);
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = Some(Arc::new(client));
    let result = le_certbot::ensure_le_cert(&cfg).await;
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = None;
    assert_eq!(result["enabled"], true);
    assert_eq!(result["issued"], false);
}

#[tokio::test]
async fn test_ensure_le_cert_issues_when_missing() {
    let _paths = le_paths();
    let cfg = default_cfg();
    let client = Arc::new(FakeClient::new(0, "ok", None));
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = Some(client.clone());
    let result = le_certbot::ensure_le_cert(&cfg).await;
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = None;
    assert_eq!(result["enabled"], true);
    assert_eq!(result["issued"], true);
    // certonly was used (not renew) for a fresh issue.
    let calls = client.run_calls();
    assert_eq!(calls[0].1[0], "certonly");
}

#[tokio::test]
async fn test_ensure_le_cert_skips_when_valid() {
    let paths = le_paths();
    write_cert_tree(std::path::Path::new(&paths.volume), "example.com", 90);
    let cfg = default_cfg();
    let client = Arc::new(FakeClient::new(0, "ok", None));
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = Some(client.clone());
    let result = le_certbot::ensure_le_cert(&cfg).await;
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = None;
    assert_eq!(result["issued"], false);
    // No certbot run was needed (the cert is still valid).
    assert!(client.run_calls().is_empty());
    let _ = paths;
}

#[tokio::test]
async fn test_renew_le_cert_not_due() {
    let paths = le_paths();
    write_cert_tree(std::path::Path::new(&paths.volume), "example.com", 90);
    let cfg = default_cfg();
    let client = Arc::new(FakeClient::new(0, "ok", None));
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = Some(client.clone());
    let result = le_certbot::renew_le_cert(&cfg).await;
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = None;
    assert_eq!(result["due"], false);
    assert_eq!(result["renewed"], false);
    assert!(client.run_calls().is_empty());
    let _ = paths;
}

#[tokio::test]
async fn test_renew_le_cert_due_and_renews() {
    let paths = le_paths();
    write_cert_tree(std::path::Path::new(&paths.volume), "example.com", 10);
    let cfg = default_cfg();
    let client = Arc::new(FakeClient::new(0, "ok", None));
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = Some(client.clone());
    let result = le_certbot::renew_le_cert(&cfg).await;
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = None;
    assert_eq!(result["due"], true);
    assert_eq!(result["renewed"], true);
    // renew was used (not certonly).
    let calls = client.run_calls();
    assert_eq!(calls[0].1[0], "renew");
    let _ = paths;
}

#[tokio::test]
async fn test_renew_le_cert_disabled() {
    let _paths = le_paths();
    let mut cfg = default_cfg();
    cfg.le_enabled = false;
    let client = Arc::new(FakeClient::new(0, "ok", None));
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = Some(client.clone());
    let result = le_certbot::renew_le_cert(&cfg).await;
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = None;
    assert_eq!(result["due"], false);
    assert!(client.run_calls().is_empty());
}

// --- effective_le_config -----------------------------------------------------
#[tokio::test]
async fn test_effective_le_config_reads_env_defaults() {
    // With no DB rows and no env, the defaults apply.
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let cfg = le_certbot::effective_le_config(&state).await;
    assert!(!cfg.le_enabled); // default
    assert_eq!(cfg.le_auth, "dns-cloudflare");
    assert_eq!(cfg.le_renew_before_days, 30);
}

#[tokio::test]
async fn test_effective_le_config_env_override() {
    let tmp = TempDir::new();
    let mut state = build_state(tmp.path()).await;
    // `get_setting` consults the settings instance attribute (populated from
    // the environment at import), so set it to simulate an env override.
    state.settings.le_enabled = true;
    state.settings.le_domains = "a.com,b.com".to_string();
    let cfg = le_certbot::effective_le_config(&state).await;
    assert!(cfg.le_enabled);
    assert_eq!(cfg.le_domains, "a.com,b.com");
}

// --- admin Certificates endpoints --------------------------------------------
#[tokio::test]
async fn test_certificates_get_default() {
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let client = admin_client(state).await;
    let (status, data) = client.call("GET", "/api/admin/certificates", None).await;
    assert_eq!(status, StatusCode::OK);
    let data = data.expect("body");
    assert_eq!(data["values"]["le_enabled"], false);
    assert_eq!(data["values"]["le_auth"], "dns-cloudflare");
    assert!(data["values"].get("le_domains").is_some());
    assert!(data.get("status").is_some());
    assert_eq!(data["status"]["present"], false);
    // The token is masked, not echoed.
    assert_eq!(data["values"]["le_cloudflare_token"], "");
}

#[tokio::test]
async fn test_certificates_put_and_get() {
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let client = admin_client(state).await;
    let payload = json!({
        "le_enabled": true,
        "le_domains": ["example.com", "www.example.com"],
        "le_auth": "dns-cloudflare",
        "le_cloudflare_email": "admin@example.com",
        "le_cloudflare_token": "cf-token-123",
        "le_staging": true,
        "le_auto_renew": true,
        "le_renew_check_hours": 12,
        "le_renew_before_days": 20,
    });
    let (status, _) = client.call("PUT", "/api/admin/certificates", Some(&payload)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, data) = client.call("GET", "/api/admin/certificates", None).await;
    assert_eq!(status, StatusCode::OK);
    let data = data.expect("body");
    let v = &data["values"];
    assert_eq!(v["le_enabled"], true);
    assert_eq!(v["le_domains"], json!(["example.com", "www.example.com"]));
    assert_eq!(v["le_staging"], true);
    assert_eq!(v["le_renew_check_hours"], 12);
    assert_eq!(v["le_renew_before_days"], 20);
    // The token is masked but reported as set.
    assert_eq!(v["le_cloudflare_token"], "");
    assert_eq!(v["token_set"], true);
    assert_eq!(v["token_length"], "cf-token-123".len() as i64);
    // The value now comes from the DB (edited in the UI).
    assert_eq!(data["sources"]["le_enabled"], "db");
}

#[tokio::test]
async fn test_certificates_put_blank_token_keeps_existing() {
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let client = admin_client(state).await;
    // First set a token.
    let first = json!({
        "le_enabled": true,
        "le_domains": ["example.com"],
        "le_auth": "dns-cloudflare",
        "le_cloudflare_email": "admin@example.com",
        "le_cloudflare_token": "original-token",
        "le_staging": false,
        "le_auto_renew": true,
        "le_renew_check_hours": 6,
        "le_renew_before_days": 30,
    });
    client.call("PUT", "/api/admin/certificates", Some(&first)).await;
    // Then save again with a blank token — the existing one should be kept.
    let mut second = first.clone();
    second["le_cloudflare_token"] = json!("");
    client.call("PUT", "/api/admin/certificates", Some(&second)).await;

    let (status, data) = client.call("GET", "/api/admin/certificates", None).await;
    assert_eq!(status, StatusCode::OK);
    let v = &data.expect("body")["values"];
    assert_eq!(v["token_set"], true);
    assert_eq!(v["token_length"], "original-token".len() as i64);
}

#[tokio::test]
async fn test_certificates_renew_disabled() {
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let client = admin_client(state).await;
    let (status, _) = client.call("POST", "/api/admin/certificates/renew", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST); // LE not enabled by default
}

#[tokio::test]
async fn test_certificates_renew_no_domains() {
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let client = admin_client(state).await;
    // Enable LE but leave domains empty.
    let payload = json!({
        "le_enabled": true,
        "le_domains": [],
        "le_auth": "dns-cloudflare",
        "le_cloudflare_email": "admin@example.com",
        "le_cloudflare_token": "t",
        "le_staging": false,
        "le_auto_renew": true,
        "le_renew_check_hours": 6,
        "le_renew_before_days": 30,
    });
    client.call("PUT", "/api/admin/certificates", Some(&payload)).await;
    let (status, _) = client.call("POST", "/api/admin/certificates/renew", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_certificates_renew_issues() {
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let client = admin_client(state).await;
    // Enable LE with a domain, then renew (no valid cert yet -> issues one).
    let payload = json!({
        "le_enabled": true,
        "le_domains": ["example.com"],
        "le_auth": "dns-cloudflare",
        "le_cloudflare_email": "admin@example.com",
        "le_cloudflare_token": "t",
        "le_staging": false,
        "le_auto_renew": true,
        "le_renew_check_hours": 6,
        "le_renew_before_days": 30,
    });
    client.call("PUT", "/api/admin/certificates", Some(&payload)).await;
    // Point the module's cert dirs at the temp tree and mock the Docker client.
    let paths = le_paths();
    let fake = Arc::new(FakeClient::new(0, "ok", None));
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = Some(fake.clone());
    let (status, data) = client.call("POST", "/api/admin/certificates/renew", None).await;
    *le_certbot::FAKE_CERTBOT_CLIENT.lock().unwrap() = None;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.expect("body")["issued"], true);
    let calls = fake.run_calls();
    assert_eq!(calls[0].1[0], "certonly");
    let _ = paths;
}

// ---------------------------------------------------------------------------
// P2-2: Cloudflare credentials file permissions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cloudflare_ini_written_0600() {
    let paths = le_paths();
    let written =
        le_certbot::write_cloudflare_credentials("admin@example.com", "cf-token-123");
    assert_eq!(written, format!("{}/cloudflare.ini", paths.volume));
    let mode = std::fs::metadata(&written)
        .expect("cloudflare.ini exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    // The token really is in the file (plaintext), hence the tight mode.
    assert!(std::fs::read_to_string(&written).unwrap().contains("cf-token-123"));
    let _ = paths;
}

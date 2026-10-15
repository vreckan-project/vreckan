//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tests for the ACME orchestration (`src/acme_orchestrator.rs`) and the admin Certificates
//! endpoints.
//!
//! Ported from the retired Python certbot test flow. The ACME issuer is mocked (a fake
//! implementing the `acme_orchestrator::AcmeIssuer` seam) so the tests never touch the
//! network or the acmex client. Certificate validity is exercised with real
//! PEM certs generated via `rcgen` into a temp tree that stands in for the
//! app's `/data/acme` storage.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use http::StatusCode;
use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType, KeyPair};
use serde_json::json;
use time::OffsetDateTime;

use common::{build_state, SecureClient, TempDir};
use vreckan::acme_orchestrator;

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

/// Write a cert+key into `base/live/<domain>/` (the cert layout). Mirrors
/// `_write_cert_tree`.
fn write_cert_tree(base: &std::path::Path, domain: &str, days: i64) {
    let live = base.join("live").join(domain);
    std::fs::create_dir_all(&live).unwrap();
    let (cert_pem, key_pem) = make_cert(domain, days);
    std::fs::write(live.join("cert.pem"), cert_pem).unwrap();
    std::fs::write(live.join("privkey.pem"), key_pem).unwrap();
}

/// A process-wide lock serialising the tests that mutate the module's global
/// cert-dir statics (mirrors pytest's function-scoped `monkeypatch`, which
/// runs tests sequentially).
static ACME_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Point the module's cert dirs at a fresh temp tree (host + app). Mirrors
/// the `acme_paths` fixture. Holds `ACME_TEST_LOCK` for its lifetime so the
/// global statics aren't clobbered by a concurrent test.
struct AcmePaths {
    host: String,
    app: String,
    _guard: std::sync::MutexGuard<'static, ()>,
    _tmp: TempDir,
}

/// Acquire the LE test lock, recovering from a poisoned lock (a prior test
/// that panicked while holding it) so one failure doesn't cascade to every
/// other test that uses the global cert-dir statics.
fn lock_le() -> std::sync::MutexGuard<'static, ()> {
    ACME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn acme_paths() -> AcmePaths {
    let guard = lock_le();
    let tmp = TempDir::new();
    let host = tmp.path().join("host-acme");
    let app = tmp.path().join("app-acme");
    std::fs::create_dir_all(&host).unwrap();
    std::fs::create_dir_all(&app).unwrap();
    *acme_orchestrator::ACME_HOST_CERT_DIR.write().unwrap() = Some(host.to_string_lossy().to_string());
    *acme_orchestrator::ACME_CERT_DIR.write().unwrap() = Some(app.to_string_lossy().to_string());
    AcmePaths {
        host: host.to_string_lossy().to_string(),
        app: app.to_string_lossy().to_string(),
        _guard: guard,
        _tmp: tmp,
    }
}

/// A default ACME config (mirrors the Python `_cfg()` helper). Tests override
/// individual fields as needed. The Cloudflare zone id is set so the provider
/// registry builds without a network zone-discovery call.
fn default_cfg() -> acme_orchestrator::AcmeConfig {
    acme_orchestrator::AcmeConfig {
        acme_enabled: true,
        acme_ca: "letsencrypt".to_string(),
        acme_domains: "example.com".to_string(),
        acme_auth: "dns-cloudflare".to_string(),
        acme_staging: false,
        acme_cloudflare_email: "admin@example.com".to_string(),
        acme_cloudflare_token: "cf-token-123".to_string(),
        acme_auto_renew: true,
        acme_renew_check_hours: 6,
        acme_renew_before_days: 30,
        acme_dns_provider: "cloudflare".to_string(),
        acme_cloudflare_zone_id: "test-zone-id".to_string(),
        acme_dns_provider_config: String::new(),
    }
}

/// The args a fake issuer recorded from one call.
#[derive(Clone)]
struct IssueCall {
    ca: String,
    auth: String,
    provider: String,
    email: String,
}

/// A scripted ACME issuer for tests: returns a fixed ok/error and records the
/// args it was called with (so tests can assert on the CA / auth / provider /
/// token selection). Replaces the legacy fake Docker client. On success it
/// returns a real self-signed cert/key PEM (generated once up front) so the
/// DB persistence + read-back paths are exercised with parseable certs.
struct FakeAcmeIssuer {
    ok: bool,
    error: Option<String>,
    cert_pem: String,
    key_pem: String,
    calls: Arc<Mutex<Vec<IssueCall>>>,
}

impl FakeAcmeIssuer {
    fn ok_issuer() -> Arc<Self> {
        let (cert_pem, key_pem) = make_cert("example.com", 90);
        Arc::new(Self {
            ok: true,
            error: None,
            cert_pem,
            key_pem,
            calls: Arc::new(Mutex::new(Vec::new())),
        })
    }
    fn failing(e: &str) -> Arc<Self> {
        Arc::new(Self {
            ok: false,
            error: Some(e.to_string()),
            cert_pem: String::new(),
            key_pem: String::new(),
            calls: Arc::new(Mutex::new(Vec::new())),
        })
    }
    fn calls(&self) -> Vec<IssueCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl acme_orchestrator::AcmeIssuer for FakeAcmeIssuer {
    async fn issue(
        &self,
        args: &vreckan::acme::IssueArgs<'_>,
    ) -> vreckan::acme::AcmeIssueResult {
        self.calls.lock().unwrap().push(IssueCall {
            ca: args.ca.to_string(),
            auth: args.auth.to_string(),
            provider: args.provider.to_string(),
            email: args.email.to_string(),
        });
        if self.ok {
            vreckan::acme::AcmeIssueResult {
                ok: true,
                logs: "fake acmex: issued".to_string(),
                error: None,
                cert_pem: Some(self.cert_pem.clone()),
                key_pem: Some(self.key_pem.clone()),
            }
        } else {
            vreckan::acme::AcmeIssueResult {
                ok: false,
                logs: "fake acmex: failed".to_string(),
                error: self.error.clone(),
                cert_pem: None,
                key_pem: None,
            }
        }
    }
}

/// Install a fake issuer; the guard clears it on drop.
struct IssuerGuard;
impl IssuerGuard {
    fn new(issuer: Arc<dyn acme_orchestrator::AcmeIssuer>) -> Self {
        *acme_orchestrator::FAKE_ACME_ISSUER.lock().unwrap() = Some(issuer);
        Self
    }
}
impl Drop for IssuerGuard {
    fn drop(&mut self) {
        *acme_orchestrator::FAKE_ACME_ISSUER.lock().unwrap() = None;
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

// --- needs_renewal -----------------------------------------------------------
#[tokio::test]
async fn test_needs_renewal() {
    assert!(acme_orchestrator::needs_renewal(Some(10), 30));
    assert!(acme_orchestrator::needs_renewal(Some(30), 30));
    assert!(!acme_orchestrator::needs_renewal(Some(31), 30));
    assert!(acme_orchestrator::needs_renewal(Some(-5), 30)); // already expired
    assert!(!acme_orchestrator::needs_renewal(None, 30)); // no cert
}

// --- cert_status / cert_expiry -----------------------------------------------
#[tokio::test]
async fn test_cert_status_absent() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let status = acme_orchestrator::cert_status(&state, &["example.com".into()]).await;
    assert_eq!(status["present"], false);
    assert_eq!(status["valid"], false);
    assert!(status["days_remaining"].is_null());
}

#[tokio::test]
async fn test_cert_status_present_valid() {
    let paths = acme_paths();
    write_cert_tree(std::path::Path::new(&paths.app), "example.com", 90);
    let state = build_state(TempDir::new().path()).await;
    let status = acme_orchestrator::cert_status(&state, &["example.com".into()]).await;
    assert_eq!(status["present"], true);
    assert_eq!(status["source"], "app");
    assert_eq!(status["valid"], true);
    let days = status["days_remaining"].as_i64().expect("days_remaining");
    assert!(80 <= days && days <= 90, "days={days}");
    assert!(status["expiry"].is_string());
}

#[tokio::test]
async fn test_cert_status_expired() {
    let paths = acme_paths();
    write_cert_tree(std::path::Path::new(&paths.app), "example.com", -5);
    let state = build_state(TempDir::new().path()).await;
    let status = acme_orchestrator::cert_status(&state, &["example.com".into()]).await;
    assert_eq!(status["present"], true);
    assert_eq!(status["valid"], false);
    assert!(status["days_remaining"].as_i64().expect("days") < 0);
}

#[tokio::test]
async fn test_cert_status_host_preferred() {
    let paths = acme_paths();
    // A cert in both the host and app paths: the host one wins.
    write_cert_tree(std::path::Path::new(&paths.host), "example.com", 90);
    write_cert_tree(std::path::Path::new(&paths.app), "example.com", 90);
    let state = build_state(TempDir::new().path()).await;
    let status = acme_orchestrator::cert_status(&state, &["example.com".into()]).await;
    assert_eq!(status["source"], "host");
    assert!(status["cert"].as_str().unwrap().starts_with(&paths.host));
}

#[tokio::test]
async fn test_cert_expiry_parses() {
    let paths = acme_paths();
    write_cert_tree(std::path::Path::new(&paths.app), "example.com", 90);
    let path = std::path::Path::new(&paths.app)
        .join("live")
        .join("example.com")
        .join("cert.pem");
    let exp = acme_orchestrator::cert_expiry(path.to_string_lossy().as_ref());
    let exp = exp.expect("expiry");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let days = (exp - now) / 86400;
    assert!(80 <= days && days <= 90, "days={days}");
}

#[tokio::test]
async fn test_cert_expiry_missing_file() {
    let paths = acme_paths();
    let path = std::path::Path::new(&paths.app).join("nope.pem");
    assert!(acme_orchestrator::cert_expiry(path.to_string_lossy().as_ref()).is_none());
}

// --- config DB persistence (source of truth) ---------------------------------
#[tokio::test]
async fn test_acme_cert_record_none_when_unset() {
    let state = build_state(TempDir::new().path()).await;
    assert!(acme_orchestrator::acme_cert_record(&state).await.is_none());
}

#[tokio::test]
async fn test_acme_persist_cert_round_trips() {
    let state = build_state(TempDir::new().path()).await;
    let (cert_pem, key_pem) = make_cert("example.com", 90);
    acme_orchestrator::acme_persist_cert(
        &state,
        &acme_orchestrator::AcmeCertRecord {
            cert_pem: cert_pem.clone(),
            key_pem: key_pem.clone(),
            ca: "letsencrypt".to_string(),
            domains: "example.com,www.example.com".to_string(),
            expires: Some(1234567890),
        },
    )
    .await;
    let rec = acme_orchestrator::acme_cert_record(&state).await.expect("record persisted");
    assert_eq!(rec.cert_pem, cert_pem);
    assert_eq!(rec.key_pem, key_pem);
    assert_eq!(rec.ca, "letsencrypt");
    assert_eq!(rec.domains, "example.com,www.example.com");
    assert_eq!(rec.expires, Some(1234567890));
}

#[tokio::test]
async fn test_cert_status_reads_from_db() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let (cert_pem, key_pem) = make_cert("example.com", 90);
    acme_orchestrator::acme_persist_cert(
        &state,
        &acme_orchestrator::AcmeCertRecord {
            cert_pem: cert_pem.clone(),
            key_pem: key_pem.clone(),
            ca: "letsencrypt".to_string(),
            domains: "example.com".to_string(),
            expires: acme_orchestrator::acme_cert_expiry_from_pem(&cert_pem),
        },
    )
    .await;
    let status = acme_orchestrator::cert_status(&state, &["example.com".into()]).await;
    // The DB is the source of truth: cert/key are returned as PEM content.
    assert_eq!(status["present"], true);
    assert_eq!(status["source"], "acme_db");
    assert_eq!(status["cert"], cert_pem);
    assert_eq!(status["key"], key_pem);
    assert_eq!(status["valid"], true);
    let days = status["days_remaining"].as_i64().expect("days_remaining");
    assert!(80 <= days && days <= 90, "days={days}");
    assert!(status["expiry"].is_string());
}

#[tokio::test]
async fn test_cert_status_db_preferred_over_disk() {
    let paths = acme_paths();
    // A (different) cert on disk, and a DIFFERENT cert persisted in the DB:
    // the DB record must win.
    write_cert_tree(std::path::Path::new(&paths.app), "example.com", 90);
    let state = build_state(TempDir::new().path()).await;
    let (db_cert, db_key) = make_cert("db.example.com", 60);
    acme_orchestrator::acme_persist_cert(
        &state,
        &acme_orchestrator::AcmeCertRecord {
            cert_pem: db_cert.clone(),
            key_pem: db_key.clone(),
            ca: "zerossl".to_string(),
            domains: "db.example.com".to_string(),
            expires: acme_orchestrator::acme_cert_expiry_from_pem(&db_cert),
        },
    )
    .await;
    let status = acme_orchestrator::cert_status(&state, &["example.com".into()]).await;
    assert_eq!(status["source"], "acme_db");
    assert_eq!(status["cert"], db_cert);
    assert_eq!(status["key"], db_key);
    assert_eq!(status["valid"], true);
}

#[tokio::test]
async fn test_ensure_acme_cert_persists_bundle_to_db() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let cfg = default_cfg();
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    let result = acme_orchestrator::ensure_acme_cert(&state, &cfg, false).await;
    assert_eq!(result["issued"], true);
    // The issued bundle is persisted to the config DB with full metadata.
    let rec = acme_orchestrator::acme_cert_record(&state).await.expect("record persisted");
    assert_eq!(rec.cert_pem, fake.cert_pem);
    assert_eq!(rec.key_pem, fake.key_pem);
    assert_eq!(rec.ca, "letsencrypt");
    assert_eq!(rec.domains, "example.com");
    assert!(rec.expires.is_some(), "expiry persisted");
    // And cert_status reads it back from the DB (not disk).
    let status = acme_orchestrator::cert_status(&state, &["example.com".into()]).await;
    assert_eq!(status["source"], "acme_db");
    assert_eq!(status["cert"], fake.cert_pem);
    assert_eq!(status["key"], fake.key_pem);
}

// --- ensure_acme_cert (acmex path, fake issuer) ------------------------------
#[tokio::test]
async fn test_ensure_acme_cert_disabled() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let mut cfg = default_cfg();
    cfg.acme_enabled = false;
    let _guard = IssuerGuard::new(FakeAcmeIssuer::ok_issuer());
    let result = acme_orchestrator::ensure_acme_cert(&state, &cfg, false).await;
    assert_eq!(result["enabled"], false);
    assert_eq!(result["issued"], false);
}

#[tokio::test]
async fn test_ensure_acme_cert_no_domains() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let mut cfg = default_cfg();
    cfg.acme_domains = "".to_string();
    let _guard = IssuerGuard::new(FakeAcmeIssuer::ok_issuer());
    let result = acme_orchestrator::ensure_acme_cert(&state, &cfg, false).await;
    assert_eq!(result["enabled"], true);
    assert_eq!(result["issued"], false);
}

#[tokio::test]
async fn test_ensure_acme_cert_issues_when_missing() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let cfg = default_cfg();
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    let result = acme_orchestrator::ensure_acme_cert(&state, &cfg, false).await;
    assert_eq!(result["enabled"], true);
    assert_eq!(result["issued"], true);
    // The issuer was invoked once, for the Cloudflare DNS-01 provider.
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].provider, "cloudflare");
    assert_eq!(calls[0].auth, "dns-cloudflare");
}

#[tokio::test]
async fn test_ensure_acme_cert_skips_when_valid() {
    let paths = acme_paths();
    write_cert_tree(std::path::Path::new(&paths.app), "example.com", 90);
    let state = build_state(TempDir::new().path()).await;
    let cfg = default_cfg();
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    let result = acme_orchestrator::ensure_acme_cert(&state, &cfg, false).await;
    assert_eq!(result["issued"], false);
    // No issuance was needed (the cert is still valid).
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn test_ensure_acme_cert_force_reissues_valid() {
    let paths = acme_paths();
    write_cert_tree(std::path::Path::new(&paths.app), "example.com", 90);
    let state = build_state(TempDir::new().path()).await;
    let cfg = default_cfg();
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    // `force` (the admin "Issue/Renew Now" button) bypasses the valid
    // short-circuit and re-issues even a currently-valid certificate.
    let result = acme_orchestrator::ensure_acme_cert(&state, &cfg, true).await;
    assert_eq!(result["issued"], true);
    assert_eq!(fake.calls().len(), 1);
    // A successful (re)issue surfaces its human-readable `logs` in the result.
    assert_eq!(
        result["logs"],
        json!("fake acmex: issued"),
        "issued run should surface logs"
    );
}

#[tokio::test]
async fn test_ensure_acme_cert_cloudflare_provider() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let cfg = default_cfg();
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    acme_orchestrator::ensure_acme_cert(&state, &cfg, false).await;
    let call = &fake.calls()[0];
    // Cloudflare DNS-01 is the default provider; the token/email flow through.
    assert_eq!(call.provider, "cloudflare");
    assert_eq!(call.auth, "dns-cloudflare");
    assert_eq!(call.email, "admin@example.com");
}

#[tokio::test]
async fn test_ensure_acme_cert_acmex_error() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let cfg = default_cfg();
    let _guard = IssuerGuard::new(FakeAcmeIssuer::failing("DNS challenge timed out"));
    let result = acme_orchestrator::ensure_acme_cert(&state, &cfg, false).await;
    assert_eq!(result["issued"], false);
    assert!(result["error"]
        .as_str()
        .unwrap()
        .contains("DNS challenge timed out"));
    // Even on failure the issuer's logs are surfaced, alongside the error.
    assert_eq!(result["logs"], json!("fake acmex: failed"), "failed run should still surface logs");
}

#[tokio::test]
async fn test_ensure_acme_cert_zerossl_ca() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let mut cfg = default_cfg();
    cfg.acme_ca = "zerossl".to_string();
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    acme_orchestrator::ensure_acme_cert(&state, &cfg, false).await;
    // The selected CA is passed through to the issuer.
    assert_eq!(fake.calls()[0].ca, "zerossl");
}

#[tokio::test]
async fn test_ensure_acme_cert_google_ca() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let mut cfg = default_cfg();
    cfg.acme_ca = "google".to_string();
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    acme_orchestrator::ensure_acme_cert(&state, &cfg, false).await;
    assert_eq!(fake.calls()[0].ca, "google");
}

// --- renew_acme_cert (acmex path, fake issuer) -------------------------------
#[tokio::test]
async fn test_renew_acme_cert_not_due() {
    let paths = acme_paths();
    write_cert_tree(std::path::Path::new(&paths.app), "example.com", 90);
    let state = build_state(TempDir::new().path()).await;
    let cfg = default_cfg();
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    let result = acme_orchestrator::renew_acme_cert(&state, &cfg).await;
    assert_eq!(result["due"], false);
    assert_eq!(result["renewed"], false);
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn test_renew_acme_cert_due_and_renews() {
    let paths = acme_paths();
    write_cert_tree(std::path::Path::new(&paths.app), "example.com", 10);
    let state = build_state(TempDir::new().path()).await;
    let cfg = default_cfg();
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    let result = acme_orchestrator::renew_acme_cert(&state, &cfg).await;
    assert_eq!(result["due"], true);
    assert_eq!(result["renewed"], true);
    assert_eq!(fake.calls().len(), 1);
    // A renewal also surfaces its `logs` in the returned status.
    assert_eq!(
        result["logs"],
        json!("fake acmex: issued"),
        "renew run should surface logs"
    );
    // The renewed bundle is now persisted to the config DB (source of truth).
    let rec = acme_orchestrator::acme_cert_record(&state).await.expect("renewed record persisted");
    assert_eq!(rec.cert_pem, fake.cert_pem);
    assert!(rec.expires.is_some());
}

#[tokio::test]
async fn test_renew_acme_cert_disabled() {
    let _paths = acme_paths();
    let state = build_state(TempDir::new().path()).await;
    let mut cfg = default_cfg();
    cfg.acme_enabled = false;
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    let result = acme_orchestrator::renew_acme_cert(&state, &cfg).await;
    assert_eq!(result["due"], false);
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn test_renew_acme_cert_acmex_error() {
    let paths = acme_paths();
    write_cert_tree(std::path::Path::new(&paths.app), "example.com", 10);
    let state = build_state(TempDir::new().path()).await;
    let cfg = default_cfg();
    let _guard = IssuerGuard::new(FakeAcmeIssuer::failing("rate limited"));
    let result = acme_orchestrator::renew_acme_cert(&state, &cfg).await;
    assert_eq!(result["due"], true);
    assert_eq!(result["renewed"], false);
    assert!(result["error"].as_str().unwrap().contains("rate limited"));
}

// --- effective_acme_config ---------------------------------------------------
#[tokio::test]
async fn test_effective_acme_config_defaults() {
    // With no DB rows and no env, the defaults apply.
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let cfg = acme_orchestrator::effective_acme_config(&state).await;
    assert!(!cfg.acme_enabled); // default
    assert_eq!(cfg.acme_auth, "dns-cloudflare");
    assert_eq!(cfg.acme_dns_provider, "cloudflare");
    assert_eq!(cfg.acme_renew_before_days, 30);
}

#[tokio::test]
async fn test_effective_acme_config_env_override() {
    let tmp = TempDir::new();
    let mut state = build_state(tmp.path()).await;
    // `get_setting` consults the settings instance attribute (populated from
    // the environment at import), so set it to simulate an env override.
    state.settings.acme_enabled = true;
    state.settings.acme_domains = "a.com,b.com".to_string();
    let cfg = acme_orchestrator::effective_acme_config(&state).await;
    assert!(cfg.acme_enabled);
    assert_eq!(cfg.acme_domains, "a.com,b.com");
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
    assert_eq!(data["values"]["acme_enabled"], false);
    assert_eq!(data["values"]["acme_auth"], "dns-cloudflare");
    assert!(data["values"].get("acme_domains").is_some());
    assert!(data.get("status").is_some());
    assert_eq!(data["status"]["present"], false);
    // The token is masked, not echoed.
    assert_eq!(data["values"]["acme_cloudflare_token"], "");
}

#[tokio::test]
async fn test_certificates_put_and_get() {
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let client = admin_client(state).await;
    let payload = json!({
        "acme_enabled": true,
        "acme_domains": ["example.com", "www.example.com"],
        "acme_auth": "dns-cloudflare",
        "acme_cloudflare_email": "admin@example.com",
        "acme_cloudflare_token": "cf-token-123",
        "acme_cloudflare_zone_id": "zone-abc",
        "acme_staging": true,
        "acme_auto_renew": true,
        "acme_renew_check_hours": 12,
        "acme_renew_before_days": 20,
    });
    let (status, _) = client.call("PUT", "/api/admin/certificates", Some(&payload)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, data) = client.call("GET", "/api/admin/certificates", None).await;
    assert_eq!(status, StatusCode::OK);
    let data = data.expect("body");
    let v = &data["values"];
    assert_eq!(v["acme_enabled"], true);
    assert_eq!(v["acme_domains"], json!(["example.com", "www.example.com"]));
    assert_eq!(v["acme_staging"], true);
    assert_eq!(v["acme_renew_check_hours"], 12);
    assert_eq!(v["acme_renew_before_days"], 20);
    // The token is masked but reported as set.
    assert_eq!(v["acme_cloudflare_token"], "");
    assert_eq!(v["token_set"], true);
    assert_eq!(v["token_length"], "cf-token-123".len() as i64);
    // The value now comes from the DB (edited in the UI).
    assert_eq!(data["sources"]["acme_enabled"], "db");
}

#[tokio::test]
async fn test_certificates_put_blank_token_keeps_existing() {
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let client = admin_client(state).await;
    // First set a token.
    let first = json!({
        "acme_enabled": true,
        "acme_domains": ["example.com"],
        "acme_auth": "dns-cloudflare",
        "acme_cloudflare_email": "admin@example.com",
        "acme_cloudflare_token": "original-token",
        "acme_cloudflare_zone_id": "zone-abc",
        "acme_staging": false,
        "acme_auto_renew": true,
        "acme_renew_check_hours": 6,
        "acme_renew_before_days": 30,
    });
    client.call("PUT", "/api/admin/certificates", Some(&first)).await;
    // Then save again with a blank token — the existing one should be kept.
    let mut second = first.clone();
    second["acme_cloudflare_token"] = json!("");
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
    assert_eq!(status, StatusCode::BAD_REQUEST); // ACME not enabled by default
}

#[tokio::test]
async fn test_certificates_renew_no_domains() {
    let tmp = TempDir::new();
    let state = build_state(tmp.path()).await;
    let client = admin_client(state).await;
    // Enable ACME but leave domains empty.
    let payload = json!({
        "acme_enabled": true,
        "acme_domains": [],
        "acme_auth": "dns-cloudflare",
        "acme_cloudflare_email": "admin@example.com",
        "acme_cloudflare_token": "t",
        "acme_cloudflare_zone_id": "zone-abc",
        "acme_staging": false,
        "acme_auto_renew": true,
        "acme_renew_check_hours": 6,
        "acme_renew_before_days": 30,
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
    // Enable ACME with a domain, then renew (no valid cert yet -> issues one).
    let payload = json!({
        "acme_enabled": true,
        "acme_domains": ["example.com"],
        "acme_auth": "dns-cloudflare",
        "acme_cloudflare_email": "admin@example.com",
        "acme_cloudflare_token": "t",
        "acme_cloudflare_zone_id": "zone-abc",
        "acme_staging": false,
        "acme_auto_renew": true,
        "acme_renew_check_hours": 6,
        "acme_renew_before_days": 30,
    });
    client.call("PUT", "/api/admin/certificates", Some(&payload)).await;
    // Point the module's cert dirs at a temp tree and mock the ACME issuer.
    let _paths = acme_paths();
    let fake = FakeAcmeIssuer::ok_issuer();
    let _guard = IssuerGuard::new(fake.clone());
    let (status, data) = client.call("POST", "/api/admin/certificates/renew", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.expect("body")["issued"], true);
    assert_eq!(fake.calls().len(), 1);
    assert_eq!(fake.calls()[0].provider, "cloudflare");
}

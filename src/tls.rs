//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! TLS certificate selection.
//!
//! Ports the cert logic in `run_https`. When ACME is enabled the real cert is
//! issued/renewed by the native acmex backend (`acme_orchestrator::ensure_acme_cert`);
//! otherwise a generated self-signed cert (CN=localhost) is used.
//! `select_cert` issues/renews when due, then serves the cert if a valid one is
//! now present, else falls back to the self-signed cert.

use crate::acme_orchestrator;
use crate::state::AppState;
use anyhow::Result;
use std::path::PathBuf;
use tracing::{info, warn};

/// Pick the TLS cert: an ACME-issued cert when ACME is enabled and a valid
/// cert is available, else a self-signed cert. Returns the PEM-encoded cert
/// and key. Mirrors `_select_cert` in `run_https`.
pub async fn select_cert(state: &AppState) -> Result<(Vec<u8>, Vec<u8>)> {
    let settings = &state.settings;
    let ssl_dir = PathBuf::from(&settings.data_root).join("ssl");
    std::fs::create_dir_all(&ssl_dir)?;
    let cert_path = ssl_dir.join("vreckan_dev.crt");
    let key_path = ssl_dir.join("vreckan_dev.key");

    // ACME: read the *effective* config (DB-over-env), issue/renew via the
    // native acmex backend when due, then serve the cert if a valid one is
    // now present on disk.
    let cfg = acme_orchestrator::effective_acme_config(state).await;
    if cfg.acme_enabled {
        let issued = acme_orchestrator::ensure_acme_cert(state, &cfg, false)
            .await
            .get("issued")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let domains: Vec<String> = cfg
            .acme_domains
            .split(',')
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect();
        let status = acme_orchestrator::cert_status(state, &domains).await;
        if status.get("present").and_then(|v| v.as_bool()).unwrap_or(false)
            && status.get("valid").and_then(|v| v.as_bool()).unwrap_or(false)
        {
            let source = status.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let cert = status.get("cert").and_then(|v| v.as_str()).unwrap_or("");
            let key = status.get("key").and_then(|v| v.as_str()).unwrap_or("");
            if !cert.is_empty() && !key.is_empty() {
                // The config DB is the source of truth: for an `acme_db`
                // record the cert/key are PEM content; for a disk fallback
                // they are on-disk paths to read.
                let (cert_bytes, key_bytes) = if source == "acme_db" {
                    (cert.as_bytes().to_vec(), key.as_bytes().to_vec())
                } else {
                    (std::fs::read(cert)?, std::fs::read(key)?)
                };
                info!(
                    "using ACME cert (source={}, {}d left)",
                    source,
                    status.get("days_remaining").and_then(|v| v.as_i64()).unwrap_or(0)
                );
                return Ok((cert_bytes, key_bytes));
            }
        }
        warn!(
            "ACME enabled but no valid cert yet (issued={}); using self-signed until one is available",
            issued
        );
    } else {
        info!("ACME not enabled; using self-signed certificate");
    }

    let expiry_path = ssl_dir.join("vreckan_dev.expiry");
    if dev_cert_needs_generation(&cert_path, &key_path, &expiry_path) {
        generate_self_signed(&cert_path, &key_path, &expiry_path)?;
        info!("wrote self-signed cert {}", cert_path.display());
    } else {
        info!("reusing existing {}", cert_path.display());
    }

    let cert = std::fs::read(&cert_path)?;
    let key = std::fs::read(&key_path)?;
    Ok((cert, key))
}

/// Validity of the self-signed dev cert, in days. rcgen's default validity
/// runs to the year 4096 (effectively never expires); the explicit window is
/// what makes the expiry check below meaningful (ANT-275, prior P3-10).
const DEV_CERT_VALIDITY_DAYS: i64 = 3 * 365;

/// Whether the self-signed dev cert/key pair needs (re)generation: either
/// file is missing, no expiry was recorded (a cert left behind by an older
/// version), or the recorded expiry has passed.
pub(crate) fn dev_cert_needs_generation(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
    expiry_path: &std::path::Path,
) -> bool {
    if !cert_path.exists() || !key_path.exists() {
        return true;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let recorded = std::fs::read_to_string(expiry_path)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());
    recorded.map_or(true, |e| e <= now)
}

/// Generate a self-signed RSA-2048 cert (CN=localhost, SAN localhost +
/// 127.0.0.1) and write the PEM cert, key, and expiry sidecar to disk.
fn generate_self_signed(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
    expiry_path: &std::path::Path,
) -> Result<()> {
    use rcgen::{
        BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
        ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
    };

    let key_pair = KeyPair::generate()?;

    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into().unwrap()),
        SanType::DnsName("127.0.0.1".try_into().unwrap()),
    ];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "localhost");
    dn.push(DnType::OrganizationName, "Vreckan Dev");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
        KeyUsagePurpose::KeyCertSign,
    ];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];

    // Explicit validity: rcgen's default runs to the year 4096, so an old
    // cert would look valid forever (ANT-275, prior P3-10).
    let not_before = time::OffsetDateTime::now_utc();
    let not_after = not_before + time::Duration::days(DEV_CERT_VALIDITY_DAYS);
    params.not_before = not_before;
    params.not_after = not_after;

    let cert: Certificate = params.self_signed(&key_pair)?;
    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;
    // The key is private — same mode as the ACME account key and the E2EE
    // key (ANT-275).
    let _ = std::fs::set_permissions(
        key_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    );
    // Record the expiry so `dev_cert_needs_generation` can retire an expired
    // cert instead of reusing it on `exists()` alone.
    std::fs::write(expiry_path, format!("{}\n", not_after.unix_timestamp()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vreckan-tls-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dev_cert_needs_generation_truth_table() {
        let dir = test_dir("truth-table");
        let cert = dir.join("c.crt");
        let key = dir.join("c.key");
        let exp = dir.join("c.expiry");
        std::fs::write(&cert, "cert").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        // Key missing → regenerate.
        assert!(dev_cert_needs_generation(&cert, &key, &exp));
        std::fs::write(&key, "key").unwrap();
        // No recorded expiry (legacy cert) → regenerate.
        assert!(dev_cert_needs_generation(&cert, &key, &exp));
        // Future expiry → reuse.
        std::fs::write(&exp, (now + 1000).to_string()).unwrap();
        assert!(!dev_cert_needs_generation(&cert, &key, &exp));
        // Expired → regenerate.
        std::fs::write(&exp, (now - 1).to_string()).unwrap();
        assert!(dev_cert_needs_generation(&cert, &key, &exp));
        // Malformed expiry → regenerate.
        std::fs::write(&exp, "not-a-number").unwrap();
        assert!(dev_cert_needs_generation(&cert, &key, &exp));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_self_signed_sets_0600_key_and_expiry_sidecar() {
        let dir = test_dir("gen");
        let cert = dir.join("c.crt");
        let key = dir.join("c.key");
        let exp = dir.join("c.expiry");
        generate_self_signed(&cert, &key, &exp).unwrap();
        // The private key is 0600 (ANT-275).
        let mode = std::fs::metadata(&key)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "key mode {mode:o}");
        // The sidecar records a future expiry ~3 years out.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let recorded = std::fs::read_to_string(&exp).unwrap().trim().parse::<i64>().unwrap();
        let delta_days = (recorded - now) / 86400;
        assert!((3 * 365 - 2..=3 * 365 + 2).contains(&delta_days), "delta_days={delta_days}");
        // And the pair no longer needs regeneration.
        assert!(!dev_cert_needs_generation(&cert, &key, &exp));
        std::fs::remove_dir_all(&dir).ok();
    }
}

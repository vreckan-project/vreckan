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

    if !cert_path.exists() || !key_path.exists() {
        generate_self_signed(&cert_path, &key_path)?;
        info!("wrote self-signed cert {}", cert_path.display());
    } else {
        info!("reusing existing {}", cert_path.display());
    }

    let cert = std::fs::read(&cert_path)?;
    let key = std::fs::read(&key_path)?;
    Ok((cert, key))
}

/// Generate a self-signed RSA-2048 cert (CN=localhost, SAN localhost +
/// 127.0.0.1) and write the PEM cert and key to disk.
fn generate_self_signed(cert_path: &std::path::Path, key_path: &std::path::Path) -> Result<()> {
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

    let cert: Certificate = params.self_signed(&key_pair)?;
    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;
    Ok(())
}

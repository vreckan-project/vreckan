//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! TLS certificate selection.
//!
//! Ports the cert logic in `run_https`. When Let's Encrypt is enabled the
//! real cert is issued/renewed via a transient certbot container (that logic
//! lives in `le_certbot`, ported in a later step); otherwise a generated
//! self-signed cert (CN=localhost) is used. For now this returns the
//! self-signed cert, generating it on first run if it is missing.

use crate::le_certbot;
use crate::state::AppState;
use anyhow::Result;
use std::path::PathBuf;
use tracing::{info, warn};

/// Pick the TLS cert: a Let's Encrypt cert (issued via certbot when LE is
/// enabled, or a host-mounted one) when available, else a self-signed cert.
/// Returns the PEM-encoded cert and key. Mirrors `_select_cert` in
/// `run_https`.
pub async fn select_cert(state: &AppState) -> Result<(Vec<u8>, Vec<u8>)> {
    let settings = &state.settings;
    let ssl_dir = PathBuf::from(&settings.data_root).join("ssl");
    std::fs::create_dir_all(&ssl_dir)?;
    let cert_path = ssl_dir.join("vreckan_dev.crt");
    let key_path = ssl_dir.join("vreckan_dev.key");

    // Let's Encrypt: issue/renew via a transient certbot container, then use
    // the cert if it is present and valid. Mirrors `run_https`, which reads
    // the *effective* config (DB-over-env) before calling `ensure_le_cert`.
    let cfg = le_certbot::effective_le_config(state).await;
    if cfg.le_enabled {
        let status = le_certbot::ensure_le_cert(&cfg).await;
        if status.get("present").and_then(|v| v.as_bool()).unwrap_or(false)
            && status.get("valid").and_then(|v| v.as_bool()).unwrap_or(false)
        {
            let cert = status.get("cert").and_then(|v| v.as_str()).unwrap_or("");
            let key = status.get("key").and_then(|v| v.as_str()).unwrap_or("");
            if !cert.is_empty() && !key.is_empty() {
                info!(
                    "using Let's Encrypt cert {cert} (source={}, {}d left)",
                    status.get("source").and_then(|v| v.as_str()).unwrap_or("?"),
                    status.get("days_remaining").and_then(|v| v.as_i64()).unwrap_or(0)
                );
                return Ok((std::fs::read(cert)?, std::fs::read(key)?));
            }
        }
        warn!(
            "LE enabled but no valid cert yet (issued={}); using self-signed until one is available",
            status.get("issued").and_then(|v| v.as_bool()).unwrap_or(false)
        );
    } else {
        info!("LE not enabled; using self-signed certificate");
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

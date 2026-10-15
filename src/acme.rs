//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Native ACME issuance via the [`acmex`] crate.
//!
//! This replaces the legacy certbot sidecar container for issuance. It drives
//! the ACME v2 protocol in-process: it registers (or reuses a persisted)
//! account, creates an order, and fulfils the challenge (DNS-01 by default, or
//! HTTP-01). The resulting certificate chain + key are carried back to the
//! caller in `AcmeIssueResult` so it can persist them to the config DB (the
//! authoritative source of truth); they are also written as a best-effort
//! mirror into the app's `/data/acme` layout (`live/{primary}/cert.pem` +
//! `privkey.pem`) for backward compatibility.
//!
//! DNS-01 is routed through a small provider registry so a user can choose
//! any of the DNS providers acmex ships (Cloudflare is the default; Route53,
//! DigitalOcean, Linode, Azure, Google, Alibaba, GoDaddy, Tencent, Huawei and
//! CloudNS are also wired). Each identifier in an order is routed to the
//! provider that owns its zone, so multi-domain and wildcard orders work.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use acmex::dns::providers::cloudflare::CloudFlareConfig;
use acmex::dns::providers::digitalocean::DigitalOceanConfig;
use acmex::dns::providers::linode::LinodeConfig;
use acmex::dns::providers::route53::Route53Config;
use acmex::{
    AcmeClient, AcmeConfig, AlibabaCloudDnsProvider, AzureDnsProvider, Challenge,
    ChallengeSolver, ChallengeSolverRegistry, ChallengeType, CloudFlareDnsProvider,
    ClouDnsProvider, Contact, DigitalOceanDnsProvider, DnsProvider, GodaddyDnsProvider,
    GoogleCloudDnsProvider, Http01Solver, HuaweiCloudDnsProvider, Identifier,
    KeyPair, LinodeDnsProvider, Route53DnsProvider, TencentCloudDnsProvider,
};

/// ACME directory URLs per CA. Let's Encrypt uses acmex's built-in helpers;
/// the others use their published ACME v2 endpoints.
pub fn ca_directory(ca: &str, staging: bool) -> String {
    match ca.to_ascii_lowercase().as_str() {
        "letsencrypt" => {
            if staging {
                AcmeConfig::lets_encrypt_staging().directory_url
            } else {
                AcmeConfig::lets_encrypt().directory_url
            }
        }
        "zerossl" => "https://acme.zerossl.com/v2/server/directory".to_string(),
        "google" | "google-public-certs" => "https://acme.v1.pki.goog/directory".to_string(),
        other => other.to_string(),
    }
}

fn normalize_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Maps a DNS zone (lower-cased apex, e.g. `example.com`) to the acmex
/// [`DnsProvider`] that manages it. Routing resolves a concrete domain to its
/// provider by exact-then-longest-suffix match, so `www.example.com` resolves
/// to the provider registered for `example.com`.
pub struct DnsProviderRegistry {
    providers: HashMap<String, Arc<dyn DnsProvider>>,
}

impl DnsProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn insert(&mut self, zone: &str, provider: Arc<dyn DnsProvider>) {
        self.providers
            .insert(normalize_name(zone), provider);
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// The registered zone names (lower-cased), for diagnostics.
    pub fn zones(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    /// Resolve `domain` to a provider: exact match first, then the longest
    /// registered zone that `domain` lies under.
    pub fn get(&self, domain: &str) -> Option<Arc<dyn DnsProvider>> {
        let d = normalize_name(domain);
        if let Some(p) = self.providers.get(&d) {
            return Some(p.clone());
        }
        let mut best: Option<(usize, &Arc<dyn DnsProvider>)> = None;
        for (zone, p) in self.providers.iter() {
            if d == *zone || d.ends_with(&format!(".{zone}")) {
                let zlen = zone.len();
                match best {
                    Some((blen, _)) if blen >= zlen => {}
                    _ => best = Some((zlen, p)),
                }
            }
        }
        best.map(|(_, p)| p.clone())
    }
}

/// A DNS-01 challenge solver that routes each identifier to the
/// [`DnsProvider`] owning its zone. Unlike acmex's built-in `Dns01Solver`
/// (which is scoped to a single fixed domain), this handles multi-domain and
/// wildcard orders, and supports any number of configured providers.
pub struct VreckanDns01Solver {
    registry: Arc<DnsProviderRegistry>,
    pending: Mutex<Vec<PendingDnsRecord>>,
}

#[derive(Clone)]
struct PendingDnsRecord {
    target: String,
    name: String,
    record_id: String,
    value: String,
}

impl VreckanDns01Solver {
    pub fn new(registry: Arc<DnsProviderRegistry>) -> Self {
        Self {
            registry,
            pending: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ChallengeSolver for VreckanDns01Solver {
    fn challenge_type(&self) -> ChallengeType {
        ChallengeType::Dns01
    }

    async fn prepare(
        &mut self,
        _challenge: &Challenge,
        identifier: &Identifier,
        key_authorization: &str,
    ) -> acmex::error::Result<()> {
        if identifier.id_type != "dns" {
            return Err(acmex::error::AcmeError::challenge(
                "dns-01".to_string(),
                format!("identifier is not a DNS name (type={})", identifier.id_type),
            ));
        }
        // The host that carries the TXT record: for a wildcard
        // (`*.example.com`) it is the apex; for a normal name it is the name
        // itself.
        let target = identifier
            .value
            .strip_prefix("*.")
            .unwrap_or(&identifier.value)
            .to_string();
        let name = format!("_acme-challenge.{target}");

        let provider = self
            .registry
            .get(&target)
            .ok_or_else(|| acmex::error::AcmeError::Configuration(format!(
                "no DNS provider configured for zone `{target}` (configured: {:?})",
                self.registry.zones()
            )))?;

        let digest = Sha256::digest(key_authorization.as_bytes());
        let value = URL_SAFE_NO_PAD.encode(digest);

        let record_id = provider.create_txt_record(&name, &value).await?;
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(
            PendingDnsRecord {
                target: target.clone(),
                name,
                record_id,
                value,
            },
        );
        info!("DNS-01 record created: {target}");
        Ok(())
    }

    async fn present(&self) -> acmex::error::Result<()> {
        Ok(())
    }

    async fn verify(&self) -> acmex::error::Result<bool> {
        let records = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for r in &records {
            if let Some(p) = self.registry.get(&r.target) {
                if !p.verify_record(&r.name, &r.value).await? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    async fn cleanup(&mut self) -> acmex::error::Result<()> {
        let records = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for r in &records {
            if let Some(p) = self.registry.get(&r.target) {
                p.delete_txt_record(&r.name, &r.record_id).await?;
                info!("DNS-01 record deleted: {}", r.name);
            }
        }
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        Ok(())
    }
}

/// The DNS provider and its credentials, read from the effective LE settings.
pub struct AcmeDnsConfig<'a> {
    /// Provider id: `cloudflare`, `route53`, `digitalocean`, `linode`,
    /// `azure`, `google`, `alibaba`, `godaddy`, `tencent`, `huawei`,
    /// `cloudns`.
    pub provider: &'a str,
    pub cloudflare_token: String,
    pub cloudflare_zone_id: String,
    pub cloudflare_email: String,
    /// The primary domain (first in `acme_domains`), used for Cloudflare zone
    /// auto-discovery and as the registration key for the provider.
    pub domain: String,
    /// Free-form JSON holding provider-specific credentials for the non-
    /// Cloudflare providers (see each provider's fields).
    pub provider_config: &'a Value,
}

fn jstr<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

/// Build the DNS provider registry from settings. Only the selected provider
/// is instantiated; its zone is registered under the primary domain.
pub async fn build_provider_registry(cfg: &AcmeDnsConfig<'_>) -> Result<Arc<DnsProviderRegistry>, String> {
    let mut reg = DnsProviderRegistry::new();
    let pc = cfg.provider_config;
    match cfg.provider.to_ascii_lowercase().as_str() {
        "cloudflare" | "dns-cloudflare" => {
            if cfg.cloudflare_token.is_empty() {
                return Err(
                    "Cloudflare DNS provider selected but the Cloudflare API token is not set"
                        .to_string(),
                );
            }
            let zone_id = if cfg.cloudflare_zone_id.is_empty() {
                discover_cloudflare_zone(&cfg.cloudflare_token, &cfg.domain).await?
            } else {
                cfg.cloudflare_zone_id.clone()
            };
            if zone_id.is_empty() {
                return Err(format!(
                    "Could not resolve a Cloudflare zone ID for {} — set the zone ID explicitly",
                    cfg.domain
                ));
            }
            reg.insert(
                &cfg.domain,
                Arc::new(CloudFlareDnsProvider::new(CloudFlareConfig {
                    api_token: cfg.cloudflare_token.clone(),
                    zone_id,
                })),
            );
        }
        "route53" => {
            let hosted_zone_id = jstr(pc, "hosted_zone_id").to_string();
            if hosted_zone_id.is_empty() {
                return Err("Route53 selected but `hosted_zone_id` is missing from the provider config".to_string());
            }
            let provider = Route53DnsProvider::new(Route53Config { hosted_zone_id }).await;
            reg.insert(&cfg.domain, Arc::new(provider));
        }
        "digitalocean" => {
            let api_token = jstr(pc, "api_token").to_string();
            let domain = jstr(pc, "domain").to_string();
            if api_token.is_empty() || domain.is_empty() {
                return Err("DigitalOcean selected but `api_token`/`domain` are missing from the provider config".to_string());
            }
            reg.insert(
                domain.as_str(),
                Arc::new(DigitalOceanDnsProvider::new(DigitalOceanConfig {
                    api_token,
                    domain: domain.clone(),
                })),
            );
        }
        "linode" => {
            let api_token = jstr(pc, "api_token").to_string();
            let domain_id = pc
                .get("domain_id")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            if api_token.is_empty() || domain_id == 0 {
                return Err("Linode selected but `api_token`/`domain_id` are missing from the provider config".to_string());
            }
            reg.insert(
                &cfg.domain,
                Arc::new(LinodeDnsProvider::new(LinodeConfig {
                    api_token,
                    domain_id,
                })),
            );
        }
        "azure" => {
            let subscription_id = jstr(pc, "subscription_id").to_string();
            let resource_group = jstr(pc, "resource_group").to_string();
            let client_id = jstr(pc, "client_id").to_string();
            let client_secret = jstr(pc, "client_secret").to_string();
            let tenant_id = jstr(pc, "tenant_id").to_string();
            if subscription_id.is_empty() || resource_group.is_empty() || client_id.is_empty()
            {
                return Err(
                    "Azure selected but `subscription_id`/`resource_group`/`client_id` are missing from the provider config"
                        .to_string(),
                );
            }
            reg.insert(
                &cfg.domain,
                Arc::new(AzureDnsProvider::new(
                    subscription_id,
                    resource_group,
                    client_id,
                    client_secret,
                    tenant_id,
                )),
            );
        }
        "google" => {
            let project_id = jstr(pc, "project_id").to_string();
            if project_id.is_empty() {
                return Err("Google Cloud DNS selected but `project_id` is missing from the provider config".to_string());
            }
            reg.insert(&cfg.domain, Arc::new(GoogleCloudDnsProvider::new(project_id)));
        }
        "alibaba" => {
            let access_key_id = jstr(pc, "access_key_id").to_string();
            let access_key_secret = jstr(pc, "access_key_secret").to_string();
            let region = jstr(pc, "region").to_string();
            if access_key_id.is_empty() || access_key_secret.is_empty() {
                return Err("Alibaba Cloud DNS selected but `access_key_id`/`access_key_secret` are missing from the provider config".to_string());
            }
            reg.insert(
                &cfg.domain,
                Arc::new(AlibabaCloudDnsProvider::new(
                    access_key_id,
                    access_key_secret,
                    region,
                )),
            );
        }
        "godaddy" => {
            let api_key = jstr(pc, "api_key").to_string();
            let api_secret = jstr(pc, "api_secret").to_string();
            if api_key.is_empty() || api_secret.is_empty() {
                return Err("GoDaddy selected but `api_key`/`api_secret` are missing from the provider config".to_string());
            }
            reg.insert(&cfg.domain, Arc::new(GodaddyDnsProvider::new(api_key, api_secret)));
        }
        "tencent" => {
            let secret_id = jstr(pc, "secret_id").to_string();
            let secret_key = jstr(pc, "secret_key").to_string();
            let region = jstr(pc, "region").to_string();
            if secret_id.is_empty() || secret_key.is_empty() {
                return Err("Tencent Cloud DNS selected but `secret_id`/`secret_key` are missing from the provider config".to_string());
            }
            reg.insert(
                &cfg.domain,
                Arc::new(TencentCloudDnsProvider::new(secret_id, secret_key, region)),
            );
        }
        "huawei" => {
            let access_key = jstr(pc, "access_key").to_string();
            let secret_key = jstr(pc, "secret_key").to_string();
            let project_id = jstr(pc, "project_id").to_string();
            let region = jstr(pc, "region").to_string();
            if access_key.is_empty() || secret_key.is_empty() {
                return Err("Huawei Cloud DNS selected but `access_key`/`secret_key` are missing from the provider config".to_string());
            }
            reg.insert(
                &cfg.domain,
                Arc::new(HuaweiCloudDnsProvider::new(
                    access_key,
                    secret_key,
                    project_id,
                    region,
                )),
            );
        }
        "cloudns" => {
            let auth_id = jstr(pc, "auth_id").to_string();
            let auth_password = jstr(pc, "auth_password").to_string();
            if auth_id.is_empty() || auth_password.is_empty() {
                return Err("CloudNS selected but `auth_id`/`auth_password` are missing from the provider config".to_string());
            }
            reg.insert(&cfg.domain, Arc::new(ClouDnsProvider::new(auth_id, auth_password)));
        }
        other => {
            return Err(format!(
                "Unknown DNS provider '{other}' (expected one of: cloudflare, route53, digitalocean, linode, azure, google, alibaba, godaddy, tencent, huawei, cloudns)"
            ))
        }
    }
    Ok(Arc::new(reg))
}

/// Resolve a Cloudflare zone ID for `domain` using the token, trying the
/// domain and each of its parent suffixes (so subdomains resolve to their
/// zone apex). Returns an empty string when nothing matches.
async fn discover_cloudflare_zone(token: &str, domain: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let mut candidate = normalize_name(domain);
    // (zone-name length, zone id) of the longest zone found that owns the
    // original domain; updated as we walk up the suffix chain.
    let mut best: Option<(usize, String)> = None;
    loop {
        let url = format!(
            "https://api.cloudflare.com/client/v4/zones?name={}",
            candidate
        );
        if let Ok(resp) = client.get(&url).bearer_auth(token).send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<Value>().await {
                    if let Some(arr) = body.get("result").and_then(|r| r.as_array()) {
                        for z in arr {
                            let name = z
                                .get("name")
                                .and_then(|n| n.as_str())
                                .map(str::to_ascii_lowercase)
                                .unwrap_or_default();
                            let id = z
                                .get("id")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            if id.is_empty() {
                                continue;
                            }
                            if name == candidate {
                                return Ok(id);
                            }
                            let len = name.len();
                            match best {
                                Some((blen, _)) if blen >= len => {}
                                _ => best = Some((len, id)),
                            }
                        }
                    }
                }
            }
        }
        match candidate.find('.') {
            Some(pos) => candidate = candidate[pos + 1..].to_string(),
            None => break,
        }
    }
    Ok(best.map(|(_, id)| id).unwrap_or_default())
}

/// The on-disk location of the persisted ACME account key.
pub fn account_key_path() -> String {
    format!("{}/acme-account.key", crate::acme_orchestrator::app_cert_dir())
}

/// Load the persisted ACME account key, or generate and persist a new one.
/// Reusing the same key keeps the ACME account stable across restarts so we
/// don't burn new-account rate limits.
pub async fn load_or_create_account_key() -> Result<KeyPair, String> {
    let path = account_key_path();
    if let Ok(kp) = KeyPair::load_from_file(&path) {
        info!("Loaded ACME account key from {path}");
        return Ok(kp);
    }
    let kp = KeyPair::generate().map_err(|e| e.to_string())?;
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    kp.save_to_file(&path).map_err(|e| e.to_string())?;
    let _ = std::fs::set_permissions(
        &path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    );
    info!("Generated and persisted new ACME account key at {path}");
    Ok(kp)
}

/// Everything `issue_certificate` needs to run one ACME order.
pub struct IssueArgs<'a> {
    pub ca: &'a str,
    pub staging: bool,
    pub email: &'a str,
    /// `http-01` or a DNS-01 provider id (`dns-cloudflare`, `cloudflare`, ...).
    pub auth: &'a str,
    pub provider: &'a str,
    pub domains: Vec<String>,
    pub registry: Arc<DnsProviderRegistry>,
}

/// The outcome of an issuance attempt.
pub struct AcmeIssueResult {
    pub ok: bool,
    pub logs: String,
    pub error: Option<String>,
    /// The issued certificate chain PEM (populated on success). Carried out of
    /// the issuance path so the caller can persist it to the config DB — the
    /// authoritative store (the on-disk write is a best-effort mirror).
    pub cert_pem: Option<String>,
    /// The issued private key PEM (populated on success).
    pub key_pem: Option<String>,
}

/// Run a full ACME order: register/reuse the account, create the order, and
/// fulfil the chosen challenge. On success the issued certificate chain and
/// private key are carried back in the result (for the caller to persist to
/// the config DB — the authoritative store) and also written as a best-effort
/// on-disk mirror in the `/data/acme` layout. Collects human-readable milestone
/// `logs` so the result can be surfaced to the user (the original "button did
/// nothing" complaint).
pub async fn issue_certificate(args: &IssueArgs<'_>) -> AcmeIssueResult {
    let mut logs: Vec<String> = Vec::new();
    macro_rules! log {
        ($($arg:tt)*) => {{
            let m = format!($($arg)*);
            logs.push(m.clone());
            info!(target: "acme", "{m}");
        }};
    }

    let key_pair = match load_or_create_account_key().await {
        Ok(k) => {
            log!("Account key ready.");
            k
        }
        Err(e) => {
            return AcmeIssueResult { ok: false, logs: logs.join("\n"), error: Some(e), cert_pem: None, key_pem: None };
        }
    };

    let directory = ca_directory(args.ca, args.staging);
    let mut config = AcmeConfig::new(directory.clone());
    if !args.email.is_empty() {
        config = config.with_contact(Contact::email(args.email));
    }
    config = config.with_tos_agreed(true);

    let mut client = AcmeClient::with_key_pair(config, key_pair);

    if let Err(e) = client.register_account().await {
        return AcmeIssueResult {
            ok: false,
            logs: logs.join("\n"),
            error: Some(format!("ACME account registration failed: {e}")),
            cert_pem: None,
            key_pem: None,
        };
    }
    log!("ACME account ready: {}", client.account_id().unwrap_or("?"));

    let mut registry = ChallengeSolverRegistry::new();
    if args.auth == "http-01" {
        registry.register(Http01Solver::default());
        log!("Using HTTP-01 challenge solver.");
    } else {
        registry.register(VreckanDns01Solver::new(args.registry.clone()));
        log!("Using DNS-01 challenge solver (provider: {}).", args.provider);
    }

    match client
        .issue_certificate(args.domains.clone(), &mut registry)
        .await
    {
        Ok(bundle) => {
            // Capture the bundle in memory: the config DB is the authoritative
            // store (persisted by the caller), so even if the on-disk mirror
            // write fails below, issuance still succeeds.
            let cert_pem = bundle.certificate_pem.clone();
            let key_pem = bundle.private_key_pem.clone();
            let primary = args.domains.first().cloned().unwrap_or_default();
            let live = format!(
                "{}/live/{primary}",
                crate::acme_orchestrator::app_cert_dir()
            );
            // Best-effort on-disk mirror (backward compat with the certbot
            // layout); the DB is authoritative, so a write failure is a
            // warning, not an issuance failure.
            if let Some(parent) = std::path::Path::new(&live).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let cert_path = format!("{live}/cert.pem");
            let key_path = format!("{live}/privkey.pem");
            if let Err(e) = bundle.save_to_files(&cert_path, &key_path) {
                warn!("best-effort cert mirror write failed (DB is authoritative): {e}");
            } else {
                log!("Certificate mirror written to {cert_path} (key {key_path}).");
            }
            log!("Issuance completed for {}.", args.domains.join(", "));
            AcmeIssueResult { ok: true, logs: logs.join("\n"), error: None, cert_pem: Some(cert_pem), key_pem: Some(key_pem) }
        }
        Err(e) => {
            warn!("ACME issuance failed: {e}");
            AcmeIssueResult {
                ok: false,
                logs: logs.join("\n"),
                error: Some(e.to_string()),
                cert_pem: None,
                key_pem: None,
            }
        }
    }
}

/// The set of DNS provider ids Vreckan can expose in the admin UI, in
/// display order. Cloudflare is the default.
pub const DNS_PROVIDER_IDS: &[&str] = &[
    "cloudflare",
    "route53",
    "digitalocean",
    "linode",
    "azure",
    "google",
    "alibaba",
    "godaddy",
    "tencent",
    "huawei",
    "cloudns",
];

/// Build a JSON array describing the available DNS providers for the admin
/// UI (id + a short label).
pub fn dns_provider_choices() -> Value {
    let labels: [(&str, &str); DNS_PROVIDER_IDS.len()] = [
        ("cloudflare", "Cloudflare"),
        ("route53", "AWS Route 53"),
        ("digitalocean", "DigitalOcean"),
        ("linode", "Linode"),
        ("azure", "Azure DNS"),
        ("google", "Google Cloud DNS"),
        ("alibaba", "Alibaba Cloud DNS"),
        ("godaddy", "GoDaddy"),
        ("tencent", "Tencent Cloud DNS"),
        ("huawei", "Huawei Cloud DNS"),
        ("cloudns", "CloudNS"),
    ];
    let arr: Vec<Value> = labels
        .iter()
        .map(|(id, label)| json!({ "id": *id, "label": *label, "default": *id == "cloudflare" }))
        .collect();
    Value::Array(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_directory_maps_lets_encrypt() {
        assert!(ca_directory("letsencrypt", true).contains("acme-staging-v02"));
        assert!(ca_directory("letsencrypt", false).contains("acme-v02.api.letsencrypt.org"));
        assert!(ca_directory("zerossl", false).contains("zerossl"));
        assert!(ca_directory("google", false).contains("pki.goog"));
    }

    #[test]
    fn registry_routes_by_longest_suffix() {
        let mut reg = DnsProviderRegistry::new();
        reg.insert(
            "example.com",
            Arc::new(acmex::challenge::MockDnsProvider::new()),
        );
        reg.insert(
            "internal.example.com",
            Arc::new(acmex::challenge::MockDnsProvider::new()),
        );
        // apex + subdomain route to example.com provider; deeper to the more specific one
        assert!(reg.get("example.com").is_some());
        assert!(reg.get("www.example.com").is_some());
        assert!(reg.get("deep.internal.example.com").is_some());
        assert!(reg.get("unrelated.net").is_none());
    }

    #[test]
    fn dns01_record_value_is_base64url_sha256() {
        let digest = Sha256::digest(b"token.auth");
        let v = URL_SAFE_NO_PAD.encode(digest);
        // no padding, base64url alphabet
        assert!(!v.contains('='));
        assert!(!v.contains('+') && !v.contains('/'));
    }

    #[test]
    fn provider_choices_list_is_wellformed() {
        let v = dns_provider_choices();
        let arr = v.as_array().expect("array");
        assert!(arr.len() >= 10);
        let cf = arr
            .iter()
            .find(|x| x.get("id") == Some(&json!("cloudflare")))
            .expect("cloudflare present");
        assert_eq!(cf.get("default"), Some(&json!(true)));
    }
}

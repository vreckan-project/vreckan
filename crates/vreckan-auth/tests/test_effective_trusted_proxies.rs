//! Effective `trusted_proxies` resolution (DB-over-env), ANT-879 finding 8a.
//!
//! The trusted-proxy gate used to read only the startup env snapshot
//! (`Settings.trusted_proxies`), so a value saved on the admin SSO page was
//! a silent no-op. `effective_trusted_proxies` re-resolves on every call so
//! a saved value takes effect on the next request without a restart.

mod common;

use common::{connect_db, test_settings, TempDir};
use vreckan_core::settings::Settings;

#[tokio::test]
async fn effective_trusted_proxies_db_over_env() {
    let root = TempDir::new();
    let d = connect_db(root.path()).await;
    let mut s: Settings = test_settings(root.path());
    s.trusted_proxies = "10.0.0.0/8".to_string();

    // No app_settings row: the startup env/default value is used.
    assert_eq!(s.effective_trusted_proxies(&d).await, "10.0.0.0/8");

    // A value saved on the admin SSO page overrides the env snapshot.
    d.set_app_setting("VRECKAN_TRUSTED_PROXIES", "192.168.1.0/24, 172.16.0.1")
        .await
        .unwrap();
    assert_eq!(
        s.effective_trusted_proxies(&d).await,
        "192.168.1.0/24, 172.16.0.1"
    );

    // A present-but-empty value shadows the env snapshot (trust no proxy),
    // mirroring `cors_config`'s precedence.
    d.set_app_setting("VRECKAN_TRUSTED_PROXIES", "").await.unwrap();
    assert_eq!(s.effective_trusted_proxies(&d).await, "");
}

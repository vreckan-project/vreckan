#!/usr/bin/env python3
"""Start the Vreckan API over HTTPS on 0.0.0.0:$VRECKAN_API_PORT (default 443).

Intended to run inside Docker with the real authentik OIDC env (VRECKAN_OIDC_*).
It will:
  1. Pick the TLS cert. When Let's Encrypt is enabled (VRECKAN_LE_* env or the
     admin Certificates page), a real cert is issued/renewed via a transient
     certbot container (server/le_certbot.py) and read from the shared
     vreckan-le volume (/data/le) or a host-mounted /etc/letsencrypt.
     Otherwise a generated self-signed cert (CN=localhost) is used from
     $WORK/ssl.
  2. Generate the required server RSA key (api.py import requirement).
  3. uvicorn-serve server.api:api_app on 0.0.0.0:$VRECKAN_API_PORT with TLS.

The OIDC redirect URI is derived per-request from the Host header, so SSO
works on any access path the provider has registered.
"""
import datetime
import ipaddress
import os
import sys

# --- env sanity ----------------------------------------------------------------
work = os.environ.get("WORK", "/data")
ssl_dir = os.path.join(work, "ssl")
os.makedirs(work, exist_ok=True)
# All durable configuration lives in the database (see VRECKAN_DATABASE_URL);
# the legacy YAML config files (installed_apps.yml, app_stores.yml,
# sessions.yml, public_shares.yml, Caddyfile, ...) are dead, and so are the
# old file-based user stores (keys/, groups/, auth_tokens.yml) — users, groups,
# passwords and web-login tokens all live in the database now. The only
# file-based material is user data (storage/), TLS material (ssl/), the
# app-store + autostart caches, and backups.
for sub in ("storage", "ssl", "autostart_cache", "app_stores_cache", "backups"):
    os.makedirs(os.path.join(work, sub), exist_ok=True)
for sub in ("vreckan_app_icons", "vreckan_home_templates", "vreckan_public", "vreckan_uploads"):
    os.makedirs(os.path.join(work, "storage", sub), exist_ok=True)

# Required before importing app.api
os.environ.setdefault("VRECKAN_SERVER_PRIVATE_KEY_PATH", os.path.join(ssl_dir, "server_key.pem"))
os.environ.setdefault("VRECKAN_STORAGE_PATH", os.path.join(work, "storage"))
os.environ.setdefault("VRECKAN_AUTO_UPDATE_APPS", "false")
# App-config caches — unified onto the /data tree (previously /config/.config/vreckan/).
os.environ.setdefault("VRECKAN_AUTOSTART_CACHE_PATH", os.path.join(work, "autostart_cache"))
os.environ.setdefault("VRECKAN_APP_STORE_CACHE_PATH", os.path.join(work, "app_stores_cache"))
# Storage sub-paths — unified onto /data/storage (previously the old /storage/ root).
os.environ.setdefault("VRECKAN_APP_ICONS_PATH", os.path.join(work, "storage", "vreckan_app_icons"))
os.environ.setdefault("VRECKAN_HOME_TEMPLATES_PATH", os.path.join(work, "storage", "vreckan_home_templates"))
os.environ.setdefault("VRECKAN_PUBLIC_STORAGE_PATH", os.path.join(work, "storage", "vreckan_public"))
os.environ.setdefault("VRECKAN_UPLOAD_DIR", os.path.join(work, "storage", "vreckan_uploads"))
# Backup / restore — the whole persistent tree lives under WORK (/data).
os.environ.setdefault("VRECKAN_DATA_ROOT", work)
os.environ.setdefault("VRECKAN_BACKUPS_PATH", os.path.join(work, "backups"))
# Configuration database — all durable config (users, groups, apps, sessions,
# tokens, shares, mounts, stores, templates) lives here instead of YAML files.
os.environ.setdefault("VRECKAN_DATABASE_URL", f"sqlite+aiosqlite:///{os.path.join(work, 'vreckan.db')}")

selfsigned_cert = os.path.join(ssl_dir, "vreckan_dev.crt")
selfsigned_key = os.path.join(ssl_dir, "vreckan_dev.key")
server_key_path = os.environ["VRECKAN_SERVER_PRIVATE_KEY_PATH"]

# --- 1) TLS cert: Let's Encrypt (via certbot) or self-signed fallback ----------
# When Let's Encrypt is enabled (VRECKAN_LE_* env or the admin Certificates
# page), a real cert is issued/renewed via a transient certbot container (see
# server/le_certbot.py). The cert lives in the shared vreckan-le volume
# (/data/le) or a host-mounted /etc/letsencrypt. Otherwise a self-signed cert
# is used. The selection happens in _select_cert() below.

def _ensure_cert():
    if os.path.exists(selfsigned_cert) and os.path.exists(selfsigned_key):
        print(f"[cert] reusing existing {selfsigned_cert}", flush=True)
        return
    from cryptography import x509
    from cryptography.x509.oid import NameOID
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import rsa

    cn = "localhost"
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    name = x509.Name([
        x509.NameAttribute(NameOID.COMMON_NAME, cn),
        x509.NameAttribute(NameOID.ORGANIZATION_NAME, "Vreckan Dev"),
    ])
    now = datetime.datetime.now(datetime.timezone.utc)
    san = x509.SubjectAlternativeName([
        x509.DNSName("localhost"),
        x509.IPAddress(ipaddress.ip_address("127.0.0.1")),
        x509.IPAddress(ipaddress.ip_address("::1")),
    ])
    cert = (
        x509.CertificateBuilder()
        .subject_name(name).issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(days=1))
        .not_valid_after(now + datetime.timedelta(days=825))
        .add_extension(san, critical=False)
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .add_extension(x509.KeyUsage(
            digital_signature=True, key_encipherment=True, key_cert_sign=True,
            crl_sign=True, content_commitment=False, data_encipherment=False,
            key_agreement=False, encipher_only=False, decipher_only=False), critical=True)
        .add_extension(x509.ExtendedKeyUsage([
            x509.oid.ExtendedKeyUsageOID.SERVER_AUTH,
            x509.oid.ExtendedKeyUsageOID.CLIENT_AUTH]), critical=False)
        .sign(key, hashes.SHA256())
    )
    with open(selfsigned_cert, "wb") as f:
        f.write(cert.public_bytes(serialization.Encoding.PEM))
    with open(selfsigned_key, "wb") as f:
        f.write(key.private_bytes(
            serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
            encryption_algorithm=serialization.NoEncryption()))
    print(f"[cert] wrote {selfsigned_cert} + {selfsigned_key}", flush=True)

# --- 2) server RSA key (required at api.py import) ------------------------------
def _ensure_server_key():
    if os.path.exists(server_key_path):
        print(f"[key] reusing existing {server_key_path}", flush=True)
        return
    from cryptography.hazmat.primitives.asymmetric import rsa
    from cryptography.hazmat.primitives import serialization
    k = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    with open(server_key_path, "wb") as f:
        f.write(k.private_bytes(
            serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
            encryption_algorithm=serialization.NoEncryption()))
    print(f"[key] wrote {server_key_path}", flush=True)

def _select_cert():
    """Pick the TLS cert: a Let's Encrypt cert (issued via certbot when LE is
    enabled, or a host-mounted one) when available, else a self-signed cert.
    Returns (cert_path, key_path)."""
    import asyncio
    from server import db
    from server import le_certbot

    # Make sure the configuration DB is up so effective (DB-overridden) LE
    # settings can be read. Idempotent; a failure just means we fall back to
    # the env/default values.
    try:
        asyncio.run(db.init_db())
    except Exception as e:  # noqa: BLE001
        print(f"[cert] DB init for LE settings failed ({e}); using env defaults", flush=True)

    cfg = asyncio.run(le_certbot.effective_le_config())
    if cfg.le_enabled:
        status = asyncio.run(le_certbot.ensure_le_cert(cfg))
        if status.get("present") and status.get("valid"):
            print(
                f"[cert] using Let's Encrypt cert {status['cert']} "
                f"(source={status['source']}, {status['days_remaining']}d left)",
                flush=True,
            )
            return status["cert"], status["key"]
        print(
            f"[cert] LE enabled but no valid cert yet (issued={status.get('issued')}); "
            "using self-signed until one is available",
            flush=True,
        )
    else:
        print("[cert] LE not enabled; using self-signed certificate", flush=True)
    _ensure_cert()
    return selfsigned_cert, selfsigned_key


cert_path, key_path = _select_cert()
_ensure_server_key()

# --- 3) uvicorn TLS on 0.0.0.0:$VRECKAN_API_PORT --------------------------------
port = int(os.environ.get("VRECKAN_API_PORT", "443"))
print(f"[start] uvicorn server.api:api_app on 0.0.0.0:{port} (TLS)", flush=True)
import uvicorn
uvicorn.run(
    "server.api:api_app",
    host="0.0.0.0",
    port=port,
    ssl_keyfile=key_path,
    ssl_certfile=cert_path,
    log_level="info",
)

"""Tests for the Let's Encrypt / certbot orchestration (``server/le_certbot.py``)
and the admin Certificates endpoints.

The Docker SDK is mocked (a fake client/container) so the tests never need a
Docker daemon or the certbot image. Certificate validity is exercised with
real PEM certs generated via ``cryptography`` into a temp tree that stands in
for the shared ``vreckan-le`` volume.

Run the whole suite against either backend; these tests are backend-agnostic
(the admin-endpoint tests use the shared ``secure_client`` fixture).
"""
import asyncio
import os
from datetime import datetime, timedelta, timezone

import pytest
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

from server import le_certbot


# --- helpers -----------------------------------------------------------------
def _make_cert(cn="example.com", days=90):
    """Generate a self-signed cert + key; return (cert_pem_bytes, key_pem_bytes).

    ``days`` is the lifetime from now; a negative value yields an already
    expired cert (not_valid_after in the past, not_valid_before even earlier).
    """
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, cn)])
    now = datetime.now(timezone.utc)
    not_after = now + timedelta(days=days)
    not_before = not_after - timedelta(days=90)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(not_before)
        .not_valid_after(not_after)
        .sign(key, hashes.SHA256())
    )
    cert_pem = cert.public_bytes(serialization.Encoding.PEM)
    key_pem = key.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.PKCS8,
        serialization.NoEncryption(),
    )
    return cert_pem, key_pem


def _write_cert_tree(base, domain, days=90):
    """Write a cert+key into ``base/live/<domain>/`` (certbot's layout)."""
    live = os.path.join(base, "live", domain)
    os.makedirs(live, exist_ok=True)
    cert_pem, key_pem = _make_cert(domain, days)
    with open(os.path.join(live, "cert.pem"), "wb") as f:
        f.write(cert_pem)
    with open(os.path.join(live, "privkey.pem"), "wb") as f:
        f.write(key_pem)
    return live


class FakeContainer:
    def __init__(self, exit_code=0, logs=b"ok"):
        self.exit_code = exit_code
        self._logs = logs
        self.removed = False
        self.run_args = None
        self.run_kwargs = None

    def wait(self, timeout=None):
        return {"StatusCode": self.exit_code}

    def logs(self):
        return self._logs

    def remove(self, force=False):
        self.removed = True


class FakeContainers:
    def __init__(self, container, mounts=None):
        self.container = container
        self.run_calls = []
        self._mounts = mounts

    def run(self, image, args, **kwargs):
        self.container.run_args = (image, args)
        self.container.run_kwargs = kwargs
        self.run_calls.append((image, args, kwargs))
        return self.container

    def get(self, name):
        # Models the app container (resolved by hostname) so the /data/le
        # mount auto-detection has something to read. Raises when no mounts
        # were configured, exercising the named-volume fallback.
        if self._mounts is None:
            raise Exception("no such container")
        return type("FakeAppContainer", (), {"attrs": {"Mounts": self._mounts}})


class FakeClient:
    def __init__(self, exit_code=0, logs=b"ok", mounts=None):
        self.containers = FakeContainers(FakeContainer(exit_code, logs), mounts=mounts)


def _cfg(**overrides):
    """A settings-like object with the LE fields the module reads."""
    class _Cfg:
        le_enabled = True
        le_domains = "example.com,www.example.com"
        le_auth = "dns-cloudflare"
        le_staging = False
        le_cloudflare_email = "admin@example.com"
        le_cloudflare_token = "cf-token-123"
        le_auto_renew = True
        le_renew_check_hours = 6
        le_renew_before_days = 30

    for k, v in overrides.items():
        setattr(_Cfg, k, v)
    return _Cfg()


@pytest.fixture
def le_paths(tmp_path, monkeypatch):
    """Point the module's cert dirs at a temp tree (host + volume)."""
    host = tmp_path / "host-le"
    volume = tmp_path / "volume-le"
    host.mkdir()
    volume.mkdir()
    monkeypatch.setattr(le_certbot, "LE_HOST_CERT_DIR", str(host))
    monkeypatch.setattr(le_certbot, "LE_APP_CERT_DIR", str(volume))
    return {"host": str(host), "volume": str(volume)}


# --- certbot_args ------------------------------------------------------------
def test_certbot_args_dns_cloudflare():
    args = le_certbot.certbot_args("dns-cloudflare", ["a.com", "b.com"], staging=False)
    assert args[0] == "certonly"
    assert "--non-interactive" in args
    assert "--agree-tos" in args
    assert "--register-unsafely-without-email" in args
    assert "--staging" not in args
    assert "--dns-cloudflare" in args
    assert f"--dns-cloudflare-credentials={le_certbot.CERTBOT_CREDENTIALS}" in args
    assert "-d" in args and "a.com" in args and "b.com" in args


def test_certbot_args_dns_cloudflare_staging():
    args = le_certbot.certbot_args("dns-cloudflare", ["a.com"], staging=True)
    assert "--staging" in args


def test_certbot_args_http01():
    args = le_certbot.certbot_args("http-01", ["a.com"], staging=False)
    assert "--http-01" in args
    assert f"--webroot-root={le_certbot.CERTBOT_CERT_DIR}" in args
    assert "--dns-cloudflare" not in args


def test_certbot_args_renew():
    args = le_certbot.certbot_args("dns-cloudflare", ["a.com"], staging=False, renew=True)
    assert args[0] == "renew"


# --- needs_renewal -----------------------------------------------------------
def test_needs_renewal():
    assert le_certbot.needs_renewal(10, 30) is True
    assert le_certbot.needs_renewal(30, 30) is True
    assert le_certbot.needs_renewal(31, 30) is False
    assert le_certbot.needs_renewal(-5, 30) is True  # already expired
    assert le_certbot.needs_renewal(None, 30) is False  # no cert


# --- cert_status / cert_expiry -----------------------------------------------
def test_cert_status_absent(le_paths):
    status = le_certbot.cert_status(["example.com"])
    assert status["present"] is False
    assert status["valid"] is False
    assert status["days_remaining"] is None


def test_cert_status_present_valid(le_paths):
    _write_cert_tree(le_paths["volume"], "example.com", days=90)
    status = le_certbot.cert_status(["example.com"])
    assert status["present"] is True
    assert status["source"] == "volume"
    assert status["valid"] is True
    assert status["days_remaining"] is not None
    assert 80 <= status["days_remaining"] <= 90
    assert status["expiry"] is not None


def test_cert_status_expired(le_paths):
    _write_cert_tree(le_paths["volume"], "example.com", days=-5)
    status = le_certbot.cert_status(["example.com"])
    assert status["present"] is True
    assert status["valid"] is False
    assert status["days_remaining"] < 0


def test_cert_status_host_preferred(le_paths):
    # A cert in both the host and volume paths: the host one wins.
    _write_cert_tree(le_paths["host"], "example.com", days=90)
    _write_cert_tree(le_paths["volume"], "example.com", days=90)
    status = le_certbot.cert_status(["example.com"])
    assert status["source"] == "host"
    assert status["cert"].startswith(le_paths["host"])


def test_cert_expiry_parses(le_paths):
    _write_cert_tree(le_paths["volume"], "example.com", days=90)
    path = os.path.join(le_paths["volume"], "live", "example.com", "cert.pem")
    exp = le_certbot.cert_expiry(path)
    assert exp is not None
    assert exp.tzinfo is not None
    assert 80 <= (exp - datetime.now(timezone.utc)).days <= 90


def test_cert_expiry_missing_file(le_paths):
    assert le_certbot.cert_expiry(os.path.join(le_paths["volume"], "nope.pem")) is None


# --- run_certbot (mocked Docker) ---------------------------------------------
def test_run_certbot_success(le_paths):
    client = FakeClient(exit_code=0, logs=b"Certificate issued.")
    result = asyncio.run(
        le_certbot.run_certbot(
            le_certbot.certbot_args("dns-cloudflare", ["a.com"], False), client=client
        )
    )
    assert result["ok"] is True
    assert result["exit_code"] == 0
    assert "Certificate issued." in result["logs"]
    # The container was removed afterwards.
    assert client.containers.container.removed is True
    # The volume was mounted at certbot's default path.
    _, _, kwargs = client.containers.run_calls[0]
    assert kwargs["volumes"] == {le_certbot.LE_VOLUME: {"bind": le_certbot.CERTBOT_CERT_DIR, "mode": "rw"}}


def test_run_certbot_mounts_app_data_le_bind(le_paths):
    # When the app's /data is a host bind mount, certbot must mount the
    # matching host sub-path (/data/le) so it shares the app's storage.
    client = FakeClient(
        exit_code=0,
        mounts=[{"Type": "bind", "Source": "/srv/vreckan-data", "Destination": "/data"}],
    )
    asyncio.run(
        le_certbot.run_certbot(
            le_certbot.certbot_args("dns-cloudflare", ["a.com"], False), client=client
        )
    )
    _, _, kwargs = client.containers.run_calls[0]
    assert kwargs["volumes"] == {
        "/srv/vreckan-data/le": {"bind": le_certbot.CERTBOT_CERT_DIR, "mode": "rw"}
    }


def test_run_certbot_mounts_app_data_le_volume(le_paths):
    # When the app's /data is a named volume, certbot mounts that same volume.
    client = FakeClient(
        exit_code=0,
        mounts=[{"Type": "volume", "Name": "vreckan-data", "Destination": "/data"}],
    )
    asyncio.run(
        le_certbot.run_certbot(
            le_certbot.certbot_args("dns-cloudflare", ["a.com"], False), client=client
        )
    )
    _, _, kwargs = client.containers.run_calls[0]
    assert kwargs["volumes"] == {
        "vreckan-data": {"bind": le_certbot.CERTBOT_CERT_DIR, "mode": "rw"}
    }


def test_run_certbot_failure(le_paths):
    client = FakeClient(exit_code=1, logs=b"DNS challenge failed")
    result = asyncio.run(
        le_certbot.run_certbot(
            le_certbot.certbot_args("dns-cloudflare", ["a.com"], False), client=client
        )
    )
    assert result["ok"] is False
    assert result["exit_code"] == 1
    assert "DNS challenge failed" in result["logs"]
    assert client.containers.container.removed is True


def test_run_certbot_writes_credentials(le_paths):
    path = le_certbot.write_cloudflare_credentials("admin@example.com", "cf-token-123")
    assert path == os.path.join(le_paths["volume"], "cloudflare.ini")
    with open(path) as f:
        content = f.read()
    # The plugin expects the dns_cloudflare_ key prefix. With a token set,
    # the email line is omitted (the plugin rejects a file with both). No
    # section header: the plugin looks the keys up at the top level.
    assert "dns_cloudflare_api_token = cf-token-123" in content
    assert "dns_cloudflare_email" not in content
    assert "[dns_cloudflare]" not in content


def test_run_certbot_writes_credentials_email_only(le_paths):
    # Without a token, the email line is written (legacy email + API-key
    # method).
    path = le_certbot.write_cloudflare_credentials("admin@example.com", "")
    with open(path) as f:
        content = f.read()
    assert "dns_cloudflare_email = admin@example.com" in content
    assert "dns_cloudflare_api_token" not in content


# --- ensure_le_cert / renew_le_cert (mocked Docker) --------------------------
def test_ensure_le_cert_disabled(le_paths):
    result = asyncio.run(le_certbot.ensure_le_cert(_cfg(le_enabled=False), client=FakeClient()))
    assert result["enabled"] is False
    assert result["issued"] is False


def test_ensure_le_cert_no_domains(le_paths):
    result = asyncio.run(le_certbot.ensure_le_cert(_cfg(le_domains=""), client=FakeClient()))
    assert result["enabled"] is True
    assert result["issued"] is False


def test_ensure_le_cert_issues_when_missing(le_paths):
    client = FakeClient(exit_code=0)
    result = asyncio.run(le_certbot.ensure_le_cert(_cfg(), client=client))
    assert result["enabled"] is True
    assert result["issued"] is True
    # certonly was used (not renew) for a fresh issue.
    assert client.containers.run_calls[0][1][0] == "certonly"


def test_ensure_le_cert_skips_when_valid(le_paths):
    _write_cert_tree(le_paths["volume"], "example.com", days=90)
    client = FakeClient(exit_code=0)
    result = asyncio.run(le_certbot.ensure_le_cert(_cfg(), client=client))
    assert result["issued"] is False
    # No certbot run was needed (the cert is still valid).
    assert client.containers.run_calls == []


def test_renew_le_cert_not_due(le_paths):
    _write_cert_tree(le_paths["volume"], "example.com", days=90)
    client = FakeClient(exit_code=0)
    result = asyncio.run(le_certbot.renew_le_cert(_cfg(le_renew_before_days=30), client=client))
    assert result["due"] is False
    assert result["renewed"] is False
    assert client.containers.run_calls == []


def test_renew_le_cert_due_and_renews(le_paths):
    _write_cert_tree(le_paths["volume"], "example.com", days=10)
    client = FakeClient(exit_code=0)
    result = asyncio.run(le_certbot.renew_le_cert(_cfg(le_renew_before_days=30), client=client))
    assert result["due"] is True
    assert result["renewed"] is True
    # renew was used (not certonly).
    assert client.containers.run_calls[0][1][0] == "renew"


def test_renew_le_cert_disabled(le_paths):
    client = FakeClient(exit_code=0)
    result = asyncio.run(le_certbot.renew_le_cert(_cfg(le_enabled=False), client=client))
    assert result["due"] is False
    assert client.containers.run_calls == []


# --- effective_le_config -----------------------------------------------------
def test_effective_le_config_reads_env_defaults(isolate, monkeypatch):
    # With no DB rows and no env, the defaults apply.
    from server import db

    asyncio.run(db.init_db())
    monkeypatch.delenv("VRECKAN_LE_ENABLED", raising=False)
    monkeypatch.delenv("VRECKAN_LE_DOMAINS", raising=False)
    cfg = asyncio.run(le_certbot.effective_le_config())
    assert cfg.le_enabled is False  # default
    assert cfg.le_auth == "dns-cloudflare"
    assert cfg.le_renew_before_days == 30


def test_effective_le_config_env_override(isolate, monkeypatch):
    from server import db
    from server.settings import settings

    asyncio.run(db.init_db())
    # get_setting consults the settings instance attribute (populated from the
    # environment at import), so monkeypatch it to simulate an env override.
    monkeypatch.setattr(settings, "le_enabled", True)
    monkeypatch.setattr(settings, "le_domains", "a.com,b.com")
    cfg = asyncio.run(le_certbot.effective_le_config())
    assert cfg.le_enabled is True
    assert cfg.le_domains == "a.com,b.com"


# --- admin Certificates endpoints --------------------------------------------
def test_certificates_get_default(secure_client):
    status, data = secure_client.call("GET", "/api/admin/certificates")
    assert status == 200
    assert data["values"]["le_enabled"] is False
    assert data["values"]["le_auth"] == "dns-cloudflare"
    assert "le_domains" in data["values"]
    assert "status" in data
    assert data["status"]["present"] is False
    # The token is masked, not echoed.
    assert data["values"]["le_cloudflare_token"] == ""


def test_certificates_put_and_get(secure_client):
    payload = {
        "le_enabled": True,
        "le_domains": ["example.com", "www.example.com"],
        "le_auth": "dns-cloudflare",
        "le_cloudflare_email": "admin@example.com",
        "le_cloudflare_token": "cf-token-123",
        "le_staging": True,
        "le_auto_renew": True,
        "le_renew_check_hours": 12,
        "le_renew_before_days": 20,
    }
    status, _ = secure_client.call("PUT", "/api/admin/certificates", body=payload)
    assert status == 200

    status, data = secure_client.call("GET", "/api/admin/certificates")
    assert status == 200
    v = data["values"]
    assert v["le_enabled"] is True
    assert v["le_domains"] == ["example.com", "www.example.com"]
    assert v["le_staging"] is True
    assert v["le_renew_check_hours"] == 12
    assert v["le_renew_before_days"] == 20
    # The token is masked but reported as set.
    assert v["le_cloudflare_token"] == ""
    assert v["token_set"] is True
    assert v["token_length"] == len("cf-token-123")
    # The value now comes from the DB (edited in the UI).
    assert data["sources"]["le_enabled"] == "db"


def test_certificates_put_blank_token_keeps_existing(secure_client):
    # First set a token.
    secure_client.call(
        "PUT",
        "/api/admin/certificates",
        body={
            "le_enabled": True,
            "le_domains": ["example.com"],
            "le_auth": "dns-cloudflare",
            "le_cloudflare_email": "admin@example.com",
            "le_cloudflare_token": "original-token",
            "le_staging": False,
            "le_auto_renew": True,
            "le_renew_check_hours": 6,
            "le_renew_before_days": 30,
        },
    )
    # Then save again with a blank token — the existing one should be kept.
    secure_client.call(
        "PUT",
        "/api/admin/certificates",
        body={
            "le_enabled": True,
            "le_domains": ["example.com"],
            "le_auth": "dns-cloudflare",
            "le_cloudflare_email": "admin@example.com",
            "le_cloudflare_token": "",
            "le_staging": False,
            "le_auto_renew": True,
            "le_renew_check_hours": 6,
            "le_renew_before_days": 30,
        },
    )
    status, data = secure_client.call("GET", "/api/admin/certificates")
    assert status == 200
    assert data["values"]["token_set"] is True
    assert data["values"]["token_length"] == len("original-token")


def test_certificates_renew_disabled(secure_client):
    status, data = secure_client.call("POST", "/api/admin/certificates/renew")
    assert status == 400  # LE not enabled by default


def test_certificates_renew_no_domains(secure_client, monkeypatch):
    # Enable LE but leave domains empty.
    secure_client.call(
        "PUT",
        "/api/admin/certificates",
        body={
            "le_enabled": True,
            "le_domains": [],
            "le_auth": "dns-cloudflare",
            "le_cloudflare_email": "admin@example.com",
            "le_cloudflare_token": "t",
            "le_staging": False,
            "le_auto_renew": True,
            "le_renew_check_hours": 6,
            "le_renew_before_days": 30,
        },
    )
    status, data = secure_client.call("POST", "/api/admin/certificates/renew")
    assert status == 400


def test_certificates_renew_issues(secure_client, monkeypatch, le_paths):
    # Enable LE with a domain, then renew (no valid cert yet -> issues one).
    secure_client.call(
        "PUT",
        "/api/admin/certificates",
        body={
            "le_enabled": True,
            "le_domains": ["example.com"],
            "le_auth": "dns-cloudflare",
            "le_cloudflare_email": "admin@example.com",
            "le_cloudflare_token": "t",
            "le_staging": False,
            "le_auto_renew": True,
            "le_renew_check_hours": 6,
            "le_renew_before_days": 30,
        },
    )
    # Point the module's cert dirs at the temp tree and mock the Docker client.
    monkeypatch.setattr(le_certbot, "LE_HOST_CERT_DIR", le_paths["host"])
    monkeypatch.setattr(le_certbot, "LE_APP_CERT_DIR", le_paths["volume"])
    client = FakeClient(exit_code=0)
    original_run = le_certbot.run_certbot

    async def _run_with_client(args, **kw):
        return await original_run(args, client=client)

    monkeypatch.setattr(le_certbot, "run_certbot", _run_with_client)

    status, data = secure_client.call("POST", "/api/admin/certificates/renew")
    assert status == 200
    assert data["issued"] is True
    assert client.containers.run_calls[0][1][0] == "certonly"

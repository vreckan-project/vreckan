"""Tier 5: real Docker integration tests.

Every other tier monkeypatches ``DockerProvider`` with a fake so the suite
stays hermetic and fast. This tier does the opposite: it lets the *real*
provider launch and stop actual containers on the local Docker daemon,
exercising the full launch -> ready -> stop lifecycle over the compose
network (the same bridge network the API container sits on, so the API can
reach the launched container's IP directly).

The probe app is backed by ``python:3.12-slim`` (already cached in the test
image, so no network pull is needed) running a catch-all HTTP server, so both
the launch readiness probe (SUBFOLDER path + Basic auth) and the session
status endpoint (root path, no auth) get a 200. The app declares no GPU
support, so no device passthrough is attempted and the tests run on any host.

These tests are skipped (not failed) when no Docker daemon is reachable, so
the suite remains runnable on machines without Docker.
"""
import asyncio

import pytest

try:
    import docker
    _DOCKER_AVAILABLE = bool(docker.from_env().ping())
except Exception:  # pragma: no cover - no daemon
    docker = None
    _DOCKER_AVAILABLE = False

requires_docker = pytest.mark.skipif(
    not _DOCKER_AVAILABLE, reason="Docker daemon not reachable"
)

import server.api as api_module  # noqa: E402
from server.models import InstalledApp, InstalledAppProviderConfig  # noqa: E402
from conftest import _REAL_FROM_ENV  # noqa: E402

APP_ID = "app-docker-probe"

# Catch-all HTTP server: 200 for any path/method, silent. Satisfies both the
# launch readiness probe and the session status endpoint.
_CATCH_ALL = (
    "from http.server import BaseHTTPRequestHandler, HTTPServer\n"
    "class H(BaseHTTPRequestHandler):\n"
    "    def _ok(self):\n"
    "        self.send_response(200)\n"
    "        self.send_header('Content-Type', 'text/plain')\n"
    "        self.end_headers()\n"
    "        self.wfile.write(b'ok')\n"
    "    do_GET = do_POST = do_PUT = do_DELETE = _ok\n"
    "    def log_message(self, *a): pass\n"
    "HTTPServer(('0.0.0.0', 8080), H).serve_forever()\n"
)


def _seed_probe_app() -> str:
    """Register a no-GPU probe app backed by python:3.12-slim."""
    app = InstalledApp(
        id=APP_ID,
        name="Docker Probe",
        logo="x",
        url="x",
        source="Test Store",
        source_app_id=APP_ID,
        provider="docker",
        # home_directories=False -> cleanroom/ephemeral: no home-dir dependency
        home_directories=False,
        users=["*"],
        groups=[],
        app_template="Default",
        provider_config=InstalledAppProviderConfig(
            image="python:3.12-slim",
            port=8080,
            nvidia_support=False,
            dri3_support=False,
            type="docker",
            url_support=False,
            open_support=False,
            extensions=[],
            docker_overrides={"command": ["python", "-c", _CATCH_ALL]},
        ),
    )
    api_module.INSTALLED_APPS[APP_ID] = app
    return APP_ID


def _force_cleanup(instance_ids):
    """Safety net: stop+remove any container a test launched but didn't stop.

    A failed launch (504) already self-cleans inside the provider, so this
    only matters when a test fails *after* a successful launch.
    """
    if not _DOCKER_AVAILABLE:
        return
    try:
        client = docker.from_env()
    except Exception:
        return
    for cid in instance_ids:
        if not cid:
            continue
        try:
            c = client.containers.get(cid)
            if c.status == "running":
                c.stop(timeout=2)
            c.remove()
        except Exception:
            pass


@pytest.fixture
def real_docker(isolate, monkeypatch):
    """Restore the real ``docker.from_env`` for the duration of a test.

    The autouse ``isolate`` fixture patches ``docker.from_env`` to a hermetic
    fake so the rest of the suite never touches the daemon; this tier needs the
    real client to launch and inspect actual containers.

    The lifespan's self-inspection (which discovers ``DISCOVERED_NETWORK``) ran
    while ``docker.from_env`` was still the fake, so it found no self-container
    and left ``DISCOVERED_NETWORK`` as ``None``. Re-run it now that the real
    client is restored so launched containers land on the same network as the
    test container (and are therefore reachable by the readiness probe).
    """
    monkeypatch.setattr(docker, "from_env", _REAL_FROM_ENV)
    try:
        asyncio.run(api_module._inspect_self_container())
    except Exception:
        pass


@pytest.fixture
def probe_app(secure_client):
    """Seed the probe app *after* the lifespan ran (so it is not wiped)."""
    yield _seed_probe_app()


@requires_docker
def test_launch_stop_lifecycle(secure_client, probe_app, real_docker):
    """Launch a real container, confirm it is running + ready, then stop it."""
    launched = []
    try:
        status, body = secure_client.call(
            "POST", "/api/launch/simple", {"application_id": probe_app}
        )
        assert status == 200, body
        session_id = body["session_id"]
        assert body["session_url"].startswith(f"/api/apps/session/{session_id}/")

        # The session is recorded in the in-memory DB.
        session = api_module.SESSIONS_DB[session_id]
        assert session["username"] == "admin"
        assert session["provider_app_id"] == probe_app
        instance_id = session["instance_id"]
        launched.append(instance_id)

        # A real container is running on the daemon.
        dclient = docker.from_env()
        container = dclient.containers.get(instance_id)
        assert container.status == "running"

        # The env layering reached the real container.
        env = container.attrs["Config"].get("Env") or []
        assert any(
            e.startswith(f"SUBFOLDER=/api/apps/session/{session_id}/") for e in env
        )

        # The status endpoint agrees the upstream is ready.
        status2, status_body = secure_client.call(
            "GET", f"/api/sessions/{session_id}/status"
        )
        assert status2 == 200
        assert status_body["ready"] is True

        # Stop: 204, session gone, container removed.
        status3, _ = secure_client.call("DELETE", f"/api/sessions/{session_id}")
        assert status3 == 204
        assert session_id not in api_module.SESSIONS_DB
        with pytest.raises(docker.errors.NotFound):
            dclient.containers.get(instance_id)
    finally:
        _force_cleanup(launched)


@requires_docker
def test_no_gpu_app_gets_no_device_passthrough(secure_client, probe_app, real_docker):
    """A no-GPU app must launch without any /dev device passthrough."""
    launched = []
    try:
        status, body = secure_client.call(
            "POST", "/api/launch/simple", {"application_id": probe_app}
        )
        assert status == 200, body
        session_id = body["session_id"]
        instance_id = api_module.SESSIONS_DB[session_id]["instance_id"]
        launched.append(instance_id)

        dclient = docker.from_env()
        container = dclient.containers.get(instance_id)
        devices = container.attrs["HostConfig"].get("Devices") or []
        assert devices == [], f"unexpected device passthrough: {devices}"
        env = container.attrs["Config"].get("Env") or []
        assert not any(e.startswith("DRI_NODE=") for e in env)
    finally:
        _force_cleanup(launched)

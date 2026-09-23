"""Launch spec assembly: env layering, volumes, GPUs, room mode, access.

These tests drive ``_launch_common`` directly (no TestClient) with a fake
provider, so they run on a plain ``asyncio.run`` loop and assert on the
exact kwargs the Docker provider would have received.
"""
import asyncio
import copy
import os

import pytest
from fastapi import HTTPException

import server.api as api_module
from server import user_manager
from server.models import InstalledApp

DEFAULT_PROVIDER_CONFIG = {
    "image": "img:latest",
    "port": 3000,
    "nvidia_support": False,
    "dri3_support": True,
    "type": "browser",
    "url_support": True,
    "open_support": False,
    "extensions": [],
    "env": [{"name": "APP_ENV", "value": "1"}],
    "docker_overrides": {"devices": ["/dev/snd"]},
}


class FakeProvider:
    """Stands in for DockerProvider: records the launch kwargs."""

    last = None

    def __init__(self, app_config_dict):
        self.app_config_dict = app_config_dict

    async def launch(self, **kwargs):
        self.launch_kwargs = kwargs
        FakeProvider.last = self
        return {"instance_id": "ctr-1", "ip": "10.0.0.5", "port": 3000}

    async def stop(self, instance_id):
        pass


def make_app(**overrides):
    data = {
        "id": "app-1",
        "name": "Firefox",
        "logo": "x",
        "url": "x",
        "source": "Test Store",
        "source_app_id": "firefox",
        "provider": "docker",
        "home_directories": True,
        "users": ["all"],
        "groups": [],
        "app_template": "Default",
        "provider_config": copy.deepcopy(DEFAULT_PROVIDER_CONFIG),
    }
    data.update(overrides)
    app = InstalledApp(**data)
    api_module.INSTALLED_APPS[app.id] = app
    return app


def seed_admin(**settings_overrides):
    s = dict(user_manager.DEFAULT_USER_SETTINGS)
    s.update(settings_overrides)
    # These tests bypass the secure_client fixture, so load_roles() never runs
    # and ROLE_DATA would be empty — the 'admin' role could not expand to the
    # permission catalog and capabilities (persistent_storage, gpu) would all
    # resolve False. Seed the built-in roles so permission resolution works.
    if not user_manager.ROLE_DATA:
        for r in user_manager.BUILTIN_ROLES:
            user_manager.ROLE_DATA[r["name"]] = dict(r)
    user_manager.USER_DATA["admin"] = {
        "username": "admin",
        "is_admin": True,
        "is_sso": False,
        "roles": ["admin"],
        "permissions": [],
        "groups": [],
        "sso_groups": [],
        "settings": s,
        "password_hash": None,
    }
    return s


@pytest.fixture
def fake_provider(monkeypatch):
    FakeProvider.last = None
    monkeypatch.setattr(api_module, "DockerProvider", FakeProvider)
    return FakeProvider


def run_launch(home_name=None, selected_gpu=None, language=None, env_vars=None, app_overrides=None, **kwargs):
    seed_admin()
    make_app(**(app_overrides or {}))
    if home_name and home_name.lower() != "cleanroom":
        user_manager.create_home_dir("admin", home_name)
    settings = user_manager.get_effective_settings("admin")
    return asyncio.run(
        api_module._launch_common(
            "app-1", "admin", settings, home_name, env_vars or {}, language, selected_gpu, **kwargs
        )
    )


def test_launch_env_volumes_and_title(fake_provider):
    result = run_launch(home_name="default", session_name="My Session")
    assert result["session_url"].startswith(f"/api/apps/session/{result['session_id']}/")
    provider = FakeProvider.last
    env = provider.launch_kwargs["env_vars"]
    # Base runtime layer.
    assert env["SUBFOLDER"].startswith(f"/api/apps/session/{result['session_id']}/")
    assert env["PUID"] == "1000"
    assert env["PGID"] == "1000"
    assert env["CUSTOM_USER"] and env["PASSWORD"]
    assert env["TZ"] == "America/Phoenix"
    assert env["SELKIES_ALLOWED_ORIGINS"] == "*"
    assert env["PIXELFLUX_WAYLAND"] == "true"
    # The app's own env layer.
    assert env["APP_ENV"] == "1"
    # The optional session name wins the TITLE layer.
    assert env["TITLE"] == "Firefox - My Session"
    # Volumes: the persistent home + the shared-files bind.
    volumes = provider.launch_kwargs["volumes"]
    home = os.path.join(api_module.settings.storage_path, "admin", "default")
    shared = os.path.join(api_module.settings.storage_path, "admin", "_vreckan_shared_files")
    assert volumes[home] == {"bind": api_module.settings.container_config_path, "mode": "rw"}
    assert volumes[shared] == {
        "bind": os.path.join(api_module.settings.container_config_path, "Desktop", "files"),
        "mode": "rw",
    }
    assert provider.launch_kwargs["gpu_config"] is None
    # The session is registered for the proxy + persistence.
    record = api_module.SESSIONS_DB[result["session_id"]]
    assert record["username"] == "admin"
    assert record["name"] == "My Session"
    assert record["container_registry"]["app-1"]["instance_id"] == "ctr-1"


def test_launch_cleanroom_uses_ephemeral_storage(fake_provider):
    result = run_launch(home_name="cleanroom")
    volumes = FakeProvider.last.launch_kwargs["volumes"]
    ephemeral_base = os.path.join(api_module.settings.storage_path, "vreckan_ephemeral")
    host_mount = next(
        p for p in volumes if p.startswith(ephemeral_base) and result["session_id"] not in p
    )
    shared = next(p for p in volumes if p != host_mount)
    assert shared.startswith(ephemeral_base)
    assert result["session_id"] in shared


def test_launch_language_sets_lc_all(fake_provider):
    run_launch(home_name="default", language="de_DE.UTF-8")
    assert FakeProvider.last.launch_kwargs["env_vars"]["LC_ALL"] == "de_DE.UTF-8"


def test_launch_english_language_leaves_lc_all_unset(fake_provider):
    run_launch(home_name="default", language="en_us.utf-8")
    assert "LC_ALL" not in FakeProvider.last.launch_kwargs["env_vars"]


def test_launch_selected_dri3_gpu_applied(fake_provider):
    api_module.AVAILABLE_GPUS = [
        {"device": "/dev/dri/renderD128", "driver": "amdgpu", "type": "dri3"}
    ]
    run_launch(home_name="default", selected_gpu="/dev/dri/renderD128")
    env = FakeProvider.last.launch_kwargs["env_vars"]
    assert env["DRI_NODE"] == "/dev/dri/renderD128"
    assert env["DRINODE"] == "/dev/dri/renderD128"
    assert FakeProvider.last.launch_kwargs["gpu_config"]["device"] == "/dev/dri/renderD128"


def test_launch_nvidia_gpu_with_wayland_sets_dri_node(fake_provider):
    pc = dict(DEFAULT_PROVIDER_CONFIG, nvidia_support=True)
    api_module.AVAILABLE_GPUS = [
        {"device": "/dev/dri/renderD129", "driver": "nvidia", "type": "nvidia", "index": 0}
    ]
    run_launch(
        home_name="default",
        selected_gpu="/dev/dri/renderD129",
        app_overrides={"provider_config": pc},
    )
    env = FakeProvider.last.launch_kwargs["env_vars"]
    assert env["DRI_NODE"] == "/dev/dri/renderD129"
    assert env["DRINODE"] == "/dev/dri/renderD129"


def test_launch_unknown_gpu_rejected(fake_provider):
    api_module.AVAILABLE_GPUS = [
        {"device": "/dev/dri/renderD128", "driver": "amdgpu", "type": "dri3"}
    ]
    with pytest.raises(HTTPException) as exc:
        run_launch(home_name="default", selected_gpu="/dev/dri/renderD999")
    assert exc.value.status_code == 400


def test_launch_nvidia_gpu_rejected_when_unsupported(fake_provider):
    api_module.AVAILABLE_GPUS = [
        {"device": "/dev/dri/renderD129", "driver": "nvidia", "type": "nvidia", "index": 0}
    ]
    # The default app has nvidia_support=False.
    with pytest.raises(HTTPException) as exc:
        run_launch(home_name="default", selected_gpu="/dev/dri/renderD129")
    assert exc.value.status_code == 400


def test_launch_global_default_gpu_fallback(fake_provider):
    api_module.AVAILABLE_GPUS = [
        {"device": "/dev/dri/renderD128", "driver": "amdgpu", "type": "dri3"}
    ]
    api_module.GLOBAL_DEFAULT_GPU = "/dev/dri/renderD128"
    run_launch(home_name="default")  # no personal pick
    assert FakeProvider.last.launch_kwargs["gpu_config"]["device"] == "/dev/dri/renderD128"


def test_launch_denied_without_access(fake_provider):
    make_app(users=["alice"])
    seed_admin()
    user_manager.create_home_dir("admin", "default")
    with pytest.raises(HTTPException) as exc:
        asyncio.run(
            api_module._launch_common(
                "app-1", "admin", user_manager.get_effective_settings("admin"), "default", {}, None, None
            )
        )
    assert exc.value.status_code == 403


def test_launch_missing_app_404(fake_provider):
    seed_admin()
    with pytest.raises(HTTPException) as exc:
        asyncio.run(
            api_module._launch_common(
                "nope", "admin", user_manager.get_effective_settings("admin"), None, {}, None, None
            )
        )
    assert exc.value.status_code == 404


def test_launch_room_mode(fake_provider):
    result = run_launch(home_name="default", launch_in_room_mode=True)
    provider = FakeProvider.last
    env = provider.launch_kwargs["env_vars"]
    assert env["SELKIES_MASTER_TOKEN"]
    kwargs = provider.launch_kwargs
    assert kwargs["is_collaboration"] is True
    assert kwargs["master_token"] == env["SELKIES_MASTER_TOKEN"]
    assert len(kwargs["initial_tokens"]) == 1
    (controller_token, info), = kwargs["initial_tokens"].items()
    assert info["role"] == "controller"
    record = api_module.SESSIONS_DB[result["session_id"]]
    assert record["is_collaboration"] is True
    assert record["master_token"] == env["SELKIES_MASTER_TOKEN"]
    assert record["controller_token"] == controller_token
    assert result["session_url"].startswith(f"/room/{result['session_id']}?")


def test_launch_base_layer_schema_env(fake_provider):
    # Re-seed the in-memory schema from the YAML baseline (the isolate
    # fixture clears it), then set admin base-layer overrides.
    api_module.TEMPLATE_SCHEMA["settings"] = list(api_module.TEMPLATE_SCHEMA_YAML)
    for s in api_module.TEMPLATE_SCHEMA["settings"]:
        if s["name"] == "SELKIES_ENCODER":
            s["current"] = "h264enc"
        elif s["name"] == "DOCKER_SHM_SIZE":
            s["current"] = "2g"
    run_launch(home_name="default")
    provider = FakeProvider.last
    env = provider.launch_kwargs["env_vars"]
    assert env["SELKIES_ENCODER"] == "h264enc"
    overrides = provider.app_config_dict["provider_config"]["docker_overrides"]
    assert overrides["shm_size"] == "2g"
    # The app's own overrides coexist with the base layer.
    assert overrides["devices"] == ["/dev/snd"]


def test_launch_template_settings_layers(fake_provider):
    api_module.APP_TEMPLATES["Default"] = {
        "name": "Default",
        "settings": {
            "TITLE": "From Template",
            "DOCKER_DEVICES": "/dev/dri/renderD128",
            "DOCKER_SHM_SIZE": "1g",
        },
    }
    run_launch(home_name="default")
    provider = FakeProvider.last
    env = provider.launch_kwargs["env_vars"]
    assert env["TITLE"] == "From Template"
    overrides = provider.app_config_dict["provider_config"]["docker_overrides"]
    # The app's own devices list is extended with the template's.
    assert overrides["devices"] == ["/dev/snd", "/dev/dri/renderD128"]
    assert overrides["shm_size"] == "1g"


def test_launch_explicit_env_vars_win_over_template(fake_provider):
    api_module.APP_TEMPLATES["Default"] = {
        "name": "Default",
        "settings": {"CUSTOM_VAR": "from-template"},
    }
    run_launch(home_name="default", env_vars={"CUSTOM_VAR": "from-request"})
    assert FakeProvider.last.launch_kwargs["env_vars"]["CUSTOM_VAR"] == "from-request"


def test_launch_url_env_var_mapping(fake_provider):
    pc = dict(DEFAULT_PROVIDER_CONFIG, url_env_var="FIREFOX_CLI")
    make_app(provider_config=pc)
    seed_admin()
    user_manager.create_home_dir("admin", "default")
    asyncio.run(
        api_module._launch_common(
            "app-1",
            "admin",
            user_manager.get_effective_settings("admin"),
            "default",
            {"VRECKAN_URL": "https://example.com"},
            None,
            None,
        )
    )
    env = FakeProvider.last.launch_kwargs["env_vars"]
    assert env["VRECKAN_URL"] == "https://example.com"
    assert env["FIREFOX_CLI"] == "https://example.com"
    sid = next(iter(api_module.SESSIONS_DB))
    assert api_module.SESSIONS_DB[sid]["launch_context"] == {
        "type": "url",
        "value": "https://example.com",
    }


def test_extract_docker_overrides_parsing():
    result = api_module._extract_docker_overrides(
        {
            "DOCKER_PRIVILEGED": "true",
            "DOCKER_CAP_ADD": "SYS_ADMIN, NET_ADMIN",
            "DOCKER_ENV": "A=1,B=2",
            "DOCKER_CPU_SHARES": "notanumber",
            "NOT_DOCKER": "x",
        }
    )
    assert result == {
        "privileged": True,
        "cap_add": ["SYS_ADMIN", "NET_ADMIN"],
        "environment": {"A": "1", "B": "2"},
    }

"""Tests for the admin app-template and template-schema endpoints.

Two related but distinct concepts:

* **Templates** (``/api/admin/apps/templates``) are named bundles of
  setting overrides that admins apply to installed apps.  Built-in
  templates ship as YAML files under ``default_app_templates_path``;
  user templates live in the database and may override a built-in by
  sharing its name.  In the test environment the default path does not
  exist, so the app starts with a single blank "Default" template —
  the ``builtin_templates`` fixture points the path at a temp dir with
  a real built-in to exercise the protection/override/restore logic.

* **The schema** (``/api/admin/apps/templates/schema``) is the base
  layer describing every setting the template editor can show.  It is
  seeded from the bundled ``template_schema.yml`` at startup and can be
  replaced wholesale through the save endpoint.
"""
from __future__ import annotations

import pytest
import yaml

from server.settings import settings


# ---------------------------------------------------------------------------
# Built-in template fixture
# ---------------------------------------------------------------------------
@pytest.fixture
def builtin_templates(tmp_path, monkeypatch):
    """Point the built-in template dir at a temp dir with one template.

    Must be requested before ``secure_client`` in the test signature so
    the patched path is in place when the app's lifespan loads templates
    at startup.
    """
    templates_dir = tmp_path / "default_templates"
    templates_dir.mkdir()
    # Filename must match the endpoint's convention:
    # name.lower().replace(" ", "_") + ".yml"
    (templates_dir / "built-in.yml").write_text(
        yaml.safe_dump({"name": "Built-In", "settings": {"TITLE": "Built In"}})
    )
    monkeypatch.setattr(settings, "default_app_templates_path", str(templates_dir))
    return templates_dir


# ---------------------------------------------------------------------------
# Template CRUD
# ---------------------------------------------------------------------------
def test_list_templates_default(secure_client):
    # No built-in templates exist in the test env, so startup creates a
    # single blank "Default" template in the database.
    status, body = secure_client.call("GET", "/api/admin/apps/templates")
    assert status == 200, body
    assert body == [{"name": "Default", "settings": {}}]


def test_create_template(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates",
        body={"name": "My Template", "settings": {"TITLE": "X"}},
    )
    assert status == 201, body
    assert body == {"name": "My Template", "settings": {"TITLE": "X"}}
    status, templates = secure_client.call("GET", "/api/admin/apps/templates")
    assert status == 200
    names = [t["name"] for t in templates]
    assert "My Template" in names
    assert "Default" in names


def test_create_template_invalid_name(secure_client):
    # The 400 used to be swallowed by the endpoint's own `except
    # Exception` handler and surfaced as a 500; the fix lets it
    # propagate.
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates",
        body={"name": "bad name!", "settings": {}},
    )
    assert status == 400
    assert body["detail"] == "Invalid template name."


def test_create_template_malformed_body(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/apps/templates", body={"settings": {}}
    )
    assert status == 422
    assert "name" in body["detail"]


def test_create_template_overrides_existing(secure_client):
    status, _ = secure_client.call(
        "POST",
        "/api/admin/apps/templates",
        body={"name": "My Template", "settings": {"TITLE": "One"}},
    )
    assert status == 201
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates",
        body={"name": "My Template", "settings": {"TITLE": "Two"}},
    )
    assert status == 201, body
    assert body["settings"] == {"TITLE": "Two"}
    status, templates = secure_client.call("GET", "/api/admin/apps/templates")
    entry = next(t for t in templates if t["name"] == "My Template")
    assert entry["settings"] == {"TITLE": "Two"}


def test_delete_template_unknown(secure_client):
    status, body = secure_client.call("DELETE", "/api/admin/apps/templates/Ghost")
    assert status == 404
    assert body["detail"] == "Template 'Ghost' not found."


def test_delete_user_template(secure_client):
    status, _ = secure_client.call(
        "POST",
        "/api/admin/apps/templates",
        body={"name": "My Template", "settings": {"TITLE": "X"}},
    )
    assert status == 201
    status, _ = secure_client.call("DELETE", "/api/admin/apps/templates/My%20Template")
    assert status == 204
    status, templates = secure_client.call("GET", "/api/admin/apps/templates")
    assert [t["name"] for t in templates] == ["Default"]


# ---------------------------------------------------------------------------
# Built-in template protection, override, and restore
# ---------------------------------------------------------------------------
def test_list_templates_with_builtin(builtin_templates, secure_client):
    # With a built-in template present at startup, no blank "Default"
    # template is created.
    status, body = secure_client.call("GET", "/api/admin/apps/templates")
    assert status == 200, body
    assert body == [{"name": "Built-In", "settings": {"TITLE": "Built In"}}]


def test_delete_builtin_template_protected(builtin_templates, secure_client):
    status, body = secure_client.call("DELETE", "/api/admin/apps/templates/Built-In")
    assert status == 403
    assert body["detail"] == (
        "Cannot delete the default template 'Built-In'. You can override it "
        "by creating a new template with the same name."
    )


def test_builtin_template_override_and_restore(builtin_templates, secure_client):
    # A pure built-in cannot be deleted ...
    status, body = secure_client.call("DELETE", "/api/admin/apps/templates/Built-In")
    assert status == 403
    # ... but it can be overridden by a user template with the same name.
    status, _ = secure_client.call(
        "POST",
        "/api/admin/apps/templates",
        body={"name": "Built-In", "settings": {"TITLE": "Overridden"}},
    )
    assert status == 201
    # Once a user override exists, deletion is allowed — and the built-in
    # (from the bundled file) takes effect again.
    status, _ = secure_client.call("DELETE", "/api/admin/apps/templates/Built-In")
    assert status == 204
    status, templates = secure_client.call("GET", "/api/admin/apps/templates")
    assert status == 200
    assert templates == [{"name": "Built-In", "settings": {"TITLE": "Built In"}}]


# ---------------------------------------------------------------------------
# Template schema (the base layer of the template editor)
# ---------------------------------------------------------------------------
def test_save_schema_missing_settings(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/apps/templates/schema", body={}
    )
    assert status == 400
    assert body["detail"] == "'settings' is required."


def test_save_schema_not_a_list(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/apps/templates/schema", body={"settings": "nope"}
    )
    assert status == 400
    assert body["detail"] == "'settings' must be a list."


def test_save_schema_entry_not_object(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/apps/templates/schema", body={"settings": [42]}
    )
    assert status == 400
    assert body["detail"] == "Each setting must be an object."


def test_save_schema_invalid_name(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates/schema",
        body={"settings": [{"name": "bad name"}]},
    )
    assert status == 400
    assert body["detail"] == "Invalid setting name: 'bad name'."


def test_save_schema_duplicate_name(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates/schema",
        body={"settings": [{"name": "TITLE"}, {"name": "TITLE"}]},
    )
    assert status == 400
    assert body["detail"] == "Duplicate setting name: 'TITLE'."


def test_save_schema_invalid_type(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates/schema",
        body={"settings": [{"name": "TITLE", "type": "bogus"}]},
    )
    assert status == 400
    assert body["detail"] == "Invalid type 'bogus' for setting 'TITLE'."


def test_save_schema_replaces_whole_schema(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates/schema",
        body={
            "settings": [
                {"name": "ONLY_ONE", "label": "Only One", "type": "text", "default": "x"}
            ]
        },
    )
    assert status == 200, body
    assert body == {"ok": True}
    status, schema = secure_client.call("GET", "/api/admin/apps/templates/schema")
    assert status == 200
    settings_list = schema["settings"]
    # The save is a wholesale replacement: every seeded setting is gone.
    assert len(settings_list) == 1
    entry = settings_list[0]
    assert entry["name"] == "ONLY_ONE"
    assert entry["label"] == "Only One"
    assert entry["default"] == "x"
    assert entry["current"] is None
    assert entry["builtin"] is False


def test_save_schema_builtin_default_tracks_yaml(secure_client):
    # Built-in baselines are read-only: an admin-supplied default is
    # ignored and the bundled YAML value wins.
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates/schema",
        body={"settings": [{"name": "TITLE", "type": "text", "default": "Wrong"}]},
    )
    assert status == 200, body
    status, schema = secure_client.call("GET", "/api/admin/apps/templates/schema")
    entry = next(s for s in schema["settings"] if s["name"] == "TITLE")
    assert entry["default"] == "Selkies"
    assert entry["builtin"] is True


def test_save_schema_empty_current_becomes_null(secure_client):
    # An empty "current" means "use the image default" and is stored as
    # NULL so the setting is not pushed through to launches.
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates/schema",
        body={"settings": [{"name": "TITLE", "type": "text", "current": ""}]},
    )
    assert status == 200, body
    status, schema = secure_client.call("GET", "/api/admin/apps/templates/schema")
    entry = next(s for s in schema["settings"] if s["name"] == "TITLE")
    assert entry["current"] is None


def test_save_schema_custom_setting_keeps_admin_default(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates/schema",
        body={
            "settings": [
                {
                    "name": "MY_CUSTOM_VAR",
                    "type": "text",
                    "default": "base",
                    "current": "override",
                }
            ]
        },
    )
    assert status == 200, body
    status, schema = secure_client.call("GET", "/api/admin/apps/templates/schema")
    entry = next(s for s in schema["settings"] if s["name"] == "MY_CUSTOM_VAR")
    assert entry["default"] == "base"
    assert entry["current"] == "override"
    assert entry["builtin"] is False


def test_save_schema_select_options_filtered(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/templates/schema",
        body={
            "settings": [
                {
                    "name": "WATERMARK_LOCATION",
                    "type": "select",
                    "options": ["junk", {"value": None}, {"value": "1", "label": "TL"}],
                }
            ]
        },
    )
    assert status == 200, body
    status, schema = secure_client.call("GET", "/api/admin/apps/templates/schema")
    entry = next(s for s in schema["settings"] if s["name"] == "WATERMARK_LOCATION")
    # Non-dict entries and entries without a value are dropped ...
    assert entry["options"] == [{"value": "1", "label": "TL"}]
    # ... and the built-in baseline is restored regardless.
    assert entry["default"] == "-1"

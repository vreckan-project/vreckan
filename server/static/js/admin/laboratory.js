// App Laboratory (Feat 6): create a meta-app from an installed base app, or
// edit an existing one. "Launch & modify" opens a live session whose home dir
// is the app's home template (read-write), so tweaks made inside are baked
// into the template for all future sessions. The panel is rendered once and
// kept alive across section switches so the embedded session iframe survives.
//
// The small DOM helpers ($, esc, toast, request, setPanel) are local copies so
// this file is self-contained; `render` comes from admin.js.
import { $, esc, toast, request } from "../util.js";
import { confirmModal } from "../modal.js";
import { t } from "../i18n.js";
import { render } from "../admin.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

let labApps = [];
let labState = { isEditing: false, currentApp: null, currentSessionId: null, sessionUrl: null, base64Icon: "", isDirty: false, dirtyFields: null };

function decodeB64(value) {
  if (!value) return "";
  try {
    return atob(value);
  } catch (e) {
    return "";
  }
}

// Resolve an app logo to an <img> src (fetches custom icons as data URLs).
async function resolveLogoSrc(app) {
  const logo = app.logo || "";
  if (logo.startsWith("http://") || logo.startsWith("https://")) return logo;
  if (logo.startsWith("/api/app_icon/")) {
    try {
      const data = await request(logo, "GET");
      if (data && data.icon_data_b64) return `data:image/png;base64,${data.icon_data_b64}`;
    } catch (e) {
      /* fall through to default */
    }
  }
  return "/img/icon128.png";
}

function setLabStatus(message, kind) {
  const el = $("lab-status");
  if (!el) return;
  if (!message) {
    el.textContent = "";
    el.hidden = true;
    el.className = "lab-status";
    return;
  }
  el.textContent = message;
  el.hidden = false;
  el.className = `lab-status${kind === "error" ? " lab-status-error" : kind === "success" ? " lab-status-ok" : ""}`;
}

function labPanelHtml() {
  return `
  <div class="card">
    <h3>${t("lab.heading")}</h3>
    <p class="muted">${t("lab.help")}</p>
    <div id="lab-status" class="lab-status" hidden></div>
    <form id="lab-form">
      <div class="admin-form-row">
        <div class="field">
          <label for="lab-app-select">${t("lab.appToCustomize")}</label>
          <select id="lab-app-select"><option value="new">${t("lab.createNew")}</option></select>
        </div>
        <div class="field">
          <label for="lab-base-app-select">${t("lab.baseApp")}</label>
          <select id="lab-base-app-select"><option value="">${t("lab.selectBaseApp")}</option></select>
        </div>
      </div>
      <p class="muted" id="lab-base-desc">${t("lab.baseDesc")}</p>
      <div class="admin-form-row">
        <div class="field">
          <label for="lab-app-name">${t("lab.appName")}</label>
          <input type="text" id="lab-app-name" maxlength="64">
        </div>
        <div class="field">
          <label>${t("lab.appIcon")}</label>
          <div class="admin-form-row">
            <img id="lab-icon-preview" class="lab-icon-preview" src="/img/icon128.png" alt="${t("lab.iconPreview")}">
            <button type="button" class="btn btn-ghost" id="lab-icon-upload-btn">${t("common.upload")}</button>
            <input type="file" id="lab-icon-upload" accept="image/png" hidden>
          </div>
        </div>
      </div>
      <div class="field">
        <label for="lab-autostart-script">${t("lab.autostartX11")}</label>
        <textarea id="lab-autostart-script" rows="4" placeholder="program \${VRECKAN_FILE:+&quot;$VRECKAN_FILE&quot;} \${VRECKAN_URL:+&quot;$VRECKAN_URL&quot;}"></textarea>
      </div>
      <div class="field">
        <label for="lab-autostart-wayland-script">${t("lab.autostartWayland")}</label>
        <textarea id="lab-autostart-wayland-script" rows="4" placeholder="program \${VRECKAN_FILE:+&quot;$VRECKAN_FILE&quot;} \${VRECKAN_URL:+&quot;$VRECKAN_URL&quot;}"></textarea>
      </div>
      <div class="admin-form-row">
        <div class="field">
          <label for="lab-app-users">${t("lab.allowedUsers")}</label>
          <input type="text" id="lab-app-users" value="all">
        </div>
        <div class="field">
          <label for="lab-app-groups">${t("lab.allowedGroups")}</label>
          <input type="text" id="lab-app-groups" value="all">
        </div>
        <div class="field field-row">
          <label class="check">
            <input type="checkbox" id="lab-launch-wayland" checked>
            <span>${t("lab.waylandMode")}</span>
          </label>
        </div>
      </div>
      <div class="admin-form-row" style="margin-top:12px;">
        <button type="button" class="btn btn-ghost" id="lab-update-btn" disabled>${t("lab.saveChanges")}</button>
        <button type="button" class="btn btn-primary" id="lab-launch-btn" style="flex-grow:1;">
          <span id="lab-launch-btn-text">${t("lab.launchModify")}</span>
        </button>
      </div>
    </form>
  </div>

  <div class="card" style="margin-top:14px;">
    <h3>${t("lab.customizationSession")}</h3>
    <p id="lab-main-placeholder" class="muted">${t("lab.sessionPlaceholder")}</p>
    <iframe id="lab-session-frame" class="lab-session-frame" hidden></iframe>
  </div>`;
}

function resetLabForm() {
  labState = { isEditing: false, currentApp: null, currentSessionId: null, sessionUrl: null, base64Icon: "", isDirty: false, dirtyFields: null };
  const appSelect = $("lab-app-select");
  if (appSelect) appSelect.value = "new";
  const nameInput = $("lab-app-name");
  if (nameInput) {
    nameInput.value = "";
    nameInput.disabled = false;
  }
  const baseSelect = $("lab-base-app-select");
  if (baseSelect) {
    baseSelect.value = "";
    baseSelect.disabled = false;
  }
  const preview = $("lab-icon-preview");
  if (preview) preview.src = "/img/icon128.png";
  const s1 = $("lab-autostart-script");
  if (s1) s1.value = "";
  const s2 = $("lab-autostart-wayland-script");
  if (s2) s2.value = "";
  const users = $("lab-app-users");
  if (users) users.value = "all";
  const groups = $("lab-app-groups");
  if (groups) groups.value = "all";
  const wayland = $("lab-launch-wayland");
  if (wayland) wayland.checked = true;
  const updateBtn = $("lab-update-btn");
  if (updateBtn) updateBtn.disabled = true;
}

async function loadLabData(app) {
  labState.isEditing = true;
  labState.currentApp = app;
  const nameInput = $("lab-app-name");
  if (nameInput) {
    nameInput.value = app.name;
    nameInput.disabled = true;
  }
  const baseSelect = $("lab-base-app-select");
  if (baseSelect) {
    baseSelect.value = app.base_app_id || "";
    baseSelect.disabled = true;
  }
  const src = await resolveLogoSrc(app);
  const preview = $("lab-icon-preview");
  if (preview) preview.src = src;
  labState.base64Icon = src;
  const users = $("lab-app-users");
  if (users) users.value = (app.users || []).join(",");
  const groups = $("lab-app-groups");
  if (groups) groups.value = (app.groups || []).join(",");
  const s1 = $("lab-autostart-script");
  if (s1) s1.value = decodeB64(app.provider_config && app.provider_config.custom_autostart_script_b64);
  const s2 = $("lab-autostart-wayland-script");
  if (s2) s2.value = decodeB64(app.provider_config && app.provider_config.custom_autostart_wayland_script_b64);
  labState.isDirty = false;
  const updateBtn = $("lab-update-btn");
  if (updateBtn) updateBtn.disabled = true;
}

function populateLabDropdowns(desiredAppId, desiredBaseId) {
  const labAppSelect = $("lab-app-select");
  const baseAppSelect = $("lab-base-app-select");
  if (!labAppSelect || !baseAppSelect) return;

  // On a fresh remount the selects only contain the placeholder option, so
  // the desired selection must be passed in explicitly (setting .value on a
  // select that lacks the option is a no-op).
  const currentLabApp = desiredAppId || labAppSelect.value;
  let html = `<option value="new">${t("lab.createNew")}</option>`;
  labApps.filter((a) => a.is_meta_app).forEach((a) => {
    html += `<option value="${esc(a.id)}">${esc(a.name)}</option>`;
  });
  labAppSelect.innerHTML = html;
  if ([...labAppSelect.options].some((o) => o.value === currentLabApp)) {
    labAppSelect.value = currentLabApp;
  } else {
    labAppSelect.value = "new";
    if (labState.isEditing) labAppSelect.dispatchEvent(new Event("change"));
  }

  const currentBaseApp = desiredBaseId || baseAppSelect.value;
  let bhtml = `<option value="">${t("lab.selectBaseApp")}</option>`;
  labApps.filter((a) => !a.is_meta_app).forEach((a) => {
    bhtml += `<option value="${esc(a.id)}">${esc(a.name)}</option>`;
  });
  baseAppSelect.innerHTML = bhtml;
  if ([...baseAppSelect.options].some((o) => o.value === currentBaseApp)) {
    baseAppSelect.value = currentBaseApp;
  }
}

async function closeLabSession() {
  const id = labState.currentSessionId;
  labState.currentSessionId = null;
  labState.sessionUrl = null;
  if (!id) return;
  try {
    await request(`/api/admin/sessions/${id}`, "DELETE");
    setLabStatus(t("lab.sessionClosed"), "success");
  } catch (err) {
    setLabStatus(t("lab.closeFailed", { message: err.message }), "error");
  }
  const frame = $("lab-session-frame");
  if (frame) {
    frame.src = "about:blank";
    frame.hidden = true;
  }
  const placeholder = $("lab-main-placeholder");
  if (placeholder) placeholder.hidden = false;
  const btnText = $("lab-launch-btn-text");
  if (btnText) btnText.textContent = t("lab.launchModify");
  window.dispatchEvent(new CustomEvent("vreckan:sessions-changed"));
}

async function handleLabUpdate() {
  if (!labState.isEditing || !labState.currentApp || !labState.isDirty) return false;
  setLabStatus(t("lab.savingSettings"));
  const updateBtn = $("lab-update-btn");
  if (updateBtn) updateBtn.disabled = true;
  try {
    const payload = { ...labState.currentApp, provider_config: { ...(labState.currentApp.provider_config || {}) } };
    // Only replace the logo when a new icon was uploaded; otherwise keep the
    // app's current logo (InstalledApp.logo is required, and the backend
    // leaves URL logos untouched).
    if (labState.base64Icon.startsWith("data:image")) {
      payload.logo = labState.base64Icon.split(",")[1];
    }
    payload.users = $("lab-app-users").value.split(",").map((s) => s.trim()).filter(Boolean);
    payload.groups = $("lab-app-groups").value.split(",").map((s) => s.trim()).filter(Boolean);
    payload.provider_config.custom_autostart_script_b64 = btoa($("lab-autostart-script").value);
    payload.provider_config.custom_autostart_wayland_script_b64 = btoa($("lab-autostart-wayland-script").value);
    const r = await request(`/api/admin/apps/installed/${labState.currentApp.id}`, "PUT", payload);
    if (r.error) throw new Error(r.error.message);
    labState.currentApp = r.data;
    const src = await resolveLogoSrc(r.data);
    const preview = $("lab-icon-preview");
    if (preview) preview.src = src;
    labState.base64Icon = src;
    labState.isDirty = false;
    setLabStatus(t("lab.saved", { name: r.data.name }), "success");
    window.dispatchEvent(new CustomEvent("vreckan:apps-changed"));
    return true;
  } catch (err) {
    setLabStatus(err.message, "error");
    return false;
  } finally {
    if (updateBtn) updateBtn.disabled = !labState.isDirty;
  }
}

async function handleLabLaunch() {
  const launchBtn = $("lab-launch-btn");
  const btnText = $("lab-launch-btn-text");

  // If a customization session is already running, this button closes it.
  if (labState.currentSessionId) {
    await closeLabSession();
    resetLabForm();
    return;
  }

  const isCreatingNew = $("lab-app-select").value === "new";
  if (isCreatingNew && (!$("lab-base-app-select").value || !$("lab-app-name").value.trim())) {
    setLabStatus(t("lab.pickBaseAndName"), "error");
    return;
  }

  if (launchBtn) launchBtn.disabled = true;
  if (btnText) btnText.textContent = t("lab.savingLaunching");
  try {
    let appToLaunch;
    if (isCreatingNew) {
      const payload = {
        name: $("lab-app-name").value.trim(),
        base_app_id: $("lab-base-app-select").value,
        logo: labState.base64Icon.startsWith("data:image") ? labState.base64Icon.split(",")[1] : labState.base64Icon,
        custom_autostart_script_b64: btoa($("lab-autostart-script").value),
        custom_autostart_wayland_script_b64: btoa($("lab-autostart-wayland-script").value),
        users: $("lab-app-users").value.split(",").map((s) => s.trim()).filter(Boolean),
        groups: $("lab-app-groups").value.split(",").map((s) => s.trim()).filter(Boolean),
      };
      const r = await request("/api/admin/apps/meta", "POST", payload);
      if (r.error) throw new Error(r.error.message);
      appToLaunch = r.data;
      // Register the new app and switch the form into edit mode.
      labApps = labApps.filter((a) => a.id !== appToLaunch.id).concat(appToLaunch);
      populateLabDropdowns();
      $("lab-app-select").value = appToLaunch.id;
      await loadLabData(appToLaunch);
      setLabStatus(t("lab.createdLaunching", { name: appToLaunch.name }));
      window.dispatchEvent(new CustomEvent("vreckan:apps-changed"));
    } else {
      if (labState.isDirty) {
        const ok = await handleLabUpdate();
        if (!ok) throw new Error(t("lab.saveBeforeLaunchFailed"));
      }
      appToLaunch = labState.currentApp;
    }

    const lr = await request("/api/admin/launch/meta_customize", "POST", {
      application_id: appToLaunch.id,
      wayland_mode: $("lab-launch-wayland").checked,
    });
    if (lr.error) throw new Error(lr.error.message);

    labState.currentSessionId = lr.data.session_id;
    const frame = $("lab-session-frame");
    if (frame) {
      frame.src = lr.data.session_url + (lr.data.session_url.includes("?") ? "&" : "?") + "embedded=true";
      frame.hidden = false;
      labState.sessionUrl = frame.src;
    }
    const placeholder = $("lab-main-placeholder");
    if (placeholder) placeholder.hidden = true;
    if (btnText) btnText.textContent = t("lab.closeSession");
    setLabStatus(t("lab.sessionRunning"), "success");
    window.dispatchEvent(new CustomEvent("vreckan:sessions-changed"));
  } catch (err) {
    setLabStatus(err.message, "error");
    if (btnText) btnText.textContent = t("lab.launchModify");
  } finally {
    if (launchBtn) launchBtn.disabled = false;
  }
}

function wireLaboratory() {
  const labAppSelect = $("lab-app-select");
  const baseAppSelect = $("lab-base-app-select");
  if (!labAppSelect || !baseAppSelect) return;

  labAppSelect.addEventListener("change", async (e) => {
    const appId = e.target.value;
    if (labState.currentSessionId) await closeLabSession();
    if (appId === "new") {
      resetLabForm();
    } else {
      const app = labApps.find((a) => a.id === appId);
      if (app) loadLabData(app);
    }
  });

  baseAppSelect.addEventListener("change", async () => {
    if (baseAppSelect.disabled) return;
    const baseApp = baseAppSelect.value ? labApps.find((a) => a.id === baseAppSelect.value) : null;
    if (baseApp) {
      const s1 = $("lab-autostart-script");
      if (s1) s1.value = decodeB64(baseApp.provider_config && baseApp.provider_config.custom_autostart_script_b64);
      const s2 = $("lab-autostart-wayland-script");
      if (s2) s2.value = decodeB64(baseApp.provider_config && baseApp.provider_config.custom_autostart_wayland_script_b64);
      const src = await resolveLogoSrc(baseApp);
      const preview = $("lab-icon-preview");
      if (preview) preview.src = src;
      labState.base64Icon = src;
    } else {
      const s1 = $("lab-autostart-script");
      if (s1) s1.value = "";
      const s2 = $("lab-autostart-wayland-script");
      if (s2) s2.value = "";
      const preview = $("lab-icon-preview");
      if (preview) preview.src = "/img/icon128.png";
      labState.base64Icon = "";
    }
  });

  const markDirty = () => {
    if (labState.isEditing) {
      labState.isDirty = true;
      // Snapshot the editable fields so a panel remount (section switch)
      // can restore unsaved edits.
      labState.dirtyFields = {
        users: $("lab-app-users").value,
        groups: $("lab-app-groups").value,
        script: $("lab-autostart-script").value,
        waylandScript: $("lab-autostart-wayland-script").value,
        icon: labState.base64Icon,
      };
      const updateBtn = $("lab-update-btn");
      if (updateBtn) updateBtn.disabled = false;
    }
  };
  $("lab-autostart-script").addEventListener("input", markDirty);
  $("lab-autostart-wayland-script").addEventListener("input", markDirty);
  $("lab-app-users").addEventListener("input", markDirty);
  $("lab-app-groups").addEventListener("input", markDirty);

  $("lab-icon-upload-btn").addEventListener("click", () => $("lab-icon-upload").click());
  $("lab-icon-upload").addEventListener("change", (e) => {
    const file = e.target.files && e.target.files[0];
    if (file && file.type === "image/png") {
      const reader = new FileReader();
      reader.onload = (ev) => {
        labState.base64Icon = ev.target.result;
        const preview = $("lab-icon-preview");
        if (preview) preview.src = labState.base64Icon;
        markDirty();
      };
      reader.readAsDataURL(file);
    } else if (file) {
      toast(t("lab.pngOnly"), "error");
    }
    e.target.value = "";
  });

  const updateBtn = $("lab-update-btn");
  if (updateBtn) updateBtn.addEventListener("click", () => handleLabUpdate());
  const launchBtn = $("lab-launch-btn");
  if (launchBtn) launchBtn.addEventListener("click", () => handleLabLaunch());
  const form = $("lab-form");
  if (form) form.addEventListener("submit", (e) => e.preventDefault());
}

async function renderLaboratory() {
  const res = await request("/api/admin/apps/installed", "GET");
  if (res.error) {
    if ($("admin-section")?.querySelector("#lab-app-select")) {
      toast(res.error.message, "error");
    } else {
      setPanel(`<div class="card"><h3>${t("lab.heading")}</h3><div class="error">${esc(res.error.message)}</div></div>`);
    }
    return;
  }
  labApps = res.data || [];
  const panelMounted = !!$("admin-section")?.querySelector("#lab-app-select");
  let desiredAppId;
  let desiredBaseId;
  if (!panelMounted) {
    // First mount, or remount after the user visited another section: rebuild
    // the panel and re-wire it (fresh elements, so no duplicate listeners).
    setPanel(labPanelHtml());
    wireLaboratory();
    // Restore the form to the app being edited (the panel was rebuilt).
    if (labState.isEditing && labState.currentApp) {
      desiredAppId = labState.currentApp.id;
      desiredBaseId = labState.currentApp.base_app_id || undefined;
      const wasDirty = labState.isDirty;
      await loadLabData(labState.currentApp);
      if (wasDirty && labState.dirtyFields) {
        const f = labState.dirtyFields;
        const users = $("lab-app-users");
        if (users) users.value = f.users;
        const groups = $("lab-app-groups");
        if (groups) groups.value = f.groups;
        const s1 = $("lab-autostart-script");
        if (s1) s1.value = f.script;
        const s2 = $("lab-autostart-wayland-script");
        if (s2) s2.value = f.waylandScript;
        if (f.icon) {
          labState.base64Icon = f.icon;
          const preview = $("lab-icon-preview");
          if (preview) preview.src = f.icon;
        }
        labState.isDirty = true;
        const updateBtn = $("lab-update-btn");
        if (updateBtn) updateBtn.disabled = false;
      }
    }
    // Restore a still-running customization session's iframe.
    if (labState.currentSessionId && labState.sessionUrl) {
      const frame = $("lab-session-frame");
      if (frame) {
        frame.src = labState.sessionUrl;
        frame.hidden = false;
      }
      const placeholder = $("lab-main-placeholder");
      if (placeholder) placeholder.hidden = true;
      const btnText = $("lab-launch-btn-text");
      if (btnText) btnText.textContent = t("lab.closeSession");
    }
  }
  populateLabDropdowns(desiredAppId, desiredBaseId);
}

export { renderLaboratory };

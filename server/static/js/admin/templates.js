// Templates section (Feat 14: categorized editor): the base-layer schema
// editor, the app-template form, and the live UI preview.
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

// One editable row for a schema setting. `isNew` rows have an editable name;
// existing names are fixed (remove + add to rename). The row shows the
// image baseline (read-only for built-in settings) and the admin's
// base-layer override ("current"; empty = use the image default).
function gsRowHtml(s, isNew) {
  const type = s.type || "text";
  const def = s.default || "";
  const cur = s.current === null || s.current === undefined ? "" : String(s.current);
  // Built-in settings ship their baseline from the image, so it is
  // reference-only; only custom settings may define their own baseline.
  const defaultHtml = `<input class="gs-default" type="text" value="${esc(def)}" placeholder="${t("tpl.imageDefault")}" ${s.builtin ? "readonly" : ""}>`;
  let currentHtml;
  if (type === "boolean") {
    currentHtml = `<select class="gs-current">
       <option value="" ${cur === "" ? "selected" : ""}>${t("tpl.useDefault")}</option>
       <option value="true" ${cur === "true" ? "selected" : ""}>true</option>
       <option value="false" ${cur === "false" ? "selected" : ""}>false</option>
     </select>`;
  } else if (type === "select") {
    const opts = (s.options || []).slice();
    if (cur && !opts.some((o) => o.value === cur)) opts.push({ value: cur, label: `${cur} (${t("tpl.current")})` });
    currentHtml = `<select class="gs-current">
       <option value="" ${cur === "" ? "selected" : ""}>${t("tpl.useDefault")}</option>
       ${opts.map((o) => `<option value="${esc(o.value)}" ${cur === o.value ? "selected" : ""}>${esc(o.label || o.value)}</option>`).join("")}
     </select>`;
  } else {
    currentHtml = `<input class="gs-current" type="text" value="${esc(cur)}" placeholder="${t("tpl.useDefault")}">`;
  }
  const optionsHtml =
    type === "select"
      ? `<div class="gs-row-options"><textarea class="gs-options" rows="2" placeholder="${t("tpl.optionsPlaceholder")}">${esc((s.options || []).map((o) => `${o.value}:${o.label || o.value}`).join("\n"))}</textarea></div>`
      : "";
  return `
  <div class="gs-row" data-name="${esc(s.name || "")}" data-builtin="${s.builtin ? "1" : "0"}">
    <div class="gs-row-top">
      <input class="gs-name" type="text" value="${esc(s.name || "")}" placeholder="ENV_VAR_NAME" ${isNew ? "" : "readonly"}>
      <button class="btn btn-sm btn-danger gs-remove" type="button">${t("common.remove")}</button>
    </div>
    <div class="gs-row-fields">
      <input class="gs-label" type="text" value="${esc(s.label || "")}" placeholder="${t("tpl.label")}">
      <input class="gs-description" type="text" value="${esc(s.description || "")}" placeholder="${t("tpl.description")}">
      <input class="gs-category" type="text" value="${esc(s.category || "")}" placeholder="${t("tpl.category")}" style="max-width:130px;">
      <select class="gs-type">
        <option value="text" ${type === "text" ? "selected" : ""}>text</option>
        <option value="boolean" ${type === "boolean" ? "selected" : ""}>boolean</option>
        <option value="select" ${type === "select" ? "selected" : ""}>select</option>
      </select>
      <label class="gs-docker-label"><input type="checkbox" class="gs-docker" ${s.docker ? "checked" : ""}> docker</label>
    </div>
    <div class="gs-row-values">
      <div class="field">
        <label>${t("tpl.defaultLabel")}</label>
        ${defaultHtml}
      </div>
      <div class="field">
        <label>${t("tpl.currentLabel")}</label>
        ${currentHtml}
      </div>
    </div>
    ${optionsHtml}
  </div>`;
}

// Wire a settings row's interactive bits (remove; re-render on type change).
function wireGsRow(row) {
  const removeBtn = row.querySelector(".gs-remove");
  if (removeBtn) {
    removeBtn.addEventListener("click", () => {
      const name = row.querySelector(".gs-name").value;
      row.remove();
      if (name) toast(t("tpl.removed", { name, save: t("tpl.saveBaseLayer") }));
    });
  }
  const typeSel = row.querySelector(".gs-type");
  if (typeSel) {
    typeSel.addEventListener("change", () => {
      const holder = document.createElement("div");
      holder.innerHTML = gsRowHtml(
        {
          name: row.querySelector(".gs-name").value,
          label: row.querySelector(".gs-label").value,
          description: row.querySelector(".gs-description").value,
          category: row.querySelector(".gs-category").value,
          type: typeSel.value,
          default: row.querySelector(".gs-default") ? row.querySelector(".gs-default").value : "",
          current: row.querySelector(".gs-current") ? row.querySelector(".gs-current").value : "",
          docker: row.querySelector(".gs-docker").checked,
          options: parseGsOptions(row.querySelector(".gs-options") ? row.querySelector(".gs-options").value : ""),
          builtin: row.dataset.builtin === "1",
        },
        !row.dataset.name
      );
      const replacement = holder.firstElementChild;
      row.replaceWith(replacement);
      wireGsRow(replacement);
    });
  }
}

// Parse the options textarea ("value:label" per line) into option objects.
function parseGsOptions(text) {
  return (text || "")
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => {
      const idx = l.indexOf(":");
      if (idx === -1) return { value: l, label: l };
      const value = l.slice(0, idx).trim();
      return { value, label: l.slice(idx + 1).trim() || value };
    });
}

// --- Templates (Feat 14: categorized editor) ------------------------------
let tplSettings = []; // Resolved schema: labels, defaults, options.
let tplList = []; // Templates from the API.
let tplState = { selected: "new", name: "", values: null }; // Snapshot so an
// in-progress edit survives a section switch (the panel is wiped on leave).

// [categoryKey, i18nKey] — the title is an i18n key (not a resolved string)
// because this const is evaluated at module-import time, before the
// translator has loaded; it is resolved with t() at render time.
const TPL_CATEGORIES = [
  ["ui", "tpl.catUi"],
  ["app", "tpl.catApp"],
  ["hardening", "tpl.catHardening"],
  ["general", "tpl.catGeneral"],
  ["webrtc", "tpl.catWebrtc"],
  ["docker", "tpl.catDocker"],
];

// Turn the server schema into editor definitions. Vreckan has no i18n layer,
// so labels/descriptions come from the schema itself (the server ships them
// in template_schema.yml); the variable name is the final fallback.
function resolveTemplateSchema(schema) {
  return (schema.settings || []).map((s) => {
    const def = {
      name: s.name,
      category: s.category,
      type: s.type,
      default: s.default === undefined || s.default === null ? "" : String(s.default),
      current: s.current === undefined || s.current === null ? null : String(s.current),
      docker: !!s.docker,
      builtin: !!s.builtin,
      label: s.label || s.name,
      description: s.description || "",
    };
    if (s.type === "select") {
      def.options = {};
      (s.options || []).forEach((opt) => {
        const value = String(opt.value ?? "");
        def.options[value] = opt.label || value;
      });
    }
    return def;
  });
}

// The value an app effectively gets from the base layer for a setting: the
// admin's override when one is set, otherwise the image baseline. App
// template forms start from this, and only fields the admin changes away
// from it are stored/pushed through.
function effBaseValue(s) {
  return s.current !== null && s.current !== undefined ? s.current : s.default || "";
}

function tplFieldHtml(s) {
  const id = `tplf-${s.name}`;
  const desc = s.description ? `<p class="muted tpl-desc">${esc(s.description)}</p>` : "";
  // Fields start from the base layer's effective value (the admin's
  // override when set, else the image baseline) so the form mirrors what
  // the app actually gets.
  const base = effBaseValue(s);
  if (s.type === "boolean") {
    return `<div class="tpl-field">
      <label class="check" for="${id}"><input type="checkbox" id="${id}"> <span>${esc(s.label)}</span></label>
      ${desc}
    </div>`;
  }
  if (s.type === "select") {
    const opts = Object.entries(s.options)
      .map(([value, text]) => `<option value="${esc(value)}"${value === base ? " selected" : ""}>${esc(text)}</option>`)
      .join("");
    // Settings whose baseline is empty need an explicit "image default"
    // option, otherwise the browser would auto-select the first option.
    const emptyOpt =
      s.default === ""
        ? `<option value=""${base === "" ? " selected" : ""}>${t("tpl.imageDefault")}</option>`
        : "";
    return `<div class="tpl-field">
      <label for="${id}">${esc(s.label)}</label>
      <select id="${id}">${emptyOpt}${opts}</select>
      ${desc}
    </div>`;
  }
  return `<div class="tpl-field">
    <label for="${id}">${esc(s.label)}</label>
    <input type="text" id="${id}" value="${esc(base)}" placeholder="${esc(s.default || "")}">
    ${desc}
  </div>`;
}

function buildTplForm() {
  TPL_CATEGORIES.forEach(([key]) => {
    const c = $(`tpl-form-${key}`);
    if (c) c.innerHTML = "";
  });
  tplSettings.forEach((s) => {
    const c = $(`tpl-form-${s.category}`);
    if (c) c.insertAdjacentHTML("beforeend", tplFieldHtml(s));
  });
}

function collectTplValues() {
  const values = {};
  tplSettings.forEach((s) => {
    const el = $(`tplf-${s.name}`);
    if (!el) return;
    values[s.name] = s.type === "boolean" ? (el.checked ? "true" : "false") : el.value;
  });
  return values;
}

function applyTplValues(values) {
  if (!values) return;
  tplSettings.forEach((s) => {
    const el = $(`tplf-${s.name}`);
    if (!el || values[s.name] === undefined) return;
    if (s.type === "boolean") el.checked = values[s.name] === "true";
    else el.value = values[s.name];
  });
}

function loadTplIntoForm(name) {
  const tpl = tplList.find((t) => t.name === name);
  const settings = tpl ? tpl.settings || {} : {};
  tplSettings.forEach((s) => {
    const el = $(`tplf-${s.name}`);
    if (!el) return;
    // Unstored fields follow the base layer's effective value.
    const v = settings[s.name] ?? effBaseValue(s);
    if (s.type === "boolean") el.checked = v === "true";
    else el.value = v;
  });
  updateTplPreview();
}

function updateTplPreview() {
  const getVal = (name, isCheckbox = false) => {
    const el = $(`tplf-${name}`);
    if (!el) return isCheckbox ? false : "";
    return isCheckbox ? el.checked : el.value;
  };
  const set = (id, fn) => {
    const el = $(id);
    if (el) fn(el);
  };
  set("preview-page-title", (el) => {
    el.textContent = getVal("TITLE") || "Selkies";
  });
  const showSidebar = getVal("SELKIES_UI_SHOW_SIDEBAR", true);
  set("preview-sidebar", (el) => {
    el.style.width = showSidebar ? "30%" : "0";
    el.style.padding = showSidebar ? "1rem" : "0";
    el.style.borderRight = showSidebar ? "1px solid var(--border)" : "none";
  });
  set("preview-title", (el) => {
    el.textContent = getVal("SELKIES_UI_TITLE") || "Selkies";
  });
  const toggles = {
    "preview-logo": "SELKIES_UI_SHOW_LOGO",
    "preview-core-buttons": "SELKIES_UI_SHOW_CORE_BUTTONS",
    "preview-soft-buttons": "SELKIES_UI_SIDEBAR_SHOW_SOFT_BUTTONS",
    "preview-video-settings": "SELKIES_UI_SIDEBAR_SHOW_VIDEO_SETTINGS",
    "preview-screen-settings": "SELKIES_UI_SIDEBAR_SHOW_SCREEN_SETTINGS",
    "preview-audio-settings": "SELKIES_UI_SIDEBAR_SHOW_AUDIO_SETTINGS",
    "preview-stats": "SELKIES_UI_SIDEBAR_SHOW_STATS",
    "preview-clipboard": "SELKIES_UI_SIDEBAR_SHOW_CLIPBOARD",
    "preview-files": "SELKIES_UI_SIDEBAR_SHOW_FILES",
    "preview-apps": "SELKIES_UI_SIDEBAR_SHOW_APPS",
    "preview-sharing": "SELKIES_UI_SIDEBAR_SHOW_SHARING",
    "preview-gamepads": "SELKIES_UI_SIDEBAR_SHOW_GAMEPADS",
  };
  Object.entries(toggles).forEach(([id, name]) => {
    set(id, (el) => {
      el.style.display = getVal(name, true) ? "block" : "none";
    });
  });
  set("preview-keyboard-button", (el) => {
    el.style.display = getVal("SELKIES_UI_SIDEBAR_SHOW_KEYBOARD_BUTTON", true) ? "flex" : "none";
  });
}

function setTplStatus(message, kind) {
  const el = $("tpl-status");
  if (!el) return;
  el.textContent = message || "";
  el.className =
    "lab-status" + (kind === "error" ? " lab-status-error" : kind === "ok" ? " lab-status-ok" : "");
  el.hidden = !message;
}

function populateTplDropdowns(desiredName) {
  const sel = $("tpl-select");
  if (!sel) return;
  // On a fresh remount the select only contains the placeholder option, so
  // the desired selection must be passed in explicitly (setting .value on a
  // select that lacks the option is a no-op).
  const current = desiredName || sel.value;
  let html = `<option value="new">${t("tpl.createNew")}</option>`;
  tplList.forEach((t) => {
    html += `<option value="${esc(t.name)}">${esc(t.name)}</option>`;
  });
  sel.innerHTML = html;
  if ([...sel.options].some((o) => o.value === current)) sel.value = current;
  else sel.value = "new";
}

// Apply the UI consequences of a template selection (name-field visibility,
// delete-button visibility, form contents).
async function onTplSelected(name) {
  const nameGroup = $("tpl-name-group");
  const deleteBtn = $("tpl-delete-btn");
  const isNew = name === "new";
  if (nameGroup) nameGroup.hidden = !isNew;
  // "Default" is recreated on every server start, so it cannot be deleted.
  if (deleteBtn) deleteBtn.hidden = isNew || name === "Default";
  if (isNew) {
    loadTplIntoForm(null);
    tplState.selected = "new";
    tplState.values = null;
  } else {
    loadTplIntoForm(name);
    tplState.selected = name;
    tplState.values = null; // Form now mirrors the saved template.
  }
}

async function saveTpl() {
  const sel = $("tpl-select");
  const nameInput = $("tpl-name-input");
  const name = sel.value === "new" ? (nameInput ? nameInput.value.trim() : "") : sel.value;
  if (!name) {
    setTplStatus(t("tpl.enterNameFirst"), "error");
    return;
  }
  // Only values the admin changed away from the base layer's effective
  // value are stored; everything else follows the base layer at launch.
  const settings = {};
  tplSettings.forEach((s) => {
    const el = $(`tplf-${s.name}`);
    if (!el) return;
    const value = s.type === "boolean" ? (el.checked ? "true" : "false") : el.value;
    if (value !== effBaseValue(s)) settings[s.name] = value;
  });
  const btn = $("tpl-save-btn");
  if (btn) btn.disabled = true;
  try {
    const r = await request("/api/admin/apps/templates", "POST", { name, settings });
    if (r.error) throw new Error(r.error.message);
    setTplStatus(t("tpl.saved", { name }), "ok");
    const tr = await request("/api/admin/apps/templates", "GET");
    if (!tr.error) tplList = tr.data || [];
    populateTplDropdowns(name);
    if (sel) sel.value = name;
    if (nameInput) nameInput.value = "";
    tplState.name = "";
    await onTplSelected(name);
  } catch (err) {
    setTplStatus(err.message, "error");
  } finally {
    if (btn) btn.disabled = false;
  }
}

async function deleteTpl() {
  const sel = $("tpl-select");
  const name = sel ? sel.value : "";
  if (!name || name === "new" || name === "Default") {
    setTplStatus(t("tpl.cannotDelete"), "error");
    return;
  }
  if (
    !(await confirmModal(t("tpl.confirmDelete", { name }), {
      title: t("tpl.confirmDeleteTitle"),
      confirmLabel: t("common.delete"),
      danger: true,
    }))
  )
    return;
  const r = await request(`/api/admin/apps/templates/${encodeURIComponent(name)}`, "DELETE");
  if (r.error) {
    if (r.status === 403) setTplStatus(t("tpl.cannotDeleteDefault"), "error");
    else setTplStatus(r.error.message, "error");
    return;
  }
  setTplStatus(t("tpl.deleted", { name }), "ok");
  const tr = await request("/api/admin/apps/templates", "GET");
  if (!tr.error) tplList = tr.data || [];
  populateTplDropdowns("new");
  await onTplSelected("new");
}

function wireTemplates() {
  const sel = $("tpl-select");
  if (sel) sel.addEventListener("change", () => onTplSelected(sel.value));
  const saveBtn = $("tpl-save-btn");
  if (saveBtn) saveBtn.addEventListener("click", () => saveTpl());
  const deleteBtn = $("tpl-delete-btn");
  if (deleteBtn) deleteBtn.addEventListener("click", () => deleteTpl());
  const nameInput = $("tpl-name-input");
  if (nameInput)
    nameInput.addEventListener("input", () => {
      tplState.name = nameInput.value;
    });
  const form = $("tpl-form");
  if (form) {
    form.addEventListener("input", () => {
      tplState.values = collectTplValues();
      updateTplPreview();
    });
    form.addEventListener("submit", (e) => e.preventDefault());
  }
  // Base-layer (template schema) editor buttons.
  const schemaAdd = $("tpl-schema-add");
  if (schemaAdd) {
    schemaAdd.addEventListener("click", () => {
      const wrap = $("tpl-schema-settings");
      if (!wrap) return;
      const holder = document.createElement("div");
      holder.innerHTML = gsRowHtml(
        { name: "", label: "", description: "", category: "general", type: "text", default: "", current: "", docker: false, options: [] },
        true
      );
      wrap.appendChild(holder.firstElementChild);
      wireGsRow(holder.firstElementChild);
      const nameInput = holder.firstElementChild.querySelector(".gs-name");
      if (nameInput) nameInput.focus();
    });
  }
  const schemaSave = $("tpl-schema-save");
  if (schemaSave) schemaSave.addEventListener("click", onSchemaSave);
}

// Populate (or re-populate) the base-layer schema editor with the given
// schema. Rebuilds the setting rows and re-wires them. The Add/Save buttons
// are wired once in wireTemplates.
function populateSchemaEditor(schemaData) {
  const container = $("tpl-schema-settings");
  if (!container) return;
  const settings = (schemaData && schemaData.settings) || [];
  const categories = {};
  for (const s of settings) {
    const cat = s.category || "general";
    (categories[cat] = categories[cat] || []).push(s);
  }
  container.innerHTML =
    Object.entries(categories)
      .map(
        ([cat, list]) => `
      <h4 class="gs-category">${esc(cat)}</h4>
      ${list.map((s) => gsRowHtml(s, false)).join("")}`
      )
      .join("") || `<p class="muted">${t("tpl.noSettingsDefined")}</p>`;
  container.querySelectorAll(".gs-row").forEach(wireGsRow);
}

// Collect the base-layer schema rows and POST them to the schema endpoint
// (kept separate from the global settings). On success, refresh the schema
// editor and rebuild the template form (which is derived from the schema).
async function onSchemaSave() {
  const errEl = $("tpl-schema-error");
  const saveBtn = $("tpl-schema-save");
  if (errEl) errEl.hidden = true;
  const settings = [];
  const seen = new Set();
  const container = $("tpl-schema-settings");
  if (!container) return;
  for (const row of container.querySelectorAll(".gs-row")) {
    const name = row.querySelector(".gs-name").value.trim();
    if (!name) continue; // blank "add setting" row that was never filled in
    if (!/^[A-Za-z][A-Za-z0-9_]*$/.test(name)) {
      if (errEl) { errEl.textContent = t("tpl.invalidName", { name }); errEl.hidden = false; }
      return;
    }
    if (seen.has(name)) {
      if (errEl) { errEl.textContent = t("tpl.duplicateName", { name }); errEl.hidden = false; }
      return;
    }
    seen.add(name);
    const type = row.querySelector(".gs-type").value;
    const s = {
      name,
      label: row.querySelector(".gs-label").value,
      description: row.querySelector(".gs-description").value,
      category: row.querySelector(".gs-category").value || "general",
      type,
      default: row.querySelector(".gs-default") ? row.querySelector(".gs-default").value : "",
      current: row.querySelector(".gs-current") ? row.querySelector(".gs-current").value : "",
      docker: row.querySelector(".gs-docker").checked,
    };
    if (type === "select") s.options = parseGsOptions(row.querySelector(".gs-options") ? row.querySelector(".gs-options").value : "");
    settings.push(s);
  }
  if (saveBtn) saveBtn.disabled = true;
  try {
    const res = await request("/api/admin/apps/templates/schema", "POST", { settings });
    if (res.error) throw new Error(res.error.message || t("tpl.saveFailed"));
    toast(t("tpl.baseLayerSaved"));
    // The schema underpins the template form, so refresh both the base-layer
    // editor and the (rebuilt) form in place, preserving the panel.
    const tr = await request("/api/admin/apps/templates/schema", "GET");
    if (!tr.error) {
      tplSettings = resolveTemplateSchema(tr.data);
      buildTplForm();
      populateSchemaEditor(tr.data);
      const sel = $("tpl-select");
      if (tplState.values) {
        applyTplValues(tplState.values);
        updateTplPreview();
      } else if (sel) {
        await onTplSelected(sel.value);
      }
    }
  } catch (e) {
    if (errEl) { errEl.textContent = e.message; errEl.hidden = false; }
  } finally {
    if (saveBtn) saveBtn.disabled = false;
  }
}

function tplPanelHtml() {
  const cats = TPL_CATEGORIES.map(([key, titleKey]) => {
    const desc =
      key === "webrtc"
        ? `<p class="muted">${t("tpl.webrtcHelp")}</p>`
        : key === "docker"
        ? `<p class="muted">${t("tpl.dockerHelp")}</p>`
        : "";
    return `<h3 class="tpl-section-title">${t(titleKey)}</h3>${desc}<div class="tpl-form-grid" id="tpl-form-${key}"></div>`;
  }).join("");
  return `
  <div class="card">
    <h3>${t("tpl.baseLayerTitle")}</h3>
    <p class="muted">${t("tpl.baseLayerHelp")}</p>
    <div id="tpl-schema-settings"></div>
    <div style="margin-top:10px;display:flex;gap:8px;flex-wrap:wrap;">
      <button class="btn btn-ghost" id="tpl-schema-add" type="button">${t("tpl.addSetting")}</button>
      <button class="btn btn-primary" id="tpl-schema-save" type="button">${t("tpl.saveBaseLayer")}</button>
    </div>
    <p id="tpl-schema-error" class="error" role="alert" hidden></p>
  </div>
  <div class="card">
    <h3>${t("tpl.appTemplatesTitle")}</h3>
    <p class="muted">${t("tpl.appTemplatesHelp")}</p>
    <div id="tpl-status" class="lab-status" hidden></div>
    <div class="admin-form-row">
      <div class="field"><label for="tpl-select">${t("tpl.template")}</label><select id="tpl-select"><option value="new">${t("tpl.createNew")}</option></select></div>
      <div class="field" id="tpl-name-group"><label for="tpl-name-input">${t("common.name")}</label><input type="text" id="tpl-name-input" maxlength="64" placeholder="${t("tpl.namePlaceholder")}"></div>
      <div class="field tpl-actions">
        <button type="button" class="btn btn-primary" id="tpl-save-btn">${t("common.save")}</button>
        <button type="button" class="btn btn-danger" id="tpl-delete-btn" hidden>${t("common.delete")}</button>
      </div>
    </div>
  </div>
  <div class="card">
    <h3>${t("tpl.uiPreviewTitle")}</h3>
    <div class="tpl-preview-container">
      <div class="tpl-browser-window">
        <div class="tpl-browser-header">
          <span class="tpl-browser-dots"></span>
          <div class="tpl-browser-address-bar"><span id="preview-page-title">Selkies</span></div>
        </div>
        <div class="tpl-browser-content">
          <div id="preview-sidebar" class="tpl-sidebar">
            <div class="tpl-sidebar-header">
              <div id="preview-logo" class="tpl-logo"></div>
              <h2 id="preview-title">Selkies</h2>
            </div>
            <div id="preview-core-buttons" class="tpl-sidebar-section">${t("tpl.previewCoreButtons")}</div>
            <div id="preview-soft-buttons" class="tpl-sidebar-section">${t("tpl.previewSoftButtons")}</div>
            <div id="preview-video-settings" class="tpl-sidebar-section">${t("tpl.previewVideoSettings")}</div>
            <div id="preview-screen-settings" class="tpl-sidebar-section">${t("tpl.previewScreenSettings")}</div>
            <div id="preview-audio-settings" class="tpl-sidebar-section">${t("tpl.previewAudioSettings")}</div>
            <div id="preview-stats" class="tpl-sidebar-section">${t("tpl.previewStats")}</div>
            <div id="preview-clipboard" class="tpl-sidebar-section">${t("tpl.previewClipboard")}</div>
            <div id="preview-files" class="tpl-sidebar-section">${t("tpl.previewFiles")}</div>
            <div id="preview-apps" class="tpl-sidebar-section">${t("tpl.previewApps")}</div>
            <div id="preview-sharing" class="tpl-sidebar-section">${t("tpl.previewSharing")}</div>
            <div id="preview-gamepads" class="tpl-sidebar-section">${t("tpl.previewGamepads")}</div>
          </div>
          <div class="tpl-main-content">
            <p class="muted">${t("tpl.previewContentArea")}</p>
            <div id="preview-keyboard-button" class="tpl-keyboard-button">K</div>
          </div>
        </div>
      </div>
    </div>
  </div>
  <form id="tpl-form" class="tpl-form">
    <div class="card">${cats}</div>
  </form>`;
}

async function renderTemplates() {
  const [schemaRes, tplRes] = await Promise.all([
    request("/api/admin/apps/templates/schema", "GET"),
    request("/api/admin/apps/templates", "GET"),
  ]);
  if (tplRes.error) {
    setPanel(`<div class="card"><h3>${t("tpl.heading")}</h3><div class="error">${esc(tplRes.error.message)}</div></div>`);
    return;
  }
  if (schemaRes.error) {
    setPanel(`<div class="card"><h3>${t("tpl.heading")}</h3><div class="error">${esc(schemaRes.error.message)}</div></div>`);
    return;
  }
  tplSettings = resolveTemplateSchema(schemaRes.data);
  tplList = tplRes.data || [];
  const panelMounted = !!$("admin-section")?.querySelector("#tpl-select");
  if (panelMounted) {
    // Re-render while the editor is still mounted: refresh the dropdown only
    // (the form keeps the user's in-progress edits).
    populateTplDropdowns(tplState.selected);
    return;
  }
  // First mount, or remount after the user visited another section: rebuild
  // the panel and re-wire it (fresh elements, so no duplicate listeners).
  setPanel(tplPanelHtml());
  wireTemplates();
  buildTplForm();
  populateSchemaEditor(schemaRes.data);
  // Restore the in-progress edit (the panel was wiped on section switch).
  const desired = tplState.selected || "new";
  const dirty = tplState.values;
  populateTplDropdowns(desired);
  const sel = $("tpl-select");
  if (sel.value !== desired) {
    // The selected template no longer exists (deleted elsewhere) — fall back
    // to a clean "create new" state.
    tplState.selected = "new";
    tplState.values = null;
    tplState.name = "";
    await onTplSelected("new");
  } else {
    await onTplSelected(sel.value);
    if (dirty) {
      applyTplValues(dirty);
      updateTplPreview();
      tplState.values = dirty;
    }
  }
  const nameInput = $("tpl-name-input");
  if (nameInput && tplState.name) nameInput.value = tplState.name;
}

export { renderTemplates };

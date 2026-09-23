import { secureFetch } from "./api.js";
import { setUserPassword } from "./auth.js";
import { confirmModal, openCustomModal, closeModal } from "./modal.js";
import { arrayBufferToBase64, base64ToArrayBuffer } from "./crypto.js";

function $(id) {
  return document.getElementById(id);
}

function toast(message, kind = "success") {
  const container = $("toasts");
  if (!container) return;
  const el = document.createElement("div");
  el.className = `toast ${kind || ""}`.trim();
  el.textContent = message;
  container.appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

function esc(value) {
  // Escape ALL five HTML-special chars. The div.textContent->innerHTML
  // technique only escapes & < > (text-node context) and leaves " and '
  // literal — which corrupts any value placed inside a quoted attribute
  // (e.g. data-install="{"...") because the first " terminates the attr.
  return String(value == null ? "" : value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

async function request(path, method = "GET", body) {
  const opts = { method };
  if (body !== undefined) opts.body = body;
  try {
    return { data: await secureFetch(path, opts), error: null };
  } catch (e) {
    const status = /status:\s*(\d+)/.exec(String(e.message));
    return { data: null, error: { message: e.message, status: status ? Number(status[1]) : null } };
  }
}

// --- UserSettings form helpers -------------------------------------------
const SETTING_FIELDS = [
  "active",
  "group",
  "persistent_storage",
  "public_sharing",
  "harden_container",
  "harden_openbox",
  "gpu",
  "storage_limit",
  "session_limit",
];

function defaultSettings(groupOptions = []) {
  return {
    active: true,
    group: "none",
    persistent_storage: true,
    public_sharing: false,
    harden_container: false,
    harden_openbox: false,
    gpu: true,
    storage_limit: -1,
    session_limit: -1,
  };
}

function settingsFormHtml(settings, groupOptions, formId) {
  // Merge over the defaults so a partial (or empty) settings object still
  // renders every field with a sensible value instead of `undefined`.
  const g = { ...defaultSettings(groupOptions), ...(settings || {}) };
  const groupOpts = ['<option value="none">none</option>'];
  (groupOptions || []).forEach((name) => {
    const sel = g.group === name ? " selected" : "";
    groupOpts.push(`<option value="${esc(name)}"${sel}>${esc(name)}</option>`);
  });
  if (g.group && g.group !== "none" && !(groupOptions || []).includes(g.group)) {
    groupOpts.push(`<option value="${esc(g.group)}" selected>${esc(g.group)}</option>`);
  }
  return `
    <div class="admin-settings-form" id="${formId}">
      <label class="check"><input type="checkbox" name="active" ${g.active ? "checked" : ""}> Active</label>
      <div class="field"><label>Group</label><select name="group">${groupOpts.join("")}</select></div>
      <label class="check"><input type="checkbox" name="persistent_storage" ${g.persistent_storage ? "checked" : ""}> Persistent storage</label>
      <label class="check"><input type="checkbox" name="public_sharing" ${g.public_sharing ? "checked" : ""}> Public sharing</label>
      <label class="check"><input type="checkbox" name="harden_container" ${g.harden_container ? "checked" : ""}> Harden container</label>
      <label class="check"><input type="checkbox" name="harden_openbox" ${g.harden_openbox ? "checked" : ""}> Harden Openbox</label>
      <label class="check"><input type="checkbox" name="gpu" ${g.gpu ? "checked" : ""}> GPU</label>
      <div class="field"><label>Storage limit (MB, -1 = unlimited)</label><input type="number" name="storage_limit" value="${g.storage_limit}"></div>
      <div class="field"><label>Session limit (-1 = unlimited)</label><input type="number" name="session_limit" value="${g.session_limit}"></div>
    </div>`;
}

function readSettingsForm(formId) {
  const form = $(formId);
  const out = {};
  SETTING_FIELDS.forEach((f) => {
    const el = form.querySelector(`[name="${f}"]`);
    if (el.type === "checkbox") out[f] = el.checked;
    else if (el.tagName === "SELECT") out[f] = el.value;
    else if (f === "storage_limit" || f === "session_limit") {
      const v = Number(el.value);
      out[f] = Number.isFinite(v) ? v : -1;
    } else out[f] = el.value;
  });
  return out;
}

// --- Permission catalog (for the role / access / group editors) -----------
// Fetched once from GET /api/admin/permissions: { admin: [{name,description}],
// user: [...] }. Cached so the various editors don't re-fetch on every open.
let permCatalog = null;
async function loadPermCatalog() {
  if (permCatalog) return permCatalog;
  const res = await request("/api/admin/permissions", "GET");
  if (res.error) return null;
  permCatalog = res.data || { admin: [], user: [] };
  return permCatalog;
}

// Render the permission checkboxes for a form. `selected` is the list of
// permission names that should start checked; `name` is the input name used
// to group the checkboxes ("perm" for permissions, "role" for roles). Returns
// the HTML.
function permCheckboxesHtml(selected = [], name = "perm") {
  const sel = new Set(selected || []);
  const group = (title, perms) =>
    perms.length
      ? `<div class="perm-group"><div class="perm-group-label">${title}</div>${perms
          .map(
            (p) => `<label class="check perm-check" title="${esc(p.description)}"><input type="checkbox" name="${name}" value="${esc(p.name)}" ${sel.has(p.name) ? "checked" : ""}> ${esc(p.name)}</label>`
          )
          .join("")}</div>`
      : "";
  return (
    group("Admin permissions", (permCatalog && permCatalog.admin) || []) +
    group("User permissions", (permCatalog && permCatalog.user) || [])
  );
}

// Read the checked values of the permission/role checkboxes (by input name)
// from a form element.
function readPermCheckboxes(form, name = "perm") {
  return Array.from(form.querySelectorAll(`input[name="${name}"]:checked`)).map(
    (el) => el.value
  );
}

// Render the role checkboxes for a form, sourced from the roles list (not the
// permission catalog, which has no roles). `selected` is the list of role
// names that should start checked.
function roleCheckboxesHtml(selected = [], roles = []) {
  const sel = new Set(selected || []);
  return (roles || [])
    .map(
      (r) => `<label class="check perm-check"><input type="checkbox" name="role" value="${esc(r.name)}" ${sel.has(r.name) ? "checked" : ""}> ${esc(r.name)}${r.is_builtin ? " (built-in)" : ""}</label>`
    )
    .join("") || '<p class="muted">No roles defined.</p>';
}

// --- Admin state ----------------------------------------------------------
let currentSection = "overview";
let lastGroups = [];

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

async function render() {
  const panel = $("admin-section");
  if (!panel) return;
  // The Laboratory panel hosts a live iframe session. When the lab panel is
  // already mounted, keep the DOM (and the live iframe) and just refresh the
  // dropdowns in place; otherwise render the section fresh.
  const labPanelMounted = !!panel.querySelector("#lab-app-select");
  if (currentSection === "laboratory" && labPanelMounted) {
    await renderLaboratory();
    return;
  }
  // The template editor keeps in-progress edits in tplState; when its panel
  // is already mounted, refresh in place instead of wiping the form.
  const tplPanelMounted = !!panel.querySelector("#tpl-select");
  if (currentSection === "templates" && tplPanelMounted) {
    await renderTemplates();
    return;
  }
  panel.innerHTML = '<div class="muted">Loading…</div>';
  await SECTION_RENDERERS[currentSection]();
}

// --- How to Use guide -----------------------------------------------------
const HOWTO_ITEMS = [
  ["Launch an app", "Open the Apps tab, pick an application, set your launch options (home directory, language, GPU, …) and click Launch. Your isolated session opens in a new tab."],
  ["Open a file in an app", "In the Files tab, select a file and choose “Open in app” to launch it directly in the application of your choice."],
  ["Manage persistent storage", "Create and delete home directories (from the launch options or the Files tab) to keep your files across sessions."],
  ["Save launch options", "Tick “Save these launch options” in the launch dialog to pin a preset for a file type or for all URLs. Manage presets under Pinned."],
  ["Admin tasks", "In the Admin tab you can manage users, admins, groups, apps, templates, volume mounts and backups."],
];

function howToGuideHtml() {
  const items = HOWTO_ITEMS.map(
    ([lead, body]) => `<li><strong>${esc(lead)}:</strong> ${body}</li>`
  ).join("");
  return `
    <p class="muted">A quick guide to the main things you can do in Vreckan.</p>
    <ul class="howto-list">${items}</ul>
    <div class="modal-actions">
      <button type="button" class="btn btn-primary" data-act="howto-close">Close</button>
    </div>`;
}

function openHowToGuide() {
  const { box } = openCustomModal({ title: "How to Use Vreckan", body: howToGuideHtml() });
  const btn = box.querySelector('[data-act="howto-close"]');
  if (btn) btn.addEventListener("click", () => closeModal());
}

// --- Overview -------------------------------------------------------------
async function renderOverview() {
  const [status, data] = await Promise.all([
    request("/api/admin/status", "POST", {}),
    request("/api/admin/data", "POST", {}),
  ]);

  const s = (status.data && status.data.settings) || {};
  const gpus = (status.data && status.data.gpus) || [];
  const kv = [
    ["Username", status.data && status.data.username],
    ["Is admin", status.data && status.data.is_admin ? "yes" : "no"],
    ["CPU", status.data && status.data.cpu_model],
    ["Disk total", status.data && status.data.disk_total != null ? `${(status.data.disk_total / 1048576).toFixed(1)} MB` : "—"],
    ["Disk used", status.data && status.data.disk_used != null ? `${(status.data.disk_used / 1048576).toFixed(1)} MB` : "—"],
    ["GPUs", gpus.length ? gpus.map((g) => g.device || "GPU").join(", ") : "none"],
    ["API port", data.data && data.data.api_port],
    ["Session port", data.data && data.data.session_port],
  ];

  const kvHtml = kv
    .map(
      ([label, value]) => `
      <div class="kv-item">
        <div class="kv-label">${esc(label)}</div>
        <div class="kv-value">${value == null || value === "" ? '<span class="muted">—</span>' : esc(String(value))}</div>
      </div>`
    )
    .join("");

  const errors = [status.error, data.error].filter(Boolean);
  let errorHtml = "";
  if (errors.length) {
    errorHtml = `<div class="error">${errors.map((e) => esc(e.message)).join(" · ")}</div>`;
  }

  setPanel(`
    <div class="card">
      <div style="display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap;">
        <h3>Overview</h3>
        <button class="btn btn-ghost" id="howto-btn" type="button">How to Use Vreckan</button>
      </div>
      <p class="muted">Server status and configuration.</p>
      ${errorHtml}
      <div class="kv-grid" style="margin-top:14px;">${kvHtml}</div>
    </div>
    <div class="card">
      <h3>My settings</h3>
      ${settingsFormHtml(s, lastGroups, "overview-settings")}
    </div>
  `);

  const howtoBtn = $("howto-btn");
  if (howtoBtn) howtoBtn.addEventListener("click", () => openHowToGuide());
}

// --- Global settings -------------------------------------------------------
// The truly global (non-template) settings: just the global default GPU.
// The template schema (the base-layer default for every setting) is the
// "template settings" and is edited on the Templates page, not here.
async function renderGlobalSettings() {
  const res = await request("/api/admin/global-settings", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>Global Settings</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const data = res.data || {};
  const gpus = data.gpus || [];
  const globalGpu = data.global_default_gpu || "";

  const gpuOptions =
    '<option value="">None (no global default)</option>' +
    gpus
      .map(
        (g) =>
          `<option value="${esc(g.device)}" ${globalGpu === g.device ? "selected" : ""}>${esc(g.device)}${g.driver ? ` (${esc(g.driver)})` : ""}</option>`
      )
      .join("");

  setPanel(`
    <div class="card">
      <h3>Global Settings</h3>
      <p class="muted">Server-wide settings that are not part of the app-template schema. The per-setting base-layer defaults (the template schema) are edited on the <strong>Templates</strong> page, and the SSO admin groups are managed on the <strong>Groups</strong> page.</p>
    </div>
    <div class="card">
      <h3>Global default GPU</h3>
      <p class="muted">Fallback GPU for launches where the user doesn't pick one. Apps that don't support the GPU type are unaffected.</p>
      <div class="field">
        <label for="gs-gpu-select">GPU</label>
        <select id="gs-gpu-select">${gpuOptions}</select>
      </div>
      <p id="gs-gpu-error" class="error" role="alert" hidden></p>
      <div style="margin-top:10px;display:flex;gap:8px;flex-wrap:wrap;">
        <button class="btn btn-primary" id="gs-save" type="button">Save global settings</button>
      </div>
    </div>
  `);

  const saveBtn = $("gs-save");
  if (saveBtn) saveBtn.addEventListener("click", onGlobalSettingsSave);
}

// --- SSO / OIDC -----------------------------------------------------------
// A small tag input: chips for the current values, a text box to add more
// (Enter or comma), and an × on each chip to remove it. Used for the SSO
// admin/user group lists.
function tagInputHtml(id, values) {
  const chips = (values || [])
    .map((v) => `<span class="tag-chip" data-name="${esc(v)}">${esc(v)}<button type="button" class="tag-remove" title="Remove">×</button></span>`)
    .join("");
  return `<div class="tag-input" id="${id}">${chips}<input type="text" placeholder="type a name, press Enter"></div>`;
}

function wireTagInput(id) {
  const box = $(id);
  if (!box) return;
  const input = box.querySelector("input");
  const add = (raw) => {
    const v = raw.trim().replace(/,+$/, "");
    if (!v) return;
    if (Array.from(box.querySelectorAll(".tag-chip")).some((c) => c.dataset.name === v)) return;
    const chip = document.createElement("span");
    chip.className = "tag-chip";
    chip.dataset.name = v;
    chip.innerHTML = `${esc(v)}<button type="button" class="tag-remove" title="Remove">×</button>`;
    box.insertBefore(chip, input);
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      add(input.value);
      input.value = "";
    } else if (e.key === "Backspace" && !input.value) {
      const chips = box.querySelectorAll(".tag-chip");
      if (chips.length) chips[chips.length - 1].remove();
    }
  });
  box.addEventListener("click", (e) => {
    if (e.target.classList.contains("tag-remove")) e.target.closest(".tag-chip").remove();
  });
}

function readTagInput(id) {
  const box = $(id);
  if (!box) return [];
  return Array.from(box.querySelectorAll(".tag-chip")).map((c) => c.dataset.name);
}

// A small source badge showing where a value currently comes from.
function ssoSourceBadge(source) {
  const label = source === "db" ? "db" : source === "env" ? "env" : "default";
  return `<span class="src-badge${source === "db" ? " db" : ""}">${label}</span>`;
}

async function renderSso() {
  const res = await request("/api/admin/sso", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>SSO</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const v = res.data.values || {};
  const src = res.data.sources || {};
  const badge = (name) => ssoSourceBadge(src[name] || "default");

  setPanel(`
    <div class="card">
      <h3>SSO / OIDC</h3>
      <p class="muted">Connect Vreckan to an OpenID Connect provider (e.g. authentik). Discovery is fetched from
        <code>{issuer}/.well-known/openid-configuration</code>. A value saved here overrides the
        <code>VRECKAN_OIDC_*</code> environment; the badge on each field shows its current source.</p>

      <div class="field" style="margin-top:12px;">
        <label>Enable SSO login ${badge("oidc_enabled")}</label>
        <label class="check"><input type="checkbox" id="sso-enabled" ${v.oidc_enabled ? "checked" : ""}> Enabled</label>
      </div>

      <div class="field" style="margin-top:12px;">
        <label>Issuer URL ${badge("oidc_issuer")}</label>
        <input type="text" id="sso-issuer" value="${esc(v.oidc_issuer || "")}">
        <p class="muted" style="margin-top:4px;">Base URL of the provider.</p>
      </div>

      <div class="admin-form-row" style="margin-top:12px;">
        <div class="field" style="flex:1;min-width:220px;">
          <label>Client ID ${badge("oidc_client_id")}</label>
          <input type="text" id="sso-client-id" value="${esc(v.oidc_client_id || "")}">
        </div>
        <div class="field" style="flex:1;min-width:220px;">
          <label>Client secret ${badge("oidc_client_secret")}</label>
          <input type="password" id="sso-client-secret" value="" placeholder="${v.secret_set ? `•••• (set, ${v.secret_length} chars — leave blank to keep)` : "(optional for public clients)"}">
        </div>
      </div>

      <div class="admin-form-row" style="margin-top:12px;">
        <div class="field" style="flex:1;min-width:220px;">
          <label>Redirect URI ${badge("oidc_redirect_uri")}</label>
          <input type="text" id="sso-redirect-uri" value="${esc(v.oidc_redirect_uri || "")}" placeholder="default: {base}/api/auth/oidc/callback">
        </div>
        <div class="field" style="flex:1;min-width:220px;">
          <label>Scopes ${badge("oidc_scopes")}</label>
          <input type="text" id="sso-scopes" value="${esc(v.oidc_scopes || "")}">
        </div>
      </div>

      <div class="admin-form-row" style="margin-top:12px;">
        <div class="field" style="flex:1;min-width:220px;">
          <label>Auto-provision new users ${badge("oidc_allow_signup")}</label>
          <label class="check"><input type="checkbox" id="sso-allow-signup" ${v.oidc_allow_signup ? "checked" : ""}> Create an account for a new SSO identity</label>
        </div>
        <div class="field" style="flex:1;min-width:220px;">
          <label>State TTL (seconds) ${badge("oidc_state_ttl_seconds")}</label>
          <input type="number" id="sso-state-ttl" value="${v.oidc_state_ttl_seconds ?? 600}">
        </div>
      </div>

      <div style="margin-top:14px;display:flex;gap:8px;flex-wrap:wrap;align-items:center;">
        <button class="btn btn-ghost" id="sso-test" type="button">Test connection</button>
        <span class="test-result" id="sso-test-result"></span>
      </div>
    </div>

    <div class="card">
      <h3>Group mapping</h3>
      <p class="muted">On each SSO login a user is joined to a Vreckan group for every provider group they're in.
        Only the groups listed below get roles auto-assigned; any other (newly observed) group starts with
        <strong>no permissions</strong> until you grant them on the <strong>Groups</strong> page.</p>

      <div class="field" style="margin-top:12px;">
        <label>Admin groups ${badge("oidc_admin_groups")}</label>
        ${tagInputHtml("sso-admin-groups", v.oidc_admin_groups || [])}
        <p class="muted" style="margin-top:4px;">Members are auto-promoted to Vreckan admin (grants the <code>admin</code> role).</p>
      </div>

      <div class="field" style="margin-top:12px;">
        <label>User groups ${badge("oidc_user_groups")}</label>
        ${tagInputHtml("sso-user-groups", v.oidc_user_groups || [])}
        <p class="muted" style="margin-top:4px;">Members get the built-in <code>user</code> role. The first of these a user belongs to becomes their primary group.</p>
      </div>
    </div>

    <div style="margin-top:14px;display:flex;gap:8px;justify-content:flex-end;">
      <button class="btn btn-primary" id="sso-save" type="button">Save SSO settings</button>
    </div>
  `);

  wireTagInput("sso-admin-groups");
  wireTagInput("sso-user-groups");

  $("sso-test").addEventListener("click", async () => {
    const out = $("sso-test-result");
    out.className = "test-result";
    out.textContent = "Contacting the provider…";
    const r = await request("/api/admin/sso/test", "POST", {});
    if (r.error) {
      out.classList.add("err");
      out.textContent = "✗ " + r.error.message;
      return;
    }
    out.classList.add("ok");
    out.innerHTML = `✓ Connected to <strong>${esc(r.data.issuer || "")}</strong>
      <span class="muted">authorization: ${esc(r.data.authorization_endpoint || "—")} · userinfo: ${esc(r.data.userinfo_endpoint || "—")} · token: ${esc(r.data.token_endpoint || "—")}</span>`;
  });

  $("sso-save").addEventListener("click", async () => {
    const payload = {
      oidc_enabled: $("sso-enabled").checked,
      oidc_issuer: $("sso-issuer").value,
      oidc_client_id: $("sso-client-id").value,
      oidc_client_secret: $("sso-client-secret").value,
      oidc_redirect_uri: $("sso-redirect-uri").value,
      oidc_scopes: $("sso-scopes").value,
      oidc_allow_signup: $("sso-allow-signup").checked,
      oidc_state_ttl_seconds: Number($("sso-state-ttl").value) || 600,
      oidc_admin_groups: readTagInput("sso-admin-groups"),
      oidc_user_groups: readTagInput("sso-user-groups"),
    };
    const r = await request("/api/admin/sso", "PUT", payload);
    if (r.error) return toast(r.error.message, "error");
    toast("SSO settings saved");
    render();
  });
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
  const defaultHtml = `<input class="gs-default" type="text" value="${esc(def)}" placeholder="image default" ${s.builtin ? "readonly" : ""}>`;
  let currentHtml;
  if (type === "boolean") {
    currentHtml = `<select class="gs-current">
       <option value="" ${cur === "" ? "selected" : ""}>use default</option>
       <option value="true" ${cur === "true" ? "selected" : ""}>true</option>
       <option value="false" ${cur === "false" ? "selected" : ""}>false</option>
     </select>`;
  } else if (type === "select") {
    const opts = (s.options || []).slice();
    if (cur && !opts.some((o) => o.value === cur)) opts.push({ value: cur, label: `${cur} (current)` });
    currentHtml = `<select class="gs-current">
       <option value="" ${cur === "" ? "selected" : ""}>use default</option>
       ${opts.map((o) => `<option value="${esc(o.value)}" ${cur === o.value ? "selected" : ""}>${esc(o.label || o.value)}</option>`).join("")}
     </select>`;
  } else {
    currentHtml = `<input class="gs-current" type="text" value="${esc(cur)}" placeholder="use default">`;
  }
  const optionsHtml =
    type === "select"
      ? `<div class="gs-row-options"><textarea class="gs-options" rows="2" placeholder="Options, one per line: value:label">${esc((s.options || []).map((o) => `${o.value}:${o.label || o.value}`).join("\n"))}</textarea></div>`
      : "";
  return `
  <div class="gs-row" data-name="${esc(s.name || "")}" data-builtin="${s.builtin ? "1" : "0"}">
    <div class="gs-row-top">
      <input class="gs-name" type="text" value="${esc(s.name || "")}" placeholder="ENV_VAR_NAME" ${isNew ? "" : "readonly"}>
      <button class="btn btn-sm btn-danger gs-remove" type="button">Remove</button>
    </div>
    <div class="gs-row-fields">
      <input class="gs-label" type="text" value="${esc(s.label || "")}" placeholder="Label">
      <input class="gs-description" type="text" value="${esc(s.description || "")}" placeholder="Description">
      <input class="gs-category" type="text" value="${esc(s.category || "")}" placeholder="Category" style="max-width:130px;">
      <select class="gs-type">
        <option value="text" ${type === "text" ? "selected" : ""}>text</option>
        <option value="boolean" ${type === "boolean" ? "selected" : ""}>boolean</option>
        <option value="select" ${type === "select" ? "selected" : ""}>select</option>
      </select>
      <label class="gs-docker-label"><input type="checkbox" class="gs-docker" ${s.docker ? "checked" : ""}> docker</label>
    </div>
    <div class="gs-row-values">
      <div class="field">
        <label>Default (image baseline)</label>
        ${defaultHtml}
      </div>
      <div class="field">
        <label>Current (base layer)</label>
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
      if (name) toast(`Removed '${name}' — press "Save base layer" to apply.`);
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

// Save the global (non-template) settings — just the global default GPU.
// The template schema (base-layer defaults) is saved from the Templates page.
async function onGlobalSettingsSave() {
  const errEl = $("gs-gpu-error");
  const saveBtn = $("gs-save");
  if (errEl) errEl.hidden = true;
  if (saveBtn) saveBtn.disabled = true;
  try {
    const res = await request("/api/admin/global-settings", "POST", {
      global_default_gpu: $("gs-gpu-select").value,
    });
    if (res.error) throw new Error(res.error.message || "Failed to save");
    toast("Global settings saved.");
    await renderGlobalSettings();
  } catch (e) {
    if (errEl) { errEl.textContent = e.message; errEl.hidden = false; }
  } finally {
    const b = $("gs-save");
    if (b) b.disabled = false;
  }
}

// --- User / Admin / Group roster -----------------------------------------
async function fetchData() {
  const res = await request("/api/admin/data", "POST", {});
  if (res.error) return res;
  const d = res.data || {};
  lastGroups = (d.groups || []).map((g) => g.name);
  return {
    users: d.users || [],
    admins: d.admins || [],
    groups: d.groups || [],
    volumeMounts: d.volume_mounts || [],
    data: d,
  };
}

function userCard(user, homedirs, adminGroups = []) {
  const g = user.settings || {};
  const chip = (ok, label) => `<span class="badge ${ok ? "badge-ok" : "badge-warn"}">${label}</span>`;
  const homeText = homedirs && homedirs.length ? homedirs.map(esc).join(", ") : "—";
  const sso = !!user.is_sso;
  // SSO groups: the provider (IdP) groups the user belongs to, captured on
  // their most recent SSO login. Groups that are in the admin-groups list are
  // what make the user an admin, so they are highlighted.
  const ssoGroups = user.sso_groups || [];
  const ssoBadges = ssoGroups
    .map(
      (grp) =>
        `<span class="badge${adminGroups.includes(grp) ? " badge-ok" : ""}" title="SSO group${adminGroups.includes(grp) ? " (grants admin)" : ""}">SSO: ${esc(grp)}${adminGroups.includes(grp) ? " · admin" : ""}</span>`
    )
    .join(" ");
  const isAdmin = !!user.is_admin;
  // SSO users authenticate through the IdP — their password is not local, so
  // they get an SSO badge instead of a Set password control.
  const setPwBtn = sso
    ? ""
    : `<button class="btn btn-sm btn-ghost" data-action="setpw" data-username="${esc(user.username)}">Set password</button>`;
  const pwChip = sso ? "" : user.has_password ? chip(true, "password set") : '<span class="badge">no password</span>';
  // The unified roster lists everyone; the admin badge marks admins. The
  // is_admin flag is changed from the Edit form (an in-place Admin checkbox),
  // not from a button here. data-username lets the Edit action open its form
  // in this card's place.
  return `
    <div class="card" data-username="${esc(user.username)}">
      <div style="display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap;">
        <div>
          <strong>${esc(user.username)}</strong>
          ${isAdmin ? '<span class="badge badge-ok" style="margin-left:8px;">admin</span>' : ""}
          ${sso ? '<span class="badge" style="margin-left:8px;">SSO</span>' : ""}
        </div>
        <div class="admin-actions">
          <button class="btn btn-sm btn-ghost" data-action="edit" data-username="${esc(user.username)}">Edit</button>
          <button class="btn btn-sm btn-ghost" data-action="access" data-username="${esc(user.username)}">Access</button>
          ${setPwBtn}
          <button class="btn btn-sm btn-ghost" data-action="homedirs" data-username="${esc(user.username)}">Home dirs</button>
          <button class="btn btn-sm btn-danger" data-action="delete" data-username="${esc(user.username)}">Delete</button>
        </div>
      </div>
      <div style="margin-top:10px;display:flex;gap:6px;flex-wrap:wrap;">
        ${chip(g.active, g.active ? "active" : "inactive")}
        ${pwChip}
        ${chip(g.persistent_storage, g.persistent_storage ? "storage" : "no storage")}
        ${chip(g.public_sharing, g.public_sharing ? "sharing" : "no sharing")}
        ${chip(g.gpu, g.gpu ? "gpu" : "no gpu")}
        <span class="badge">group: ${esc(g.group || "none")}</span>
        ${(user.roles || []).length ? `<span class="badge">roles: ${esc((user.roles || []).join(", "))}</span>` : ""}
        <span class="badge">storage: ${g.storage_limit == null || g.storage_limit < 0 ? "∞" : g.storage_limit + "MB"}</span>
        <span class="badge">sessions: ${g.session_limit == null || g.session_limit < 0 ? "∞" : g.session_limit}</span>
        ${ssoBadges}
      </div>
      <div class="muted" style="margin-top:8px;font-size:0.85rem;">Home dirs: ${homeText}</div>
    </div>`;
}

async function renderRoster() {
  const res = await fetchData();
  if (res.error) {
    setPanel(`<div class="card"><h3>Accounts</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  // The roster is unified: every account (admins and users) appears in one
  // list, admins first. The is_admin flag is what distinguishes them, and it
  // can be flipped in place via the toggle on each card.
  const list = [...res.admins, ...res.users];
  // Fetch the configured SSO admin-groups list so each card can highlight
  // the SSO groups that grant admin (best-effort: a failure just means no
  // highlighting). The list is read-only (sourced from the environment).
  const agRes = await request("/api/admin/sso-groups", "GET");
  const adminGroups = (agRes.data && agRes.data.admin_groups) || [];
  const cards = list.map((u) => userCard(u, u.homedirs, adminGroups)).join("");
  setPanel(`
    <div class="card">
      <h3>Add account</h3>
      <p class="muted">Create a new account. Tick "Admin" to make it an administrator.</p>
      <div class="admin-form-row" style="margin-bottom:12px;">
        <div class="field"><label>Username</label><input type="text" id="roster-new-username" autocomplete="off"></div>
        <div class="field"><label class="check"><input type="checkbox" id="roster-new-isadmin"><span>Admin</span></label></div>
      </div>
      ${settingsFormHtml(defaultSettings(lastGroups), lastGroups, "roster-new-settings")}
      <div style="margin-top:12px;"><button class="btn btn-primary" id="roster-create">Create</button></div>
      <div id="roster-result"></div>
    </div>
    <div class="section-title">Accounts</div>
    <div id="roster-cards">${cards}</div>
    <div id="roster-modal"></div>
  `);
  wireRoster();
}

async function wireRoster() {
  // The roster is unified: one list of every account, and the `is_admin` flag
  // in the create payload (from the "Admin" checkbox) decides the account kind.
  const basePath = "/api/admin/people";
  const homeBase = "/api/admin/people";

  const createBtn = $("roster-create");
  if (createBtn) {
    createBtn.addEventListener("click", async () => {
      const username = $("roster-new-username").value.trim();
      if (!username) return toast("Username is required", "error");
      const settings = readSettingsForm("roster-new-settings");
      const payload = { username, settings, is_admin: !!$("roster-new-isadmin").checked };
      const res = await request(basePath, "POST", payload);
      const result = $("roster-result");
      if (res.error) {
        result.innerHTML = `<div class="error" style="margin-top:10px;">${esc(res.error.message)}</div>`;
        return;
      }
      toast("Created");
      setTimeout(render, 300);
    });
  }

  const cards = $("roster-cards");
  if (cards) {
    cards.addEventListener("click", async (e) => {
      const btn = e.target.closest("button[data-action]");
      if (!btn) return;
      const username = btn.dataset.username;
      const action = btn.dataset.action;

      if (action === "delete") {
        if (!(await confirmModal(`Delete ${username}?`, { title: "Delete user", confirmLabel: "Delete", danger: true }))) return;
        const res = await request(`${basePath}/${encodeURIComponent(username)}`, "DELETE");
        if (res.error) return toast(res.error.message, "error");
        toast("Deleted");
        render();
        return;
      }

      if (action === "homedirs") {
        const res = await request(`${homeBase}/${encodeURIComponent(username)}/homedirs`, "GET");
        if (res.error) return toast(res.error.message, "error");
        const dirs = (res.data && res.data.home_dirs) || [];
        const listHtml = dirs.length
          ? dirs
              .map(
                (d) => `
                <div class="share-row">
                  <code>${esc(d)}</code>
                  <button class="btn btn-sm btn-danger" data-rmhome="${esc(d)}">Remove</button>
                </div>`
              )
              .join("")
          : `<p class="muted">No home directories.</p>`;
        $("roster-modal").innerHTML = `
          <div class="card" style="margin-top:16px;">
            <h3>Home directories — ${esc(username)}</h3>
            <div id="home-list">${listHtml}</div>
            <div class="admin-form-row" style="margin-top:10px;">
              <div class="field"><label>New home dir</label><input type="text" id="new-home" placeholder="e.g. games"></div>
              <button class="btn btn-primary" id="add-home">Add</button>
              <button class="btn btn-ghost" id="close-modal">Close</button>
            </div>
          </div>`;
        $("close-modal").addEventListener("click", () => ($("roster-modal").innerHTML = ""));
        $("add-home").addEventListener("click", async () => {
          const name = $("new-home").value.trim();
          if (!name) return toast("Name required", "error");
          const r = await request(`${homeBase}/${encodeURIComponent(username)}/homedirs`, "POST", { home_name: name });
          if (r.error) return toast(r.error.message, "error");
          toast("Added");
          render();
        });
        $("home-list").addEventListener("click", async (e) => {
          const btn = e.target.closest("button[data-rmhome]");
          if (!btn) return;
          if (!(await confirmModal("Remove this home directory?", { confirmLabel: "Remove", danger: true }))) return;
          const r = await request(`${homeBase}/${encodeURIComponent(username)}/homedirs/${encodeURIComponent(btn.dataset.rmhome)}`, "DELETE");
          if (r.error) return toast(r.error.message, "error");
          toast("Removed");
          render();
        });
        return;
      }

      if (action === "access") {
        const d = await fetchData();
        const target = (d.users.concat(d.admins)).find((u) => u.username === username);
        if (!target) return toast("Not found", "error");
        await loadPermCatalog();
        const roles = target.roles || [];
        const perms = target.permissions || [];
        const groups = target.groups || [];
        // Role options come from the management data (all roles), not the catalog.
        const allRoles = (d.data && d.data.roles) || [];
        const roleChecks = allRoles
          .map(
            (r) => `<label class="check"><input type="checkbox" name="role" value="${esc(r.name)}" ${roles.includes(r.name) ? "checked" : ""}> ${esc(r.name)}${r.is_builtin ? " (built-in)" : ""}</label>`
          )
          .join("");
        const groupChecks = lastGroups
          .map(
            (gn) => `<label class="check"><input type="checkbox" name="group" value="${esc(gn)}" ${groups.includes(gn) ? "checked" : ""}> ${esc(gn)}</label>`
          )
          .join("") || '<p class="muted">No groups defined.</p>';
        $("roster-modal").innerHTML = `
          <div class="card" style="margin-top:16px;">
            <h3>Access — ${esc(username)}</h3>
            <p class="muted">Assign roles, direct permissions, and group memberships. The account's effective access is the union of all three (the most permissive wins).</p>
            <div class="field" style="margin-bottom:10px;"><label>Roles</label>${roleChecks}</div>
            <div class="field" style="margin-bottom:10px;"><label>Direct permissions</label><div id="access-perms">${permCheckboxesHtml(perms)}</div></div>
            <div class="field" style="margin-bottom:10px;"><label>Groups</label>${groupChecks}</div>
            <div id="access-result"></div>
            <div style="margin-top:12px;">
              <button class="btn btn-primary" id="access-save">Save</button>
              <button class="btn btn-ghost" id="close-modal">Cancel</button>
            </div>
          </div>`;
        $("close-modal").addEventListener("click", () => ($("roster-modal").innerHTML = ""));
        $("access-save").addEventListener("click", async () => {
          const form = $("roster-modal");
          const newRoles = Array.from(form.querySelectorAll('input[name="role"]:checked')).map((el) => el.value);
          const newPerms = readPermCheckboxes($("access-perms"));
          const newGroups = Array.from(form.querySelectorAll('input[name="group"]:checked')).map((el) => el.value);
          const res = await request(`${basePath}/${encodeURIComponent(username)}/access`, "POST", { roles: newRoles, permissions: newPerms, groups: newGroups });
          if (res.error) {
            $("access-result").innerHTML = `<div class="error" style="margin-top:10px;">${esc(res.error.message)}</div>`;
            return;
          }
          toast("Access saved");
          render();
        });
        return;
      }

      if (action === "setpw") {
        const d = await fetchData();
        const target = (d.users.concat(d.admins)).find((u) => u.username === username);
        if (!target) return toast("Not found", "error");
        const hasPw = !!target.has_password;
        $("roster-modal").innerHTML = `
          <div class="card" style="margin-top:16px;">
            <h3>${hasPw ? "Reset password" : "Set password"} — ${esc(username)}</h3>
            <p class="muted">Sets the web-login password for ${esc(username)}. It takes effect immediately — they can use it to sign in to the web UI.</p>
            <div class="admin-form-row">
              <div class="field"><label>New password</label><input type="password" id="setpw-new" minlength="8" autocomplete="new-password" placeholder="At least 8 characters"></div>
              <div class="field"><label>Confirm password</label><input type="password" id="setpw-confirm" minlength="8" autocomplete="new-password"></div>
            </div>
            <div id="setpw-result"></div>
            <div style="margin-top:12px;">
              <button class="btn btn-primary" id="setpw-save">Save password</button>
              <button class="btn btn-ghost" id="close-modal">Cancel</button>
            </div>
          </div>`;
        $("close-modal").addEventListener("click", () => ($("roster-modal").innerHTML = ""));
        $("setpw-save").addEventListener("click", async () => {
          const pw = $("setpw-new").value;
          if (pw.length < 8) return toast("Password must be at least 8 characters", "error");
          if (pw !== $("setpw-confirm").value) return toast("Passwords do not match", "error");
          try {
            await setUserPassword(username, pw);
          } catch (e) {
            $("setpw-result").innerHTML = `<div class="error" style="margin-top:10px;">${esc(e.message)}</div>`;
            return;
          }
          $("roster-modal").innerHTML = "";
          toast(`Password ${hasPw ? "reset" : "set"} for ${username}`);
          render();
        });
        $("setpw-new").focus();
        return;
      }

      if (action === "edit") {
        const d = await fetchData();
        const target = (d.users.concat(d.admins)).find((u) => u.username === username);
        if (!target) return toast("Not found", "error");
        // Open the edit form in place: it replaces this account's card so the
        // "Edit <name>" form occupies the same space the card did.
        const card = cards.querySelector(`.card[data-username="${username}"]`);
        if (!card) return toast("Not found", "error");
        // The Admin checkbox lives in the edit form (is_admin is not part of
        // the settings payload). The bootstrap 'admin' account can't be
        // demoted, so its checkbox is locked on.
        const isRootAdmin = username === "admin";
        const adminField = isRootAdmin
          ? `<label class="check" title="The bootstrap admin account cannot be demoted."><input type="checkbox" id="edit-isadmin" checked disabled> Admin</label>`
          : `<label class="check"><input type="checkbox" id="edit-isadmin" ${target.is_admin ? "checked" : ""}> Admin</label>`;
        card.innerHTML = `
          <h3>Edit ${esc(username)}</h3>
          <div class="admin-form-row" style="margin-bottom:12px;">
            <div class="field"><label>Username</label><input type="text" value="${esc(username)}" disabled></div>
            <div class="field">${adminField}</div>
          </div>
          ${settingsFormHtml(target.settings || defaultSettings(lastGroups), lastGroups, "edit-settings")}
          <div id="edit-result"></div>
          <div style="margin-top:12px;">
            <button class="btn btn-primary" id="save-edit">Save</button>
            <button class="btn btn-ghost" id="cancel-edit">Cancel</button>
          </div>`;
        $("cancel-edit").addEventListener("click", () => render());
        $("save-edit").addEventListener("click", async () => {
          const settings = readSettingsForm("edit-settings");
          const errEl = $("edit-result");
          // If the admin status changed, flip it via the dedicated endpoint
          // first (is_admin is not part of the settings payload).
          const wantAdmin = isRootAdmin ? true : !!$("edit-isadmin").checked;
          if (wantAdmin !== !!target.is_admin) {
            const r = await request(`${basePath}/${encodeURIComponent(username)}/admin-status`, "POST", { is_admin: wantAdmin });
            if (r.error) { errEl.innerHTML = `<div class="error" style="margin-top:10px;">${esc(r.error.message)}</div>`; return; }
          }
          const r = await request(`${basePath}/${encodeURIComponent(username)}`, "PUT", { settings });
          if (r.error) { errEl.innerHTML = `<div class="error" style="margin-top:10px;">${esc(r.error.message)}</div>`; return; }
          toast("Saved");
          render();
        });
      }
    });
  }
}

// --- Groups ---------------------------------------------------------------
async function renderGroups() {
  const res = await fetchData();
  if (res.error) {
    setPanel(`<div class="card"><h3>Groups</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  // The SSO group configuration lives on this page. It is read-only: the
  // admin/user group lists come from the environment (VRECKAN_OIDC_*_GROUPS)
  // and the observed list is derived from the SSO accounts.
  const ssoRes = await request("/api/admin/sso-groups", "GET");
  const ssoData = ssoRes.data || {};
  const adminGroups = ssoData.admin_groups || [];
  // The Add-group form renders role + permission checkboxes, so make sure the
  // permission catalog is loaded before the panel HTML is built (otherwise the
  // permission list renders empty).
  await loadPermCatalog();
  const allRoles = res.data.roles || [];
  const observedGroups = ssoData.observed || [];
  // Each group is a card (like the account cards) that expands in place when
  // edited. SSO-observed groups are badged so an admin can tell them apart
  // from manually created ones; ones in the admin SSO list are highlighted.
  const cards = res.groups
    .map((g) => {
      const s = g.settings || {};
      const isSso = observedGroups.includes(g.name);
      const ssoBadge = isSso
        ? `<span class="badge${adminGroups.includes(g.name) ? " badge-ok" : ""}" title="${adminGroups.includes(g.name) ? "In the admin SSO list (grants admin)" : "Observed from SSO"}">SSO${adminGroups.includes(g.name) ? " · admin" : ""}</span>`
        : "";
      return `
        <div class="card" data-group-name="${esc(g.name)}">
          <div style="display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap;">
            <div>
              <strong>${esc(g.name)}</strong>
              ${ssoBadge}
            </div>
            <div class="admin-actions">
              <button class="btn btn-sm btn-ghost" data-act="edit" data-name="${esc(g.name)}">Edit</button>
              <button class="btn btn-sm btn-danger" data-act="delete" data-name="${esc(g.name)}">Delete</button>
            </div>
          </div>
          <div style="margin-top:10px;display:flex;gap:6px;flex-wrap:wrap;">
            <span class="badge ${s.active ? "badge-ok" : "badge-warn"}">${s.active ? "active" : "inactive"}</span>
            <span class="badge">${s.persistent_storage ? "storage" : "no storage"}</span>
            <span class="badge">${s.gpu ? "gpu" : "no gpu"}</span>
            ${(g.roles || []).length ? `<span class="badge">roles: ${esc((g.roles || []).join(", "))}</span>` : '<span class="badge">no roles</span>'}
            ${(g.permissions || []).length ? `<span class="badge">perms: ${esc((g.permissions || []).join(", "))}</span>` : ""}
            <span class="badge">storage: ${s.storage_limit == null || s.storage_limit < 0 ? "∞" : s.storage_limit + "MB"}</span>
          </div>
        </div>`;
    })
    .join("");

  setPanel(`
    <div class="card">
      <h3>Add group</h3>
      <div class="admin-form-row" style="margin-bottom:12px;">
        <div class="field"><label>Name</label><input type="text" id="group-new-name" placeholder="^[a-zA-Z0-9_-]+$"></div>
      </div>
      ${settingsFormHtml(defaultSettings(lastGroups), lastGroups, "group-new-settings")}
      <div class="field" style="margin-top:12px;"><label>Roles (granted to every member)</label><div id="group-new-roles">${roleCheckboxesHtml([], allRoles)}</div></div>
      <div class="field" style="margin-top:12px;"><label>Permissions (granted to every member)</label><div id="group-new-perms">${permCheckboxesHtml([], "perm")}</div></div>
      <div style="margin-top:12px;"><button class="btn btn-primary" id="group-create">Create</button></div>
    </div>
    <div class="section-title">Groups</div>
    <div id="group-cards">${cards || '<div class="card"><p class="muted">No groups.</p></div>'}</div>
  `);

  $("group-create").addEventListener("click", async () => {
    const name = $("group-new-name").value.trim();
    if (!name) return toast("Name required", "error");
    const settings = readSettingsForm("group-new-settings");
    const roles = Array.from($("group-new-roles").querySelectorAll('input[name="role"]:checked')).map((el) => el.value);
    const permissions = Array.from($("group-new-perms").querySelectorAll('input[name="perm"]:checked')).map((el) => el.value);
    const r = await request("/api/admin/groups", "POST", { name, settings, roles, permissions });
    if (r.error) return toast(r.error.message, "error");
    toast("Created");
    render();
  });

  const cardsEl = $("group-cards");
  cardsEl.addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-act]");
    if (!btn) return;
    const name = btn.dataset.name;
    if (btn.dataset.act === "delete") {
      if (!(await confirmModal(`Delete group ${name}?`, { title: "Delete group", confirmLabel: "Delete", danger: true }))) return;
      const r = await request(`/api/admin/groups/${encodeURIComponent(name)}`, "DELETE");
      if (r.error) return toast(r.error.message, "error");
      toast("Deleted");
      render();
      return;
    }
    if (btn.dataset.act === "edit") {
      const d = await fetchData();
      const g = d.groups.find((x) => x.name === name);
      if (!g) return toast("Not found", "error");
      // Load the permission catalog before rendering the edit form so the
      // permission checkboxes are populated (the roles list comes from d.data).
      await loadPermCatalog();
      const allRoles = (d.data && d.data.roles) || [];
      // Expand the edit form in place: it replaces this group's card so the
      // form occupies the same space the card did (mirrors the account cards).
      const card = cardsEl.querySelector(`.card[data-group-name="${name}"]`);
      if (!card) return toast("Not found", "error");
      card.innerHTML = `
        <h3>Edit group ${esc(name)}</h3>
        ${settingsFormHtml(g.settings || defaultSettings(lastGroups), lastGroups, "group-edit-settings")}
        <div class="field" style="margin-top:12px;"><label>Roles (granted to every member)</label><div id="group-edit-roles">${roleCheckboxesHtml(g.roles || [], allRoles)}</div></div>
        <div class="field" style="margin-top:12px;"><label>Permissions (granted to every member)</label><div id="group-edit-perms">${permCheckboxesHtml(g.permissions || [], "perm")}</div></div>
        <div id="group-edit-result"></div>
        <div style="margin-top:12px;">
          <button class="btn btn-primary" id="group-save">Save</button>
          <button class="btn btn-ghost" id="group-close">Cancel</button>
        </div>`;
      $("group-close").addEventListener("click", () => render());
      $("group-save").addEventListener("click", async () => {
        const settings = readSettingsForm("group-edit-settings");
        const roles = Array.from($("group-edit-roles").querySelectorAll('input[name="role"]:checked')).map((el) => el.value);
        const permissions = Array.from($("group-edit-perms").querySelectorAll('input[name="perm"]:checked')).map((el) => el.value);
        const r = await request(`/api/admin/groups/${encodeURIComponent(name)}`, "PUT", { settings, roles, permissions });
        if (r.error) {
          $("group-edit-result").innerHTML = `<div class="error" style="margin-top:10px;">${esc(r.error.message)}</div>`;
          return;
        }
        toast("Saved");
        render();
      });
    }
  });

}

// --- Volume mounts --------------------------------------------------------
async function renderVolumeMounts() {
  const res = await fetchData();
  if (res.error) {
    setPanel(`<div class="card"><h3>Volume mounts</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const mounts = res.volumeMounts || [];
  const userNames = [...(res.admins || []).map((u) => u.username), ...(res.users || []).map((u) => u.username)];
  const groupNames = lastGroups;

  const rows = mounts
    .map((m) => {
      const assign = m.scope === "none" ? "\u2014" : `${m.scope}: ${m.target || "\u2014"}`;
      return `
        <tr>
          <td><strong>${esc(m.name)}</strong></td>
          <td><code>${esc(m.host_path)}</code></td>
          <td><code>${esc(m.container_path)}</code></td>
          <td>${m.read_only ? "ro" : "rw"}</td>
          <td>${esc(assign)}</td>
          <td class="admin-actions">
            <button class="btn btn-sm btn-ghost" data-act="edit" data-name="${esc(m.name)}">Edit</button>
            <button class="btn btn-sm btn-danger" data-act="delete" data-name="${esc(m.name)}">Delete</button>
          </td>
        </tr>`;
    })
    .join("");

  const targetOptions = (scope, selected) => {
    if (scope === "none") return `<option value="">\u2014</option>`;
    const names = scope === "group" ? groupNames : userNames;
    return names
      .map((n) => `<option value="${esc(n)}" ${n === selected ? "selected" : ""}>${esc(n)}</option>`)
      .join("");
  };

  const mountFormHtml = (m, prefix, isNew) => `
    <div class="admin-form-row">
      <div class="field"><label>Name</label><input type="text" id="${prefix}-name" value="${esc(m.name || "")}" placeholder="^[a-zA-Z0-9_-]+$" ${isNew ? "" : "readonly"}></div>
      <div class="field"><label>Host path</label><input type="text" id="${prefix}-host" value="${esc(m.host_path || "")}" placeholder="/mnt/NVME/apps/depot"></div>
      <div class="field"><label>Container path</label><input type="text" id="${prefix}-container" value="${esc(m.container_path || "")}" placeholder="depot (relative to home dir)"></div>
    </div>
    <div class="admin-form-row">
      <div class="field"><label>Access</label><label class="checkbox"><input type="checkbox" id="${prefix}-ro" ${m.read_only ? "checked" : ""}> Read-only</label></div>
      <div class="field"><label>Assign to</label><select id="${prefix}-scope">
        <option value="none" ${(m.scope || "none") === "none" ? "selected" : ""}>None (inactive)</option>
        <option value="user" ${m.scope === "user" ? "selected" : ""}>User</option>
        <option value="group" ${m.scope === "group" ? "selected" : ""}>Group</option>
      </select></div>
      <div class="field"><label>Target</label><select id="${prefix}-target" ${(m.scope || "none") === "none" ? "disabled" : ""}>${targetOptions(m.scope || "none", m.target)}</select></div>
    </div>`;

  setPanel(`
    <div class="card">
      <h3>Add volume mount</h3>
      <p class="muted">Bind a directory from the Docker host into app containers. Container paths are relative to the user's home directory inside the container.</p>
      ${mountFormHtml({}, "vm-new", true)}
      <div style="margin-top:12px;"><button class="btn btn-primary" id="vm-create">Create</button></div>
    </div>
    <div class="card">
      <h3 style="margin-bottom:12px;">Volume mounts</h3>
      <table class="admin-table">
        <thead><tr><th>Name</th><th>Host path</th><th>Container path</th><th>Mode</th><th>Assigned to</th><th></th></tr></thead>
        <tbody id="vm-rows">${rows || '<tr><td colspan="6" class="muted">No volume mounts.</td></tr>'}</tbody>
      </table>
    </div>
    <div id="vm-modal"></div>
  `);

  const wireScope = (prefix) => {
    const scopeSel = $(`${prefix}-scope`);
    const targetSel = $(`${prefix}-target`);
    if (!scopeSel || !targetSel) return;
    scopeSel.addEventListener("change", () => {
      const scope = scopeSel.value;
      targetSel.innerHTML = targetOptions(scope, "");
      targetSel.disabled = scope === "none";
    });
  };
  wireScope("vm-new");

  $("vm-create").addEventListener("click", async () => {
    const name = $("vm-new-name").value.trim();
    const host_path = $("vm-new-host").value.trim();
    const container_path = $("vm-new-container").value.trim();
    const read_only = $("vm-new-ro").checked;
    const scope = $("vm-new-scope").value;
    const target = $("vm-new-target").value;
    if (!name) return toast("Name required", "error");
    if (!host_path) return toast("Host path required", "error");
    if (!container_path) return toast("Container path required", "error");
    if (scope !== "none" && !target) return toast("Pick a target", "error");
    const r = await request("/api/admin/volume_mounts", "POST", { name, host_path, container_path, read_only, scope, target });
    if (r.error) return toast(r.error.message, "error");
    toast("Created");
    render();
  });

  $("vm-rows").addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-act]");
    if (!btn) return;
    const name = btn.dataset.name;
    if (btn.dataset.act === "delete") {
      if (!(await confirmModal(`Delete volume mount ${name}?`, { title: "Delete volume mount", confirmLabel: "Delete", danger: true }))) return;
      const r = await request(`/api/admin/volume_mounts/${encodeURIComponent(name)}`, "DELETE");
      if (r.error) return toast(r.error.message, "error");
      toast("Deleted");
      render();
      return;
    }
    if (btn.dataset.act === "edit") {
      const d = await fetchData();
      const m = (d.volumeMounts || []).find((x) => x.name === name);
      if (!m) return;
      $("vm-modal").innerHTML = `
        <div class="card" style="margin-top:16px;">
          <h3>Edit volume mount ${esc(name)}</h3>
          ${mountFormHtml(m, "vm-edit", false)}
          <div style="margin-top:12px;">
            <button class="btn btn-primary" id="vm-save">Save</button>
            <button class="btn btn-ghost" id="vm-close">Cancel</button>
          </div>
        </div>`;
      wireScope("vm-edit");
      $("vm-close").addEventListener("click", () => ($("vm-modal").innerHTML = ""));
      $("vm-save").addEventListener("click", async () => {
        const r = await request(`/api/admin/volume_mounts/${encodeURIComponent(name)}`, "PUT", {
          host_path: $("vm-edit-host").value.trim(),
          container_path: $("vm-edit-container").value.trim(),
          read_only: $("vm-edit-ro").checked,
          scope: $("vm-edit-scope").value,
          target: $("vm-edit-target").value,
        });
        if (r.error) return toast(r.error.message, "error");
        toast("Saved");
        render();
      });
    }
  });
}

// --- App stores -----------------------------------------------------------
async function renderStores() {
  const res = await request("/api/admin/apps/stores", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>App Stores</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const stores = res.data || [];
  const rows = stores
    .map(
      (s) => `
      <tr>
        <td><strong>${esc(s.name)}</strong></td>
        <td><code>${esc(s.url)}</code></td>
        <td class="admin-actions"><button class="btn btn-sm btn-danger" data-store="${esc(s.name)}">Delete</button></td>
      </tr>`
    )
    .join("");
  setPanel(`
    <div class="card">
      <h3>Add app store</h3>
      <div class="admin-form-row" style="margin-bottom:12px;">
        <div class="field"><label>Name</label><input type="text" id="store-name"></div>
        <div class="field" style="flex:2;"><label>URL (YAML)</label><input type="text" id="store-url" placeholder="https://…/store.yml"></div>
      </div>
      <button class="btn btn-primary" id="store-create">Add</button>
    </div>
    <div class="card">
      <h3 style="margin-bottom:12px;">Configured stores</h3>
      <table class="admin-table">
        <thead><tr><th>Name</th><th>URL</th><th></th></tr></thead>
        <tbody id="store-rows">${rows || '<tr><td colspan="3" class="muted">No stores.</td></tr>'}</tbody>
      </table>
    </div>
  `);
  $("store-create").addEventListener("click", async () => {
    const name = $("store-name").value.trim();
    const url = $("store-url").value.trim();
    if (!name || !url) return toast("Name and URL required", "error");
    const r = await request("/api/admin/apps/stores", "POST", { name, url });
    if (r.error) return toast(r.error.message, "error");
    toast("Added");
    render();
  });
  $("store-rows").addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-store]");
    if (!btn) return;
    if (!(await confirmModal(`Delete store ${btn.dataset.store}?`, { title: "Delete store", confirmLabel: "Delete", danger: true }))) return;
    const r = await request(`/api/admin/apps/stores/${encodeURIComponent(btn.dataset.store)}`, "DELETE");
    if (r.error) return toast(r.error.message, "error");
    toast("Deleted");
    render();
  });
}

// --- Apps -----------------------------------------------------------------
function shortSha(sha) {
  if (!sha) return "—";
  return String(sha).slice(0, 10);
}

function fmtBytes(n) {
  if (!n || n <= 0) return "";
  const units = ["B", "KB", "MB", "GB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(n >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

// Update the progress bar inside an installed-apps table row in place, so
// the table visibly tracks the download without a full re-render.
function updatePullProgressRow(appId, app) {
  const wrap = document.querySelector(`[data-progress-for="${appId}"]`);
  if (!wrap) return;
  const ps = app.pull_status;
  if (ps === "pulling" || ps === "queued") {
    wrap.hidden = false;
    const prog = app.pull_progress || {};
    const fill = wrap.querySelector(".progress-fill");
    const label = wrap.querySelector(".progress-pct");
    if (fill && prog.percentage != null) fill.style.width = `${prog.percentage}%`;
    if (label)
      label.textContent =
        prog.percentage != null
          ? `${Math.floor(prog.percentage)}%${prog.total ? " · " + fmtBytes(prog.total) : ""}`
          : prog.status || "starting…";
  } else {
    wrap.hidden = true;
    // The pull reached a terminal state — refresh the status badge in place so
    // it no longer reads "pulling"/"waiting to start" (the badge is otherwise
    // only refreshed on a full table re-render).
    const badge = wrap.closest("td")?.querySelector(".badge");
    if (badge) {
      if (ps === "pull_failed") {
        badge.textContent = "pull failed";
        badge.className = "badge badge-warn";
      } else if (app.image_sha) {
        badge.textContent = "up to date";
        badge.className = "badge badge-ok";
      }
    }
  }
}

// Poll the installed-apps list until the app's image pull reaches a terminal
// state, mirroring progress into the modal (and the table row) throughout.
// `isDismissed` reports whether the user closed the modal (stop polling).
// Returns "ok" | "failed" | "gone" | "timeout".
async function trackPullInModal(appId, box, confirmBtn, close, isDismissed) {
  const fill = () => box.querySelector("#install-progress-fill");
  const pct = () => box.querySelector("#install-progress-pct");
  const text = () => box.querySelector("#install-progress-text");
  const deadline = Date.now() + 30 * 60 * 1000; // generous: big images
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 1500));
    if (isDismissed && isDismissed()) return "gone";
    let apps;
    try {
      const r = await request("/api/admin/apps/installed", "GET");
      if (r.error) continue;
      apps = r.data || [];
    } catch (_) {
      continue;
    }
    const app = apps.find((a) => a.id === appId);
    if (!app) {
      confirmBtn.textContent = "Close";
      return "gone";
    }
    const ps = app.pull_status;
    const prog = app.pull_progress || {};
    updatePullProgressRow(appId, app);
    if (ps === "pulling" || ps === "queued") {
      const f = fill();
      if (f && prog.percentage != null) f.style.width = `${prog.percentage}%`;
      const p = pct();
      if (p) p.textContent = prog.percentage != null ? `${Math.floor(prog.percentage)}%` : "";
      const t = text();
      if (t) t.textContent = `Downloading image… ${prog.status ? `(${prog.status})` : ""}`.trim();
      continue;
    }
    if (ps === "pull_failed" || (typeof ps === "string" && ps.startsWith("error"))) {
      const t = text();
      if (t) t.textContent = "Download failed";
      const errEl = box.querySelector("#install-error");
      if (errEl) {
        errEl.textContent = (prog && prog.status) || "The image download failed.";
        errEl.hidden = false;
      }
      confirmBtn.textContent = "Close";
      return "failed";
    }
    // pull_status cleared -> success
    const f = fill();
    if (f) f.style.width = "100%";
    const p = pct();
    if (p) p.textContent = "100%";
    const t = text();
    if (t) t.textContent = "Download complete";
    confirmBtn.textContent = "Done";
    return "ok";
  }
  return "timeout";
}

// In-app install flow: a modal replaces the old confirm()+prompt() pair,
// then tracks the container image download with a live progress bar while
// the installed-apps table refreshes in the background.
async function openInstallModal(avail, storeName) {
  let templates = [];
  try {
    const t = await request("/api/admin/apps/templates", "GET");
    if (!t.error) templates = t.data || [];
  } catch (_) {
    /* template list is optional */
  }
  const tplOptions = (templates.length ? templates : [{ name: "default" }])
    .map((t) => `<option value="${esc(t.name)}">${esc(t.name)}</option>`)
    .join("");
  const { userNames, groupNames } = await rosterNames();

  let phase = "confirm"; // confirm -> downloading -> done | failed
  let dismissed = false;
  const { box, close } = openCustomModal({
    title: `Install ${avail.name}`,
    wide: true,
    onDismiss: () => {
      dismissed = true;
    },
    body: `
      <p class="muted">Installs the app and downloads its container image. The download runs in the background — progress is shown below.</p>
      <label>App template
        <select id="install-template">${tplOptions}</select>
      </label>
      <div class="admin-form-row" style="margin-top:12px;">
        <div class="field" style="flex:1;">
          <label>Who can use this app</label>
          ${accessCheckboxesHtml("install-access", ["*"], [], userNames, groupNames)}
        </div>
      </div>
      <div id="install-progress" hidden>
        <div class="progress-label">
          <span id="install-progress-text">Downloading image…</span>
          <span id="install-progress-pct" class="muted"></span>
        </div>
        <div class="progress-track"><div class="progress-fill" id="install-progress-fill"></div></div>
      </div>
      <p id="install-error" class="error" hidden></p>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" id="install-cancel">Cancel</button>
        <button type="button" class="btn btn-primary" id="install-confirm">Install</button>
      </div>`,
  });

  const cancelBtn = box.querySelector("#install-cancel");
  const confirmBtn = box.querySelector("#install-confirm");
  const errEl = box.querySelector("#install-error");
  const access = wireAccessChecks(box, "install-access");
  cancelBtn.addEventListener("click", () => close());

  confirmBtn.addEventListener("click", async () => {
    if (phase === "confirm") {
      const appTemplate = box.querySelector("#install-template").value || "default";
      const payload = {
        id: crypto.randomUUID(),
        name: avail.name,
        logo: avail.logo,
        url: avail.url,
        source: storeName,
        source_app_id: avail.id,
        provider: avail.provider,
        // The web client uses home_directories as its "file-backed" flag
        // (Files badge + the open-file-on-launch modal), so derive it from the
        // store's open_support capability. (Previously hardcoded to false,
        // which silently installed file-backed apps as non-file-backed.)
        home_directories: !!avail.provider_config.open_support,
        ...access.collect(),
        provider_config: {
          image: avail.provider_config.image,
          port: avail.provider_config.port,
          nvidia_support: !!avail.provider_config.nvidia_support,
          dri3_support: !!avail.provider_config.dri3_support,
          type: avail.provider_config.type,
          url_support: !!avail.provider_config.url_support,
          open_support: !!avail.provider_config.open_support,
          extensions: avail.provider_config.extensions || [],
          autostart: false,
          env: [],
        },
        auto_update: true,
        app_template: appTemplate,
      };
      confirmBtn.disabled = true;
      cancelBtn.disabled = true;
      confirmBtn.textContent = "Installing…";
      const res = await request("/api/admin/apps/installed", "POST", payload);
      if (res.error) {
        errEl.textContent = res.error.message;
        errEl.hidden = false;
        confirmBtn.disabled = false;
        cancelBtn.disabled = false;
        confirmBtn.textContent = "Install";
        return;
      }
      phase = "downloading";
      errEl.hidden = true;
      box.querySelector("#install-progress").hidden = false;
      cancelBtn.remove();
      confirmBtn.textContent = "Waiting for download…";
      render(); // refresh the installed-apps table so the new app is visible
      const outcome = await trackPullInModal(payload.id, box, confirmBtn, close, () => dismissed);
      if (outcome === "ok") {
        phase = "done";
      } else {
        phase = "failed";
        if (outcome === "timeout") {
          const t = box.querySelector("#install-progress-text");
          if (t) t.textContent = "Still downloading in the background…";
        }
      }
      if (phase === "done") confirmBtn.textContent = "Done";
      else confirmBtn.textContent = "Close";
      // Re-enable now that the download reached a terminal state, so the
      // user can click Done/Close to dismiss the modal.
      confirmBtn.disabled = false;
    } else if (phase === "done") {
      close();
      render();
      toast("Installed");
    } else if (phase === "failed") {
      close();
      render();
    }
  });
}

// Usernames (admins + regular users) and group names for the access pickers.
async function rosterNames() {
  const res = await request("/api/admin/data", "POST", {});
  if (res.error) return { userNames: [], groupNames: [] };
  const d = res.data || {};
  return {
    userNames: [...(d.admins || []).map((u) => u.username), ...(d.users || []).map((u) => u.username)],
    groupNames: (d.groups || []).map((g) => g.name),
  };
}

// Checkbox block for per-app access control. `prefix` namespaces the element
// IDs so two modals can coexist. selectedUsers/selectedGroups pre-check
// entries; the "*" (or legacy "all") wildcard maps to the "All users" box.
function accessCheckboxesHtml(prefix, selectedUsers, selectedGroups, userNames, groupNames) {
  const su = selectedUsers || [];
  const sg = selectedGroups || [];
  const all = su.includes("*") || su.includes("all");
  const opt = (value, checked) =>
    `<label style="display:inline-flex;align-items:center;gap:4px;margin:2px 12px 2px 0;"><input type="checkbox" value="${esc(value)}"${checked ? " checked" : ""}> ${esc(value)}</label>`;
  const users = (userNames || []).map((u) => opt(u, !all && su.includes(u))).join("");
  const groups = (groupNames || []).map((g) => opt(g, !all && sg.includes(g))).join("");
  return `
    <label style="display:flex;align-items:center;gap:6px;font-weight:600;"><input type="checkbox" id="${prefix}-all"${all ? " checked" : ""}> All users</label>
    <div id="${prefix}-users" style="margin-top:6px; display:flex; flex-wrap:wrap;">${users || '<span class="muted">No users</span>'}</div>
    <div id="${prefix}-groups" style="margin-top:6px; display:flex; flex-wrap:wrap;">${groups || '<span class="muted">No groups</span>'}</div>
    <p class="error" id="${prefix}-warn" hidden style="margin-top:8px;">No one is selected — this app will be hidden from and unusable by everyone.</p>`;
}

// Wire up the "All users" master checkbox: when checked it disables (and
// unchecks) the individual user/group boxes. collect() returns the
// {users, groups} arrays to persist — ["*"] / [] when "All users" is set.
function wireAccessChecks(box, prefix) {
  const all = box.querySelector(`#${prefix}-all`);
  const usersBox = box.querySelector(`#${prefix}-users`);
  const groupsBox = box.querySelector(`#${prefix}-groups`);
  const warn = box.querySelector(`#${prefix}-warn`);
  const sync = () => {
    const off = all.checked;
    for (const el of [usersBox, groupsBox]) {
      el.style.opacity = off ? "0.4" : "";
      el.style.pointerEvents = off ? "none" : "";
    }
    if (off) {
      usersBox.querySelectorAll("input").forEach((i) => (i.checked = false));
      groupsBox.querySelectorAll("input").forEach((i) => (i.checked = false));
    }
    const any = usersBox.querySelector("input:checked") || groupsBox.querySelector("input:checked");
    if (warn) warn.hidden = off || !!any;
  };
  all.addEventListener("change", sync);
  [...usersBox.querySelectorAll("input"), ...groupsBox.querySelectorAll("input")].forEach((i) =>
    i.addEventListener("change", sync)
  );
  sync();
  return {
    collect: () => {
      if (all.checked) return { users: ["*"], groups: [] };
      return {
        users: [...usersBox.querySelectorAll("input:checked")].map((i) => i.value),
        groups: [...groupsBox.querySelectorAll("input:checked")].map((i) => i.value),
      };
    },
  };
}

// Edit an installed app's provider config and access. Exposes the URL env
// var (the container env var the app reads the launch URL from, e.g.
// CHROME_CLI for linuxserver/chrome, FIREFOX_CLI for linuxserver/firefox)
// and the user/group access list.
async function openAppEditModal(app) {
  const { userNames, groupNames } = await rosterNames();
  const { box, close } = openCustomModal({
    title: `Edit ${app.name}`,
    wide: true,
    body: `
      <div class="admin-form-row">
        <div class="field">
          <label>URL environment variable</label>
          <input type="text" id="edit-url-env-var" value="${esc((app.provider_config || {}).url_env_var || "")}" placeholder="e.g. CHROME_CLI">
        </div>
      </div>
      <p class="muted" style="font-size:0.8rem; margin-top:10px; line-height:1.4;">
        The environment variable this app reads the launch URL from. Upstream
        linuxserver images use <code>&lt;APP&gt;_CLI</code> — e.g.
        <code>CHROME_CLI</code> for Chrome, <code>FIREFOX_CLI</code> for
        Firefox. Leave empty if the app doesn't open a URL.
      </p>
      <div class="admin-form-row" style="margin-top:12px;">
        <div class="field" style="flex:1;">
          <label>Who can use this app</label>
          ${accessCheckboxesHtml("edit-access", app.users || [], app.groups || [], userNames, groupNames)}
        </div>
      </div>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" id="edit-cancel">Cancel</button>
        <button type="button" class="btn btn-primary" id="edit-save">Save</button>
      </div>
    `,
  });

  const cancelBtn = box.querySelector("#edit-cancel");
  const saveBtn = box.querySelector("#edit-save");
  const input = box.querySelector("#edit-url-env-var");
  const access = wireAccessChecks(box, "edit-access");

  cancelBtn.addEventListener("click", () => close());
  saveBtn.addEventListener("click", async () => {
    const value = input.value.trim();
    const collected = access.collect();
    app.users = collected.users;
    app.groups = collected.groups;
    app.provider_config.url_env_var = value || null;
    saveBtn.disabled = true;
    const res = await request(`/api/admin/apps/installed/${app.id}`, "PUT", app);
    if (res.error) {
      toast(res.error.message, "error");
      saveBtn.disabled = false;
      return;
    }
    close();
    toast("Saved");
    render();
  });
  input.focus();
  input.select();
}

async function renderApps() {
  const [installedRes, storesRes] = await Promise.all([
    request("/api/admin/apps/installed", "GET"),
    request("/api/admin/apps/stores", "GET"),
  ]);
  if (installedRes.error) {
    setPanel(`<div class="card"><h3>Apps</h3><div class="error">${esc(installedRes.error.message)}</div></div>`);
    return;
  }
  const apps = installedRes.data || [];
  const stores = (storesRes.data || []);
  const storeOpts = stores.map((s) => `<option value="${esc(s.name)}" data-url="${esc(s.url)}">${esc(s.name)}</option>`).join("");

  const rows = apps
    .map((a) => {
      const pulling = a.pull_status === "pulling" || a.pull_status === "queued";
      const statusBadge = pulling
        ? `<span class="badge badge-pull">${a.pull_status === "queued" ? "waiting to start" : "pulling"}</span>`
        : a.pull_status === "pull_failed"
        ? `<span class="badge badge-warn">pull failed</span>`
        : a.image_sha
        ? `<span class="badge badge-ok">up to date</span>`
        : '<span class="badge">unknown</span>';
      // Always render the progress wrap (hidden unless pulling) so the
      // in-place progress updater can find it right after a re-render.
      const progress = `<div class="row-progress" data-progress-for="${esc(a.id)}" ${pulling ? "" : "hidden"}><div class="progress-track"><div class="progress-fill" style="width:0%"></div></div><div class="progress-pct muted"></div></div>`;
      return `
      <tr>
        <td><strong>${esc(a.name)}</strong><div class="muted" style="font-size:0.78rem;">${esc(a.id)}</div></td>
        <td>${esc(a.source || "")} / ${esc(a.source_app_id || "")}</td>
        <td>${a.home_directories ? "yes" : "no"}</td>
        <td>${a.users ? a.users.join(", ") : ""}</td>
        <td>${(a.groups || []).join(", ") || "—"}</td>
        <td><span class="badge">${shortSha(a.image_sha)}</span> ${statusBadge}${progress}</td>
        <td class="admin-actions">
          <button class="btn btn-sm btn-ghost" data-act="edit" data-id="${esc(a.id)}">Edit</button>
          <button class="btn btn-sm btn-ghost" data-act="check" data-id="${esc(a.id)}">Check</button>
          <button class="btn btn-sm btn-ghost" data-act="pull" data-id="${esc(a.id)}">Pull</button>
          <button class="btn btn-sm btn-danger" data-act="del" data-id="${esc(a.id)}">Delete</button>
        </td>
      </tr>`;
    })
    .join("");

  setPanel(`
    <div class="card">
      <h3>Install app from store</h3>
      <p class="muted">Pick a store to browse available apps, then choose one to install.</p>
      <div class="admin-form-row" style="margin-bottom:10px;">
        <div class="field"><label>Store</label><select id="avail-store">${storeOpts || '<option value="">No stores configured</option>'}</select></div>
        <div class="field"><label>Search</label><input id="avail-search" type="search" placeholder="Filter loaded apps…" autocomplete="off"></div>
        <button class="btn btn-ghost" id="avail-load">Load</button>
      </div>
      <div id="avail-list"></div>
    </div>

    <div class="card">
      <h3 style="margin-bottom:12px;">Installed apps</h3>
      <table class="admin-table">
        <thead><tr><th>App</th><th>Source</th><th>Home dirs</th><th>Users</th><th>Groups</th><th>Image</th><th></th></tr></thead>
        <tbody id="app-rows">${rows || '<tr><td colspan="7" class="muted">No apps installed.</td></tr>'}</tbody>
      </table>
    </div>
  `);

  const availStore = $("avail-store");
  const availSearch = $("avail-search");
  const availList = $("avail-list");
  let availApps = [];

  // Render (a filtered view of) the loaded store's apps into #avail-list.
  function renderAvailList() {
    if (!availApps.length) {
      availList.innerHTML = '<p class="muted">No apps in this store.</p>';
      return;
    }
    const q = (availSearch.value || "").trim().toLowerCase();
    const filtered = q
      ? availApps.filter((a) =>
          [a.name, a.id, a.provider].some((f) => (f || "").toLowerCase().includes(q))
        )
      : availApps;
    if (!filtered.length) {
      availList.innerHTML = `<p class="muted">No apps match “${esc(q)}”.</p>`;
      return;
    }
    availList.innerHTML = filtered
      .map(
        (a) => `
        <div class="share-row">
          <div class="share-meta">
            <strong>${esc(a.name)}</strong>
            <span class="muted">${esc(a.id)} · ${esc(a.provider || "")}</span>
          </div>
          <button class="btn btn-sm btn-primary" data-install="${esc(JSON.stringify(a))}">Install</button>
        </div>`
      )
      .join("");
  }

  // Delegated handler for the per-app Install buttons. Attached once to the
  // list container so re-rendering the (filtered) list never stacks duplicates.
  availList.addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-install]");
    if (!btn) return;
    const avail = JSON.parse(btn.dataset.install);
    await openInstallModal(avail, availStore.value);
  });

  // Live-filter the loaded list as the admin types.
  availSearch.addEventListener("input", () => {
    if (availApps.length) renderAvailList();
  });

  $("avail-load").addEventListener("click", async () => {
    const sel = availStore.selectedOptions[0];
    if (!sel) return toast("No store selected", "error");
    const url = sel.dataset.url;
    availList.innerHTML = '<div class="muted">Loading store…</div>';
    const r = await request(
      `/api/admin/apps/available?store_name=${encodeURIComponent(availStore.value)}&url=${encodeURIComponent(url)}`,
      "GET"
    );
    if (r.error) {
      availList.innerHTML = `<div class="error">${esc(r.error.message)}</div>`;
      return;
    }
    availApps = r.data || [];
    renderAvailList();
  });

  $("app-rows").addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-act]");
    if (!btn) return;
    const id = btn.dataset.id;
    if (btn.dataset.act === "edit") {
      const app = (apps || []).find((x) => x.id === id);
      if (!app) return toast("App not found", "error");
      openAppEditModal(app);
      return;
    }
    if (btn.dataset.act === "del") {
      if (!(await confirmModal("Delete this app?", { title: "Delete app", confirmLabel: "Delete", danger: true }))) return;
      const r = await request(`/api/admin/apps/installed/${id}`, "DELETE");
      if (r.error) return toast(r.error.message, "error");
      toast("Deleted");
      render();
      return;
    }
    if (btn.dataset.act === "check") {
      const r = await request(`/api/admin/apps/installed/${id}/check_update`, "POST", {});
      if (r.error) return toast(r.error.message, "error");
      const upd = r.data && r.data.update_available;
      toast(upd ? "Update available" : "Up to date", upd ? "error" : "success");
      return;
    }
    if (btn.dataset.act === "pull") {
      const r = await request(`/api/admin/apps/installed/${id}/pull_latest`, "POST", {});
      if (r.error) return toast(r.error.message, "error");
      toast("Pull started — progress is shown in the table", "success");
      render(); // show the row with its pulling badge + progress bar
      trackPullInRow(id);
    }
  });
}

// Follow a user-triggered pull in the background, updating the table row's
// progress bar in place until it completes (or fails).
async function trackPullInRow(appId) {
  const deadline = Date.now() + 30 * 60 * 1000;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 1500));
    let app;
    try {
      const r = await request("/api/admin/apps/installed", "GET");
      if (r.error) continue;
      app = (r.data || []).find((a) => a.id === appId);
    } catch (_) {
      continue;
    }
    if (!app) {
      render();
      return;
    }
    updatePullProgressRow(appId, app);
    const ps = app.pull_status;
    if (ps === "pulling" || ps === "queued") continue;
    if (ps === "pull_failed" || (typeof ps === "string" && ps.startsWith("error"))) {
      toast(`Pull failed: ${(app.pull_progress || {}).status || "unknown error"}`, "error");
    } else {
      toast("Pull complete");
    }
    render();
    return;
  }
  toast("Pull timed out — check back later", "error");
}

// --- Templates (Feat 14: categorized editor) ------------------------------
let tplSettings = []; // Resolved schema: labels, defaults, options.
let tplList = []; // Templates from the API.
let tplState = { selected: "new", name: "", values: null }; // Snapshot so an
// in-progress edit survives a section switch (the panel is wiped on leave).

const TPL_CATEGORIES = [
  ["ui", "UI settings"],
  ["app", "App settings"],
  ["hardening", "Hardening"],
  ["general", "General & performance"],
  ["webrtc", "WebRTC & TURN"],
  ["docker", "Docker advanced overrides"],
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
        ? `<option value=""${base === "" ? " selected" : ""}>— image default —</option>`
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
  let html = `<option value="new">— Create new —</option>`;
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
    setTplStatus("Enter a template name first.", "error");
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
    setTplStatus(`Saved template "${name}".`, "ok");
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
    setTplStatus("This template cannot be deleted.", "error");
    return;
  }
  if (
    !(await confirmModal(`Delete template "${name}"?`, {
      title: "Delete template",
      confirmLabel: "Delete",
      danger: true,
    }))
  )
    return;
  const r = await request(`/api/admin/apps/templates/${encodeURIComponent(name)}`, "DELETE");
  if (r.error) {
    if (r.status === 403) setTplStatus("Cannot delete the default template.", "error");
    else setTplStatus(r.error.message, "error");
    return;
  }
  setTplStatus(`Deleted template "${name}".`, "ok");
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
      .join("") || '<p class="muted">No settings defined.</p>';
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
      if (errEl) { errEl.textContent = `Invalid setting name: '${name}'.`; errEl.hidden = false; }
      return;
    }
    if (seen.has(name)) {
      if (errEl) { errEl.textContent = `Duplicate setting name: '${name}'.`; errEl.hidden = false; }
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
    if (res.error) throw new Error(res.error.message || "Failed to save");
    toast("Base layer saved.");
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
  const cats = TPL_CATEGORIES.map(([key, title]) => {
    const desc =
      key === "webrtc"
        ? `<p class="muted">Sessions stream over WebSockets by default. Setting any STUN, TURN, Cloudflare or public IP value below, or choosing WebRTC mode, switches the container to WebRTC with dual mode enabled so users can still fall back to WebSockets.</p>`
        : key === "docker"
        ? `<p class="muted">These settings override the default container configuration. Use with caution and only if you know what you are doing.</p>`
        : "";
    return `<h3 class="tpl-section-title">${title}</h3>${desc}<div class="tpl-form-grid" id="tpl-form-${key}"></div>`;
  }).join("");
  return `
  <div class="card">
    <h3>Base layer — global defaults</h3>
    <p class="muted">The template schema: the environment variables offered to app templates and their default (base-layer) values. Every launch starts from these defaults; per-app templates and per-launch choices override them. "docker" settings become container run-options instead of environment variables. An empty value lets the image's built-in default apply. Changes apply to new launches.</p>
    <div id="tpl-schema-settings"></div>
    <div style="margin-top:10px;display:flex;gap:8px;flex-wrap:wrap;">
      <button class="btn btn-ghost" id="tpl-schema-add" type="button">Add setting</button>
      <button class="btn btn-primary" id="tpl-schema-save" type="button">Save base layer</button>
    </div>
    <p id="tpl-schema-error" class="error" role="alert" hidden></p>
  </div>
  <div class="card">
    <h3>App templates</h3>
    <p class="muted">Templates define the environment variables passed to session containers. Only values that differ from the defaults are stored.</p>
    <div id="tpl-status" class="lab-status" hidden></div>
    <div class="admin-form-row">
      <div class="field"><label for="tpl-select">Template</label><select id="tpl-select"><option value="new">— Create new —</option></select></div>
      <div class="field" id="tpl-name-group"><label for="tpl-name-input">Name</label><input type="text" id="tpl-name-input" maxlength="64" placeholder="Enter a name for the new template"></div>
      <div class="field tpl-actions">
        <button type="button" class="btn btn-primary" id="tpl-save-btn">Save</button>
        <button type="button" class="btn btn-danger" id="tpl-delete-btn" hidden>Delete</button>
      </div>
    </div>
  </div>
  <div class="card">
    <h3>UI preview</h3>
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
            <div id="preview-core-buttons" class="tpl-sidebar-section">Core Buttons</div>
            <div id="preview-soft-buttons" class="tpl-sidebar-section">Soft Buttons</div>
            <div id="preview-video-settings" class="tpl-sidebar-section">Video Settings</div>
            <div id="preview-screen-settings" class="tpl-sidebar-section">Screen Settings</div>
            <div id="preview-audio-settings" class="tpl-sidebar-section">Audio Settings</div>
            <div id="preview-stats" class="tpl-sidebar-section">Stats</div>
            <div id="preview-clipboard" class="tpl-sidebar-section">Clipboard</div>
            <div id="preview-files" class="tpl-sidebar-section">Files</div>
            <div id="preview-apps" class="tpl-sidebar-section">Apps</div>
            <div id="preview-sharing" class="tpl-sidebar-section">Sharing</div>
            <div id="preview-gamepads" class="tpl-sidebar-section">Gamepads</div>
          </div>
          <div class="tpl-main-content">
            <p class="muted">Application Content Area</p>
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
    setPanel(`<div class="card"><h3>Templates</h3><div class="error">${esc(tplRes.error.message)}</div></div>`);
    return;
  }
  if (schemaRes.error) {
    setPanel(`<div class="card"><h3>Templates</h3><div class="error">${esc(schemaRes.error.message)}</div></div>`);
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

// --- Sessions -------------------------------------------------------------
async function renderSessions() {
  const res = await request("/api/admin/sessions", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>Sessions</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const groups = res.data || [];
  // Map of session_id -> session URL (with access token) so the Connect
  // button can open any user's session in a new tab.
  const urlMap = {};
  for (const g of groups) {
    for (const s of g.sessions || []) {
      if (s.session_id && s.session_url) urlMap[s.session_id] = s.session_url;
    }
  }
  const html = groups.length
    ? groups
        .map((g) => {
          const rows = (g.sessions || [])
            .map(
              (s) => `
              <tr>
                <td><strong>${esc(s.name ? `${s.app_name || s.app_id} - ${s.name}` : (s.app_name || s.app_id))}</strong><div class="muted" style="font-size:0.78rem;">${esc(s.session_id)}</div></td>
                <td>${s.created_at ? new Date(s.created_at * 1000).toLocaleString() : "—"}</td>
                <td>${s.is_collaboration ? "yes" : "no"}</td>
                <td class="admin-actions">
                  <button class="btn btn-sm btn-ghost" data-connect="${esc(s.session_id)}">Connect</button>
                  <button class="btn btn-sm btn-danger" data-kill="${esc(s.session_id)}">Kill</button>
                </td>
              </tr>`
            )
            .join("");
          return `
            <div class="card">
              <h3>${esc(g.username)}</h3>
              <table class="admin-table" style="margin-top:8px;">
                <thead><tr><th>Session</th><th>Started</th><th>Room mode</th><th></th></tr></thead>
                <tbody>${rows || '<tr><td colspan="4" class="muted">No active sessions.</td></tr>'}</tbody>
              </table>
            </div>`;
        })
        .join("")
    : '<div class="card"><h3>Sessions</h3><p class="muted">No active sessions across users.</p></div>';
  setPanel(`<div id="session-cards">${html}</div>`);
  const wrap = $("session-cards");
  wrap.addEventListener("click", async (e) => {
    const connectBtn = e.target.closest("button[data-connect]");
    if (connectBtn) {
      const url = urlMap[connectBtn.dataset.connect];
      if (url) window.open(new URL(url, window.location.origin).href, "_blank");
      return;
    }
    const btn = e.target.closest("button[data-kill]");
    if (!btn) return;
    if (!(await confirmModal("Kill this session?", { title: "Kill session", confirmLabel: "Kill", danger: true }))) return;
    const r = await request(`/api/admin/sessions/${encodeURIComponent(btn.dataset.kill)}`, "DELETE");
    if (r.error) return toast(r.error.message, "error");
    toast("Killed");
    render();
  });
}

// --- Backup / restore ------------------------------------------------------
const BACKUP_CHUNK_SIZE = 2 * 1024 * 1024;

function formatBytes(n) {
  if (n == null) return "—";
  if (n < 1024) return `${n} B`;
  if (n < 1048576) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1073741824) return `${(n / 1048576).toFixed(1)} MB`;
  return `${(n / 1073741824).toFixed(2)} GB`;
}

// Download a stored backup over the encrypted channel in 2 MiB base64 chunks,
// reassemble it into a Blob and trigger a browser download.
async function downloadBackup(name) {
  const parts = [];
  let filename = name;
  let index = 0;
  for (;;) {
    const res = await request(`/api/admin/backup/${encodeURIComponent(name)}/download?chunk_index=${index}`, "GET");
    if (res.error) throw new Error(res.error.message);
    const d = res.data || {};
    if (d.filename) filename = d.filename;
    if (d.chunk_data_b64) parts.push(base64ToArrayBuffer(d.chunk_data_b64));
    if (d.is_last_chunk) break;
    index += 1;
  }
  const blob = new Blob(parts, { type: "application/gzip" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

// Upload a backup archive over the encrypted channel in 2 MiB base64 chunks
// (initiate -> chunk* -> finalize); the server reassembles and restores it.
async function uploadBackup(file) {
  const init = await request("/api/admin/backup/restore/initiate", "POST", {});
  if (init.error) throw new Error(init.error.message);
  const upload_id = init.data.upload_id;
  const total = Math.ceil(file.size / BACKUP_CHUNK_SIZE) || 1;
  for (let i = 0; i < total; i++) {
    const buf = await file.slice(i * BACKUP_CHUNK_SIZE, (i + 1) * BACKUP_CHUNK_SIZE).arrayBuffer();
    const r = await request("/api/admin/backup/restore/chunk", "POST", {
      upload_id,
      chunk_index: i,
      chunk_data_b64: arrayBufferToBase64(buf),
    });
    if (r.error) throw new Error(r.error.message);
  }
  const fin = await request("/api/admin/backup/restore/finalize", "POST", {
    upload_id,
    total_chunks: total,
  });
  if (fin.error) throw new Error(fin.error.message);
}

async function renderBackup() {
  const res = await request("/api/admin/backup", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>Backup</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const backups = res.data || [];
  const rows = backups
    .map(
      (b) => `
        <tr>
          <td><strong>${esc(b.name)}</strong></td>
          <td>${formatBytes(b.size)}</td>
          <td>${esc(b.created_at || "")}</td>
          <td>${b.include_user_data ? "yes" : "no"}</td>
          <td class="admin-actions">
            <button class="btn btn-sm btn-ghost" data-bact="download" data-name="${esc(b.name)}">Download</button>
            <button class="btn btn-sm btn-primary" data-bact="restore" data-name="${esc(b.name)}">Restore</button>
            <button class="btn btn-sm btn-danger" data-bact="delete" data-name="${esc(b.name)}">Delete</button>
          </td>
        </tr>`
    )
    .join("");

  setPanel(`
    <div class="card">
      <h3>Create backup</h3>
      <p class="muted">Creates a <code>tar.gz</code> of the application configuration in <code>/data/backups</code>. Restoring re-applies it to the running server without a restart.</p>
      <label class="checkbox"><input type="checkbox" id="bk-include-data"> Include user data (home directories)</label>
      <div style="margin-top:12px;"><button class="btn btn-primary" id="bk-create">Create backup</button></div>
    </div>
    <div class="card">
      <h3 style="margin-bottom:12px;">Backups</h3>
      <table class="admin-table">
        <thead><tr><th>Name</th><th>Size</th><th>Created</th><th>User data</th><th></th></tr></thead>
        <tbody id="bk-rows">${rows || '<tr><td colspan="5" class="muted">No backups yet.</td></tr>'}</tbody>
      </table>
    </div>
    <div class="card">
      <h3>Restore from file</h3>
      <p class="muted">Upload a previously downloaded backup <code>tar.gz</code> to restore it. Restoring overwrites the configuration files contained in the archive; files not present in the archive are left unchanged. Active sessions are not affected.</p>
      <input type="file" id="bk-file" accept=".tar.gz,.tgz,application/gzip">
      <div style="margin-top:12px;"><button class="btn btn-primary" id="bk-restore-file">Restore uploaded file</button></div>
    </div>
  `);

  $("bk-create").addEventListener("click", async () => {
    const include_user_data = $("bk-include-data").checked;
    const r = await request("/api/admin/backup/create", "POST", { include_user_data });
    if (r.error) return toast(r.error.message, "error");
    toast(`Created ${r.data.name}`);
    render();
  });

  $("bk-rows").addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-bact]");
    if (!btn) return;
    const name = btn.dataset.name;
    const act = btn.dataset.bact;
    if (act === "download") {
      try {
        await downloadBackup(name);
        toast("Download started");
      } catch (err) {
        toast(err.message, "error");
      }
      return;
    }
    if (act === "restore") {
      const ok = await confirmModal(
        `Restore ${name}? This overwrites the configuration files contained in the archive and reloads them. Active sessions are not affected. If the archive is older than the current admin key, you could lock yourself out.`,
        { title: "Restore backup", confirmLabel: "Restore", danger: true }
      );
      if (!ok) return;
      const r = await request("/api/admin/backup/restore", "POST", { name });
      if (r.error) return toast(r.error.message, "error");
      toast("Restored and reloaded");
      render();
      return;
    }
    if (act === "delete") {
      if (!(await confirmModal(`Delete backup ${name}?`, { title: "Delete backup", confirmLabel: "Delete", danger: true }))) return;
      const r = await request(`/api/admin/backup/${encodeURIComponent(name)}`, "DELETE");
      if (r.error) return toast(r.error.message, "error");
      toast("Deleted");
      render();
    }
  });

  $("bk-restore-file").addEventListener("click", async () => {
    const input = $("bk-file");
    if (!input.files || !input.files.length) return toast("Choose a backup file first", "error");
    const file = input.files[0];
    const ok = await confirmModal(
      `Restore from ${file.name}? This overwrites the configuration files contained in the archive and reloads them. Active sessions are not affected.`,
      { title: "Restore from file", confirmLabel: "Restore", danger: true }
    );
    if (!ok) return;
    try {
      await uploadBackup(file);
      toast("Restored and reloaded");
      render();
    } catch (err) {
      toast(err.message, "error");
    }
  });
}

// --- App Laboratory (Feat 6) ------------------------------------------------
// Create a meta-app from an installed base app, or edit an existing one.
// "Launch & modify" opens a live session whose home dir is the app's home
// template (read-write), so tweaks made inside are baked into the template
// for all future sessions. The panel is rendered once and kept alive across
// section switches so the embedded session iframe survives.

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
    <h3>App Laboratory</h3>
    <p class="muted">Create a custom app from an installed base app, or edit an existing one. "Launch &amp; modify" opens a live session whose home directory is the app's home template — changes you make inside are saved into the template for all future sessions.</p>
    <div id="lab-status" class="lab-status" hidden></div>
    <form id="lab-form">
      <div class="admin-form-row">
        <div class="field">
          <label for="lab-app-select">App to customize</label>
          <select id="lab-app-select"><option value="new">— Create new —</option></select>
        </div>
        <div class="field">
          <label for="lab-base-app-select">Base application</label>
          <select id="lab-base-app-select"><option value="">Select a base application…</option></select>
        </div>
      </div>
      <p class="muted" id="lab-base-desc">The new app inherits the image and provider settings from the base application.</p>
      <div class="admin-form-row">
        <div class="field">
          <label for="lab-app-name">App name</label>
          <input type="text" id="lab-app-name" maxlength="64">
        </div>
        <div class="field">
          <label>App icon (PNG)</label>
          <div class="admin-form-row">
            <img id="lab-icon-preview" class="lab-icon-preview" src="/img/icon128.png" alt="Icon preview">
            <button type="button" class="btn btn-ghost" id="lab-icon-upload-btn">Upload</button>
            <input type="file" id="lab-icon-upload" accept="image/png" hidden>
          </div>
        </div>
      </div>
      <div class="field">
        <label for="lab-autostart-script">Autostart script (X11)</label>
        <textarea id="lab-autostart-script" rows="4" placeholder="program \${VRECKAN_FILE:+&quot;$VRECKAN_FILE&quot;} \${VRECKAN_URL:+&quot;$VRECKAN_URL&quot;}"></textarea>
      </div>
      <div class="field">
        <label for="lab-autostart-wayland-script">Autostart script (Wayland)</label>
        <textarea id="lab-autostart-wayland-script" rows="4" placeholder="program \${VRECKAN_FILE:+&quot;$VRECKAN_FILE&quot;} \${VRECKAN_URL:+&quot;$VRECKAN_URL&quot;}"></textarea>
      </div>
      <div class="admin-form-row">
        <div class="field">
          <label for="lab-app-users">Allowed users</label>
          <input type="text" id="lab-app-users" value="all">
        </div>
        <div class="field">
          <label for="lab-app-groups">Allowed groups</label>
          <input type="text" id="lab-app-groups" value="all">
        </div>
        <div class="field field-row">
          <label class="check">
            <input type="checkbox" id="lab-launch-wayland" checked>
            <span>Wayland mode</span>
          </label>
        </div>
      </div>
      <div class="admin-form-row" style="margin-top:12px;">
        <button type="button" class="btn btn-ghost" id="lab-update-btn" disabled>Save changes</button>
        <button type="button" class="btn btn-primary" id="lab-launch-btn" style="flex-grow:1;">
          <span id="lab-launch-btn-text">Launch &amp; modify home directory</span>
        </button>
      </div>
    </form>
  </div>

  <div class="card" style="margin-top:14px;">
    <h3>Customization session</h3>
    <p id="lab-main-placeholder" class="muted">The customization session will appear here. Launch one to modify the app's home directory template.</p>
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
  let html = `<option value="new">— Create new —</option>`;
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
  let bhtml = `<option value="">Select a base application…</option>`;
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
    setLabStatus("Customization session closed.", "success");
  } catch (err) {
    setLabStatus(`Failed to close session: ${err.message}`, "error");
  }
  const frame = $("lab-session-frame");
  if (frame) {
    frame.src = "about:blank";
    frame.hidden = true;
  }
  const placeholder = $("lab-main-placeholder");
  if (placeholder) placeholder.hidden = false;
  const btnText = $("lab-launch-btn-text");
  if (btnText) btnText.textContent = "Launch & modify home directory";
  window.dispatchEvent(new CustomEvent("vreckan:sessions-changed"));
}

async function handleLabUpdate() {
  if (!labState.isEditing || !labState.currentApp || !labState.isDirty) return false;
  setLabStatus("Saving application settings…");
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
    setLabStatus(`Saved ${r.data.name}.`, "success");
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
    setLabStatus("Pick a base application and an app name first.", "error");
    return;
  }

  if (launchBtn) launchBtn.disabled = true;
  if (btnText) btnText.textContent = "Saving & launching…";
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
      setLabStatus(`Created ${appToLaunch.name} — launching customization session…`);
      window.dispatchEvent(new CustomEvent("vreckan:apps-changed"));
    } else {
      if (labState.isDirty) {
        const ok = await handleLabUpdate();
        if (!ok) throw new Error("Failed to save app settings before launching.");
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
    if (btnText) btnText.textContent = "Close session";
    setLabStatus("Customization session running — changes to the home directory are written into the app's template.", "success");
    window.dispatchEvent(new CustomEvent("vreckan:sessions-changed"));
  } catch (err) {
    setLabStatus(err.message, "error");
    if (btnText) btnText.textContent = "Launch & modify home directory";
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
      toast("Only PNG icons are supported", "error");
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
      setPanel(`<div class="card"><h3>App Laboratory</h3><div class="error">${esc(res.error.message)}</div></div>`);
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
      if (btnText) btnText.textContent = "Close session";
    }
  }
  populateLabDropdowns(desiredAppId, desiredBaseId);
}

// --- Roles ----------------------------------------------------------------
// Roles bundle fine-grained permissions and can be assigned to users and
// groups. Built-in roles (admin / operator / user) can be edited but not
// deleted; custom roles can be created and deleted (deletion is refused while
// a role is still assigned to a user or group).
async function renderRoles() {
  const [rolesRes, permRes] = await Promise.all([
    request("/api/admin/roles", "GET"),
    loadPermCatalog(),
  ]);
  if (rolesRes.error) {
    setPanel(`<div class="card"><h3>Roles</h3><div class="error">${esc(rolesRes.error.message)}</div></div>`);
    return;
  }
  const roles = rolesRes.data || [];
  const rows = roles
    .map(
      (r) => `
      <tr>
        <td><strong>${esc(r.name)}</strong>${r.is_builtin ? ' <span class="badge">built-in</span>' : ""}</td>
        <td class="muted">${esc(r.description || "")}</td>
        <td>${(r.permissions || []).length ? (r.permissions || []).map((p) => `<span class="badge">${esc(p)}</span>`).join(" ") : '<span class="muted">none</span>'}</td>
        <td class="admin-actions">
          <button class="btn btn-sm btn-ghost" data-act="edit" data-name="${esc(r.name)}">Edit</button>
          <button class="btn btn-sm btn-danger" data-act="delete" data-name="${esc(r.name)}" ${r.is_builtin ? "disabled" : ""}>Delete</button>
        </td>
      </tr>`
    )
    .join("");

  setPanel(`
    <div class="card">
      <h3>Create role</h3>
      <div class="admin-form-row" style="margin-bottom:12px;">
        <div class="field"><label>Name</label><input type="text" id="role-new-name" placeholder="^[a-zA-Z0-9_-]+$"></div>
        <div class="field" style="flex:2;"><label>Description</label><input type="text" id="role-new-desc"></div>
      </div>
      <div id="role-new-perms">${permCheckboxesHtml()}</div>
      <div style="margin-top:12px;"><button class="btn btn-primary" id="role-create">Create</button></div>
      <div id="role-result"></div>
    </div>
    <div class="card">
      <h3 style="margin-bottom:12px;">Roles</h3>
      <table class="admin-table">
        <thead><tr><th>Name</th><th>Description</th><th>Permissions</th><th></th></tr></thead>
        <tbody id="role-rows">${rows || '<tr><td colspan="4" class="muted">No roles.</td></tr>'}</tbody>
      </table>
    </div>
    <div id="role-modal"></div>
  `);

  $("role-create").addEventListener("click", async () => {
    const name = $("role-new-name").value.trim();
    if (!name) return toast("Name is required", "error");
    const description = $("role-new-desc").value.trim();
    const permissions = readPermCheckboxes($("role-new-perms"));
    const res = await request("/api/admin/roles", "POST", { name, description, permissions });
    if (res.error) {
      $("role-result").innerHTML = `<div class="error" style="margin-top:10px;">${esc(res.error.message)}</div>`;
      return;
    }
    toast("Role created");
    render();
  });

  $("role-rows").addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-act]");
    if (!btn) return;
    const name = btn.dataset.name;
    if (btn.dataset.act === "delete") {
      if (!(await confirmModal(`Delete role ${name}?`, { title: "Delete role", confirmLabel: "Delete", danger: true }))) return;
      const res = await request(`/api/admin/roles/${encodeURIComponent(name)}`, "DELETE");
      if (res.error) return toast(res.error.message, "error");
      toast("Role deleted");
      render();
      return;
    }
    if (btn.dataset.act === "edit") {
      const d = await request("/api/admin/roles", "GET");
      const role = (d.data || []).find((r) => r.name === name);
      if (!role) return toast("Not found", "error");
      $("role-modal").innerHTML = `
        <div class="card" style="margin-top:16px;">
          <h3>Edit role ${esc(name)}</h3>
          <div class="field" style="margin-bottom:12px;"><label>Description</label><input type="text" id="role-edit-desc" value="${esc(role.description || "")}"></div>
          <div id="role-edit-perms">${permCheckboxesHtml(role.permissions || [])}</div>
          <div id="role-edit-result"></div>
          <div style="margin-top:12px;">
            <button class="btn btn-primary" id="role-save">Save</button>
            <button class="btn btn-ghost" id="role-close">Cancel</button>
          </div>
        </div>`;
      $("role-close").addEventListener("click", () => ($("role-modal").innerHTML = ""));
      $("role-save").addEventListener("click", async () => {
        const description = $("role-edit-desc").value;
        const permissions = readPermCheckboxes($("role-edit-perms"));
        const res = await request(`/api/admin/roles/${encodeURIComponent(name)}`, "PUT", { description, permissions });
        if (res.error) {
          $("role-edit-result").innerHTML = `<div class="error" style="margin-top:10px;">${esc(res.error.message)}</div>`;
          return;
        }
        toast("Role saved");
        render();
      });
    }
  });
}

const SECTION_RENDERERS = {
  overview: renderOverview,
  "global-settings": renderGlobalSettings,
  accounts: renderRoster,
  groups: renderGroups,
  roles: renderRoles,
  sso: renderSso,
  mounts: renderVolumeMounts,
  backup: renderBackup,
  apps: renderApps,
  laboratory: renderLaboratory,
  stores: renderStores,
  templates: renderTemplates,
  sessions: renderSessions,
};

// Which permission unlocks each admin sidebar section. "overview" is the
// landing page and is shown to anyone who can see the admin area at all;
// every other section maps to its own admin.<section> permission. The "admin"
// super-permission (checked in hasPerm) unlocks all of them.
const SECTION_PERMS = {
  overview: null,
  "global-settings": "admin.global_settings",
  accounts: "admin.accounts",
  groups: "admin.groups",
  roles: "admin.roles",
  sso: "admin.sso",
  mounts: "admin.mounts",
  backup: "admin.backup",
  apps: "admin.apps",
  laboratory: "admin.laboratory",
  stores: "admin.stores",
  templates: "admin.templates",
  sessions: "admin.sessions",
};

// True if the user holds the "admin" super-permission or any admin.* permission.
showAdminTab.hasAnyAdminPermission = (perms) =>
  (perms || []).some((p) => p === "admin" || p.startsWith("admin."));

export function showAdminTab(visible, permissions = []) {
  // Feat 5: the admin entries live in the persistent sidebar, grouped into
  // labelled sections. Each section is shown only if the caller holds the
  // permission that unlocks it (or the "admin" super-permission). Then any
  // group that ends up with no visible links is hidden so an empty label never
  // shows on its own.
  const hasPerm = (perm) =>
    !perm ||
    (permissions || []).some((p) => p === "admin" || p === perm);
  document.querySelectorAll(".sidebar-nav .admin-link").forEach((b) => {
    const section = b.dataset.adminSection;
    const perm = SECTION_PERMS[section] !== undefined ? SECTION_PERMS[section] : null;
    b.hidden = !visible || !hasPerm(perm);
  });
  document.querySelectorAll(".sidebar-nav .admin-group").forEach((g) => {
    g.hidden = !visible || !g.querySelector(".nav-link:not([hidden])");
  });
}

// Re-render the currently selected admin section. Called by the main app on
// page load / navigation so the panel is never blank when the admin view is
// shown without a nav click (e.g. a reload that lands on #/admin).
export function refreshAdminSection() {
  render();
}

// Switch to an admin section (called by the main app's persistent sidebar).
// Sets the active section and renders it into the shared content area.
export function activateAdminSection(section) {
  if (!SECTION_RENDERERS[section]) section = "overview";
  currentSection = section;
  render();
}

export function initAdminView() {
  // Feat 5: the admin section buttons now live in the persistent sidebar and
  // are wired by app.js, which calls activateAdminSection(). Nothing to wire here.
}

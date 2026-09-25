import { $, esc, request } from "./util.js";
import { t } from "./i18n.js";
import { renderOverview } from "./admin/overview.js";
import { renderRoster } from "./admin/accounts.js";
import { renderGroups } from "./admin/groups.js";
import { renderRoles } from "./admin/roles.js";
import { renderGlobalSettings } from "./admin/global-settings.js";
import { renderSso } from "./admin/sso.js";
import { renderVolumeMounts } from "./admin/mounts.js";
import { renderStores } from "./admin/stores.js";
import { renderApps } from "./admin/apps.js";
import { renderTemplates } from "./admin/templates.js";
import { renderSessions } from "./admin/sessions.js";
import { renderBackup } from "./admin/backup.js";
import { renderLaboratory } from "./admin/laboratory.js";

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

export function defaultSettings(groupOptions = []) {
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

export function settingsFormHtml(settings, groupOptions, formId, includeGroup = true) {
  // Merge over the defaults so a partial (or empty) settings object still
  // renders every field with a sensible value instead of `undefined`.
  const g = { ...defaultSettings(groupOptions), ...(settings || {}) };
  // The "group" field is a user's primary group — meaningful on the account
  // and overview forms, but vestigial on a group form (a group's own
  // settings.group is never read by the server), so the group editor passes
  // includeGroup=false to drop it.
  let groupField = "";
  if (includeGroup) {
    const groupOpts = [`<option value="none">${t("settings.none")}</option>`];
    (groupOptions || []).forEach((name) => {
      const sel = g.group === name ? " selected" : "";
      groupOpts.push(`<option value="${esc(name)}"${sel}>${esc(name)}</option>`);
    });
    if (g.group && g.group !== "none" && !(groupOptions || []).includes(g.group)) {
      groupOpts.push(`<option value="${esc(g.group)}" selected>${esc(g.group)}</option>`);
    }
    groupField = `<div class="field"><label>${t("settings.group")}</label><select name="group">${groupOpts.join("")}</select></div>`;
  }
  return `
    <div class="admin-settings-form" id="${formId}">
      <label class="check"><input type="checkbox" name="active" ${g.active ? "checked" : ""}> ${t("settings.active")}</label>
      ${groupField}
      <label class="check"><input type="checkbox" name="persistent_storage" ${g.persistent_storage ? "checked" : ""}> ${t("settings.persistentStorage")}</label>
      <label class="check"><input type="checkbox" name="public_sharing" ${g.public_sharing ? "checked" : ""}> ${t("settings.publicSharing")}</label>
      <label class="check"><input type="checkbox" name="harden_container" ${g.harden_container ? "checked" : ""}> ${t("settings.hardenContainer")}</label>
      <label class="check"><input type="checkbox" name="harden_openbox" ${g.harden_openbox ? "checked" : ""}> ${t("settings.hardenOpenbox")}</label>
      <label class="check"><input type="checkbox" name="gpu" ${g.gpu ? "checked" : ""}> ${t("settings.gpu")}</label>
      <div class="field"><label>${t("settings.storageLimit")}</label><input type="number" name="storage_limit" value="${g.storage_limit}"></div>
      <div class="field"><label>${t("settings.sessionLimit")}</label><input type="number" name="session_limit" value="${g.session_limit}"></div>
    </div>`;
}

export function readSettingsForm(formId) {
  const form = $(formId);
  const out = {};
  SETTING_FIELDS.forEach((f) => {
    const el = form.querySelector(`[name="${f}"]`);
    // A field may be intentionally omitted from a form (e.g. the group
    // editor drops the "group" field) — skip it rather than reading it.
    if (!el) return;
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
export async function loadPermCatalog() {
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
export function permCheckboxesHtml(selected = [], name = "perm") {
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
    group(t("settings.adminPerms"), (permCatalog && permCatalog.admin) || []) +
    group(t("settings.userPerms"), (permCatalog && permCatalog.user) || [])
  );
}

// Read the checked values of the permission/role checkboxes (by input name)
// from a form element.
export function readPermCheckboxes(form, name = "perm") {
  return Array.from(form.querySelectorAll(`input[name="${name}"]:checked`)).map(
    (el) => el.value
  );
}

// Render the role checkboxes for a form, sourced from the roles list (not the
// permission catalog, which has no roles). `selected` is the list of role
// names that should start checked.
export function roleCheckboxesHtml(selected = [], roles = []) {
  const sel = new Set(selected || []);
  return (roles || [])
    .map(
      (r) => `<label class="check perm-check"><input type="checkbox" name="role" value="${esc(r.name)}" ${sel.has(r.name) ? "checked" : ""}> ${esc(r.name)}${r.is_builtin ? t("accounts.builtinSuffix") : ""}</label>`
    )
    .join("") || `<p class="muted">${t("groups.noRolesDefined")}</p>`;
}

// --- Admin state ----------------------------------------------------------
let currentSection = "overview";
export let lastGroups = [];

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

export async function render() {
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
  panel.innerHTML = `<div class="muted">${t("common.loading")}</div>`;
  await SECTION_RENDERERS[currentSection]();
}

// Shared management-data fetch used by several sections (accounts, groups,
// mounts, roles). Populates the shared `lastGroups` list as a side effect.
export async function fetchData() {
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

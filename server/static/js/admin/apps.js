// Apps section: the installed-apps table (with live pull progress), the
// "Install app from store" browser, and the install/edit modals.
//
// The small DOM helpers ($, esc, toast, request, setPanel) are local copies so
// this file is self-contained; `render` comes from admin.js.
import { $, esc, toast, request } from "../util.js";
import { confirmModal, openCustomModal } from "../modal.js";
import { t } from "../i18n.js";
import { render, pickSectionHtml, wirePickSections } from "../admin.js";
import { renderStores } from "./stores.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
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
    // only refreshed on a full table re-render). The status badge is the last
    // badge in the image cell (the first is the short-SHA badge).
    const badges = wrap.closest(".image-meta")?.querySelectorAll(".badge") || [];
    const badge = badges[badges.length - 1];
    if (badge) {
      if (ps === "pull_failed") {
        badge.textContent = t("apps.pullFailed");
        badge.className = "badge badge-warn";
      } else if (app.image_sha) {
        badge.textContent = t("apps.upToDate");
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
      confirmBtn.textContent = t("apps.close");
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
      const textEl = text();
      if (textEl) textEl.textContent = `${t("apps.downloading")}${prog.status ? ` (${prog.status})` : ""}`.trim();
      continue;
    }
    if (ps === "pull_failed" || (typeof ps === "string" && ps.startsWith("error"))) {
      const textEl = text();
      if (textEl) textEl.textContent = t("apps.downloadFailed");
      const errEl = box.querySelector("#install-error");
      if (errEl) {
        errEl.textContent = (prog && prog.status) || t("apps.downloadFailedDefault");
        errEl.hidden = false;
      }
      confirmBtn.textContent = t("apps.close");
      return "failed";
    }
    // pull_status cleared -> success
    const f = fill();
    if (f) f.style.width = "100%";
    const p = pct();
    if (p) p.textContent = "100%";
    const textEl = text();
    if (textEl) textEl.textContent = t("apps.downloadComplete");
    confirmBtn.textContent = t("apps.done");
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
  // The template dropdown only earns its place once the admin has created a
  // custom template; otherwise the first entry in the list is auto-selected
  // (the seeded "Default" on a fresh install) and its real name is sent on
  // submit — the old lowercase "default" fallback literal never matched the
  // case-sensitive template lookup in the catalog. A server that predates
  // provenance tagging (no entry carries the field) keeps the dropdown as a
  // conservative fallback.
  const autoTemplate = (templates[0] && templates[0].name) || "Default";
  const showTemplateSelect =
    templates.some((x) => x.provenance === "custom") ||
    !templates.some((x) => x.provenance !== undefined);
  const tplOptions = (templates.length ? templates : [{ name: autoTemplate }])
    .map((x) => `<option value="${esc(x.name)}">${esc(x.name)}</option>`)
    .join("");
  const templateField = showTemplateSelect
    ? `<label>${t("apps.appTemplate")}
        <select id="install-template">${tplOptions}</select>
      </label>`
    : `<label>${t("apps.appTemplate")}
        <span class="install-template-static">${esc(autoTemplate)}<span class="muted install-template-static-note">${t("apps.templateAutoSelected")}</span></span>
      </label>`;
  const { userNames, groupNames } = await rosterNames();

  let phase = "confirm"; // confirm -> downloading -> done | failed
  let dismissed = false;
  const { box, close } = openCustomModal({
    title: t("apps.installTitle", { name: avail.name }),
    wide: true,
    onDismiss: () => {
      dismissed = true;
    },
    body: `
      <p class="muted">${t("apps.installHelp")}</p>
      ${templateField}
      <div class="admin-form-row mt-12" >
        <div class="field flex-1" >
          <label>${t("apps.accessLabel")}</label>
          ${accessCheckboxesHtml("install-access", ["*"], [], userNames, groupNames)}
        </div>
      </div>
      <div id="install-progress" hidden>
        <div class="progress-label">
          <span id="install-progress-text">${t("apps.downloading")}</span>
          <span id="install-progress-pct" class="muted"></span>
        </div>
        <div class="progress-track"><div class="progress-fill" id="install-progress-fill"></div></div>
      </div>
      <p id="install-error" class="error" hidden></p>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" id="install-cancel">${t("common.cancel")}</button>
        <button type="button" class="btn btn-primary" id="install-confirm">${t("apps.install")}</button>
      </div>`,
  });

  const cancelBtn = box.querySelector("#install-cancel");
  const confirmBtn = box.querySelector("#install-confirm");
  const errEl = box.querySelector("#install-error");
  const access = wireAccessChecks(box, "install-access");
  cancelBtn.addEventListener("click", () => close());

  confirmBtn.addEventListener("click", async () => {
    if (phase === "confirm") {
      const templateSelect = box.querySelector("#install-template");
      const appTemplate = (templateSelect && templateSelect.value) || autoTemplate;
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
      confirmBtn.textContent = t("apps.installing");
      const res = await request("/api/admin/apps/installed", "POST", payload);
      if (res.error) {
        errEl.textContent = res.error.message;
        errEl.hidden = false;
        confirmBtn.disabled = false;
        cancelBtn.disabled = false;
        confirmBtn.textContent = t("apps.install");
        return;
      }
      phase = "downloading";
      errEl.hidden = true;
      box.querySelector("#install-progress").hidden = false;
      cancelBtn.remove();
      confirmBtn.textContent = t("apps.waitingDownload");
      render(); // refresh the installed-apps table so the new app is visible
      const outcome = await trackPullInModal(payload.id, box, confirmBtn, close, () => dismissed);
      if (outcome === "ok") {
        phase = "done";
      } else {
        phase = "failed";
        if (outcome === "timeout") {
          const t = box.querySelector("#install-progress-text");
          if (t) t.textContent = t("apps.stillDownloading");
        }
      }
      if (phase === "done") confirmBtn.textContent = t("apps.done");
      else confirmBtn.textContent = t("apps.close");
      // Re-enable now that the download reached a terminal state, so the
      // user can click Done/Close to dismiss the modal.
      confirmBtn.disabled = false;
    } else if (phase === "done") {
      close();
      render();
      toast(t("apps.installed"));
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
  // One item per line, sorted A→Z: [checkbox] name … tag. The tag is a small
  // right-aligned marker so users and groups stay distinguishable even when
  // the two sections are collapsed into one view. Sections are collapsible
  // (see wireAccessChecks).
  const item = (value, checked, kind) =>
    ` <label class="pick-item" ><input type="checkbox" value="${esc(value)}"${checked ? " checked" : ""}> <span class="pick-item-name">${esc(value)}</span> <span class="pick-item-tag pick-item-tag-${kind}">${kind}</span></label>`;
  const users = [...(userNames || [])].sort((a, b) => a.localeCompare(b)).map((u) => item(u, !all && su.includes(u), "user")).join("");
  const groups = [...(groupNames || [])].sort((a, b) => a.localeCompare(b)).map((g) => item(g, !all && sg.includes(g), "group")).join("");
  return `
     <label class="access-all" ><input type="checkbox" id="${prefix}-all"${all ? " checked" : ""}> ${t("apps.allUsers")}</label>
     <div class="access-list" id="${prefix}-list" >
       ${pickSectionHtml(`${prefix}-users`, t("apps.accessUsersSection"), (userNames || []).length, users, { empty: t("apps.noUsers") })}
       ${pickSectionHtml(`${prefix}-groups`, t("apps.accessGroupsSection"), (groupNames || []).length, groups, { empty: t("apps.noGroups") })}
     </div>
    <p class="error mt-8" id="${prefix}-warn" hidden >${t("apps.noOneSelected")}</p>`;
}

// Wire up the access picker: the "All users" master checkbox (disables and
// unchecks both sections when checked), the collapsible section headers, and
// the per-section "select all" buttons. collect() returns the {users, groups}
// arrays to persist — ["*"] / [] when "All users" is set.
function wireAccessChecks(box, prefix) {
  const all = box.querySelector(`#${prefix}-all`);
  const list = box.querySelector(`#${prefix}-list`);
  const usersBox = box.querySelector(`#${prefix}-users`);
  const groupsBox = box.querySelector(`#${prefix}-groups`);
  const warn = box.querySelector(`#${prefix}-warn`);
  const sections = [
    { box: usersBox, header: box.querySelector(`#${prefix}-users-header`), selectAll: box.querySelector(`#${prefix}-users-selectall`) },
    { box: groupsBox, header: box.querySelector(`#${prefix}-groups-header`), selectAll: box.querySelector(`#${prefix}-groups-selectall`) },
  ];
  const sync = () => {
    const off = all.checked;
    if (list) list.classList.toggle("pick-disabled", off);
    if (off) {
      for (const s of sections) s.box.querySelectorAll("input").forEach((i) => (i.checked = false));
    }
    const any = usersBox.querySelector("input:checked") || groupsBox.querySelectorAll("input:checked").length > 0;
    if (warn) warn.hidden = off || !!any;
  };
  // Collapse/expand + per-section select-all come from the shared helper;
  // select-all flips call sync() to keep the master box and warning in step.
  wirePickSections(box, sync);
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
    title: t("apps.editTitle", { name: app.name }),
    wide: true,
    body: `
      <div class="admin-form-row">
        <div class="field">
          <label>${t("apps.urlEnvVarLabel")}</label>
          <input type="text" id="edit-url-env-var" value="${esc((app.provider_config || {}).url_env_var || "")}" placeholder="e.g. CHROME_CLI">
        </div>
      </div>
      <p class="muted fs-xs mt-10 lh-14" >
        ${t("apps.urlEnvVarHelp")}
      </p>
      <div class="admin-form-row mt-12" >
        <div class="field flex-1" >
          <label>${t("apps.accessLabel")}</label>
          ${accessCheckboxesHtml("edit-access", app.users || [], app.groups || [], userNames, groupNames)}
        </div>
      </div>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" id="edit-cancel">${t("common.cancel")}</button>
        <button type="button" class="btn btn-primary" id="edit-save">${t("common.save")}</button>
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
    toast(t("common.saved"));
    render();
  });
  input.focus();
  input.select();
}

// Human duration ("58 min", "3 h 12 min", "2 d 4 h") for the auto-update
// status line's "next check in …" part.
function fmtDuration(seconds) {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s} ${t("apps.unitMinutes")}`;
  const mins = Math.floor(s / 60);
  if (mins < 60) return `${mins} ${t("apps.unitMinutes")}`;
  const hours = Math.floor(mins / 60);
  const remMin = mins % 60;
  if (hours < 48) return remMin ? `${hours} ${t("apps.unitHours")} ${remMin} ${t("apps.unitMinutes")}` : `${hours} ${t("apps.unitHours")}`;
  const days = Math.floor(hours / 24);
  const remH = hours % 24;
  return remH ? `${days} ${t("apps.unitDays")} ${remH} ${t("apps.unitHours")}` : `${days} ${t("apps.unitDays")}`;
}

// "Next check in 58 min · Last checked 14:02 · 3 of 7 apps set to auto-update"
function autoUpdateStatusText(status, totalApps, optedCount) {
  const parts = [];
  if (status && status.next_check_at) {
    const ms = new Date(status.next_check_at * 1000).getTime() - Date.now();
    parts.push(t("apps.nextCheckIn", { time: ms > 0 ? fmtDuration(ms / 1000) : `0 ${t("apps.unitMinutes")}` }));
  }
  if (status && status.last_check_at) {
    const d = new Date(status.last_check_at * 1000);
    const hh = String(d.getHours()).padStart(2, "0");
    const mm = String(d.getMinutes()).padStart(2, "0");
    parts.push(t("apps.lastCheckedAt", { time: `${hh}:${mm}` }));
  }
  parts.push(t("apps.autoUpdateOptInCount", { count: optedCount, total: totalApps }));
  return parts.join(" · ");
}

async function renderApps() {
  // Kick off auto_update_status in parallel but do NOT await it before the
  // first paint: a slow or hung status endpoint (see ANT-347/ANT-348) must
  // not keep the installed table and store cards on "Loading…" forever.
  // While the fetch is in flight a sticky "Checking for updates…" toast marks
  // the wait; the status line and controls pick the value up when it lands
  // (at the end of this function) and fall back to the code defaults until
  // then.
  const autoUpdatePromise = request("/api/admin/apps/auto_update_status", "GET");
  // Track whether that fetch has settled — even if it settles before we get
  // to await it — so the "checking" UI below never flashes when the response
  // is fast.
  let autoUpdateSettled = false;
  autoUpdatePromise.then(
    () => { autoUpdateSettled = true; },
    () => { autoUpdateSettled = true; }
  );
  const [installedRes, storesRes] = await Promise.all([
    request("/api/admin/apps/installed", "GET"),
    request("/api/admin/apps/stores", "GET"),
  ]);
  if (installedRes.error) {
    setPanel(`<div class="card"><h3>${t("apps.heading")}</h3><div class="error">${esc(installedRes.error.message)}</div></div>`);
    return;
  }
  const apps = installedRes.data || [];
  const stores = (storesRes.data || []);
  const storeOpts = stores.map((s) => `<option value="${esc(s.name)}" data-url="${esc(s.url)}">${esc(s.name)}</option>`).join("");

  const rows = apps
    .map((a) => {
      const pulling = a.pull_status === "pulling" || a.pull_status === "queued";
      const updateAvailable = a.update_available === true;
      const statusBadge = pulling
        ? `<span class="badge badge-pull">${a.pull_status === "queued" ? t("apps.waitingToStart") : t("apps.pulling")}</span>`
        : a.pull_status === "pull_failed"
        ? `<span class="badge badge-warn">${t("apps.pullFailed")}</span>`
        : updateAvailable
        ? `<span class="badge badge-warn">${t("apps.updateAvailable")}</span>`
        : a.image_sha
        ? `<span class="badge badge-ok">${t("apps.upToDate")}</span>`
        : `<span class="badge">${t("apps.unknown")}</span>`;
      // The image reference is the row's actionable identity (which image it
      // runs); the short SHA and status badges sit on the line below. When the
      // SHA is unknown, name the image and say why instead of a bare "—".
      const imageRef = (a.provider_config || {}).image || "";
      const shaPart = a.image_sha
        ? `<span class="badge" title="${esc(a.image_sha)}">${shortSha(a.image_sha)}</span>`
        : `<span class="muted fs-xs" title="${t("apps.imageNotInspected")}">${t("apps.imageNotInspected")}</span>`;
      // Always render the progress wrap (hidden unless pulling) so the
      // in-place progress updater can find it right after a re-render.
      const progress = `<div class="row-progress" data-progress-for="${esc(a.id)}" ${pulling ? "" : "hidden"}><div class="progress-track"><div class="progress-fill" ></div></div><div class="progress-pct muted"></div></div>`;
      return `
      <tr>
        <td><strong>${esc(a.name)}</strong><div class="muted fs-xs" >${esc(a.id)}</div></td>
        <td>${esc(a.source || "")} / ${esc(a.source_app_id || "")}</td>
        <td>${a.home_directories ? t("apps.yes") : t("apps.no")}</td>
        <td>${a.users ? a.users.join(", ") : ""}</td>
        <td>${(a.groups || []).join(", ") || "—"}</td>
        <td>
          ${imageRef ? `<div class="image-ref" title="${esc(imageRef)}">${esc(imageRef)}</div>` : ""}
          <div class="image-meta">${shaPart} ${statusBadge}${progress}</div>
        </td>
        <td class="admin-actions">
          <button class="btn btn-sm btn-ghost" data-act="edit" data-id="${esc(a.id)}">${t("common.edit")}</button>
          <button class="btn btn-sm btn-ghost" data-act="check" data-id="${esc(a.id)}">${t("apps.check")}</button>
          <button class="btn btn-sm ${updateAvailable ? "btn-warning" : "btn-ghost"}" data-act="pull" data-id="${esc(a.id)}">${t("apps.pull")}</button>
          <label class="auto-toggle" title="${t("apps.auto")}"><input type="checkbox" data-act="auto" data-id="${esc(a.id)}"${a.auto_update ? " checked" : ""}> ${t("apps.auto")}</label>
          <button class="btn btn-sm btn-danger" data-act="del" data-id="${esc(a.id)}">${t("common.delete")}</button>
        </td>
      </tr>`;
    })
    .join("");

  const anyUpdatable = apps.some((a) => a.update_available === true);

  setPanel(`
    <div class="card">
      <div class="row row-between mb-12">
        <h3 class="mb-0">${t("apps.installedTitle")}</h3>
        <div class="row gap-8">
          <button class="btn btn-sm btn-ghost" id="apps-check-all" type="button">${t("apps.checkAll")}</button>
          <button class="btn btn-sm ${anyUpdatable ? "btn-warning" : "btn-ghost"}" id="apps-update-all" type="button">${t("apps.updateAll")}</button>
        </div>
      </div>
      <div class="auto-update-row row" id="auto-update-controls">
        <label>${t("apps.autoUpdate")} <input type="checkbox" id="auto-update-enabled"></label>
        <label>${t("apps.autoUpdateCheckEvery")}
          <select id="auto-update-interval">
            <option value="3600">${t("apps.interval1h")}</option>
            <option value="21600">${t("apps.interval6h")}</option>
            <option value="43200">${t("apps.interval12h")}</option>
            <option value="86400">${t("apps.interval24h")}</option>
          </select>
        </label>
        <span class="muted fs-xs flex-grow" id="auto-update-status"></span>
      </div>
      <table class="admin-table">
        <thead><tr><th>${t("files.app")}</th><th>${t("apps.source")}</th><th>${t("apps.homeDirs")}</th><th>${t("apps.users")}</th><th>${t("apps.groups")}</th><th>${t("apps.image")}</th><th></th></tr></thead>
        <tbody id="app-rows">${rows || `<tr><td colspan="7" class="muted">${t("apps.noAppsInstalled")}</td></tr>`}</tbody>
      </table>
    </div>

    <div class="card">
      <h3>${t("apps.installFromStoreTitle")}</h3>
      <p class="muted">${t("apps.installFromStoreHelp")}</p>
      <div class="admin-form-row mb-10" >
        <div class="field"><label>${t("apps.store")}</label><select id="avail-store">${storeOpts || `<option value="">${t("apps.noStoresConfigured")}</option>`}</select></div>
        <div class="field"><label>${t("apps.search")}</label><input id="avail-search" type="search" placeholder="${t("apps.searchPlaceholder")}" autocomplete="off"></div>
        <button class="btn btn-ghost" id="avail-load">${t("apps.load")}</button>
      </div>
      <div id="avail-list"></div>
    </div>
  `);

  // The store-management cards ("Add app store" + "Configured stores") used
  // to live on a separate Stores page; they now follow the install-from-store
  // card on this page. renderStores() appends them to the panel and wires
  // their add/delete handlers.
  await renderStores();

  const availStore = $("avail-store");
  const availSearch = $("avail-search");
  const availList = $("avail-list");
  let availApps = [];

  // Render (a filtered view of) the loaded store's apps into #avail-list.
  function renderAvailList() {
    if (!availApps.length) {
      availList.innerHTML = `<p class="muted">${t("apps.noAppsInStore")}</p>`;
      return;
    }
    const q = (availSearch.value || "").trim().toLowerCase();
    const filtered = q
      ? availApps.filter((a) =>
          [a.name, a.id, a.provider].some((f) => (f || "").toLowerCase().includes(q))
        )
      : availApps;
    if (!filtered.length) {
      availList.innerHTML = `<p class="muted">${t("apps.noAppsMatch", { q: esc(q) })}</p>`;
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
          <button class="btn btn-sm btn-primary" data-install="${esc(JSON.stringify(a))}">${t("apps.install")}</button>
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
    if (!sel) return toast(t("apps.noStoreSelected"), "error");
    const url = sel.dataset.url;
    availList.innerHTML = `<div class="muted">${t("apps.loadingStore")}</div>`;
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
      if (!app) return toast(t("apps.appNotFound"), "error");
      openAppEditModal(app);
      return;
    }
    if (btn.dataset.act === "del") {
      if (!(await confirmModal(t("apps.confirmDelete"), { title: t("apps.confirmDeleteTitle"), confirmLabel: t("common.delete"), danger: true }))) return;
      const r = await request(`/api/admin/apps/installed/${id}`, "DELETE");
      if (r.error) return toast(r.error.message, "error");
      toast(t("apps.deleted"));
      render();
      return;
    }
    if (btn.dataset.act === "check") {
      const r = await request(`/api/admin/apps/installed/${id}/check_update`, "POST", {});
      if (r.error) return toast(r.error.message, "error");
      const upd = r.data && r.data.update_available;
      toast(upd ? t("apps.updateAvailable") : t("apps.upToDate"), upd ? "error" : "success");
      render(); // reflect the fresh SHA / status / button color immediately
      return;
    }
    if (btn.dataset.act === "pull") {
      const r = await request(`/api/admin/apps/installed/${id}/pull_latest`, "POST", {});
      if (r.error) return toast(r.error.message, "error");
      toast(t("apps.pullStarted"), "success");
      render(); // show the row with its pulling badge + progress bar
      trackPullInRow(id);
    }
  });

  // "Check all": query every installed app's registry for a newer image and
  // report how many have an update available (no pulls are started).
  const checkAllBtn = $("apps-check-all");
  if (checkAllBtn) {
    checkAllBtn.addEventListener("click", async () => {
      checkAllBtn.disabled = true;
      const orig = checkAllBtn.textContent;
      checkAllBtn.textContent = t("apps.checkingAll");
      const r = await request("/api/admin/apps/installed/check_all_updates", "POST", {});
      checkAllBtn.disabled = false;
      checkAllBtn.textContent = orig;
      if (r.error) return toast(r.error.message, "error");
      const n = r.data && r.data.updates_available;
      toast(n ? t("apps.updatesAvailable", { count: n }) : t("apps.allUpToDate"), n ? "error" : "success");
      render(); // show the fresh per-app SHA / status / button color
    });
  }

  // "Update all": start a background pull of the latest image for every
  // installed app, then follow the aggregate progress in the table.
  const updateAllBtn = $("apps-update-all");
  if (updateAllBtn) {
    updateAllBtn.addEventListener("click", async () => {
      updateAllBtn.disabled = true;
      const orig = updateAllBtn.textContent;
      updateAllBtn.textContent = t("apps.updatingAll");
      const r = await request("/api/admin/apps/installed/pull_all_latest", "POST", {});
      updateAllBtn.disabled = false;
      updateAllBtn.textContent = orig;
      if (r.error) return toast(r.error.message, "error");
      const started = r.data && r.data.started;
      toast(started ? t("apps.updatesStarted", { count: started }) : t("apps.allUpToDate"), "success");
      render(); // show the rows with their pulling badges + progress bars
      trackAllPulls();
    });
  }

  // Per-app "Auto" checkbox: persist the app's auto_update flag through the
  // existing installed-app PUT endpoint (the full app round-trips, like the
  // edit modal does).
  $("app-rows").addEventListener("change", async (e) => {
    const input = e.target.closest('input[data-act="auto"]');
    if (!input) return;
    const app = (apps || []).find((x) => x.id === input.dataset.id);
    if (!app) return;
    const next = input.checked;
    if (app.auto_update === next) return;
    input.disabled = true;
    const r = await request(`/api/admin/apps/installed/${app.id}`, "PUT", { ...app, auto_update: next });
    if (r.error) {
      toast(r.error.message, "error");
      input.disabled = false;
      input.checked = !next;
      return;
    }
    render();
  });

  // Auto-update controls: enable toggle + interval select persist through
  // the admin settings path; the muted line mirrors the read-only job status
  // (which may not exist yet on older server builds, or may simply be slow —
  // until the status lands the controls fall back to the code defaults, the
  // line reads "checking" while the fetch is in flight, and settles on
  // "unavailable" only when the fetch actually fails).
  const auEnabled = $("auto-update-enabled");
  const auInterval = $("auto-update-interval");
  const auStatus = $("auto-update-status");
  const AUTO_UPDATE_INTERVAL_PRESETS = [
    [3600, "apps.interval1h"],
    [21600, "apps.interval6h"],
    [43200, "apps.interval12h"],
    [86400, "apps.interval24h"],
  ];
  let autoUpdateState = null;
  const optedInCount = () =>
    autoUpdateState && Number.isInteger(autoUpdateState.auto_update_apps_count)
      ? autoUpdateState.auto_update_apps_count
      : apps.filter((a) => a.auto_update).length;

  function applyAutoUpdateControls() {
    if (!auEnabled || !auInterval) return;
    const enabled = autoUpdateState && typeof autoUpdateState.enabled === "boolean" ? autoUpdateState.enabled : true;
    const interval = autoUpdateState && Number.isInteger(autoUpdateState.interval_seconds) ? autoUpdateState.interval_seconds : 3600;
    auEnabled.checked = enabled;
    auInterval.innerHTML = AUTO_UPDATE_INTERVAL_PRESETS.map(([s, key]) => `<option value="${s}">${t(key)}</option>`).join("");
    if (![...auInterval.options].some((o) => Number(o.value) === interval)) {
      const opt = document.createElement("option");
      opt.value = interval;
      opt.textContent = t("apps.intervalCustom", { seconds: interval });
      auInterval.appendChild(opt);
    }
    auInterval.value = String(interval);
    if (autoUpdateState) {
      auStatus.textContent = autoUpdateStatusText(autoUpdateState, apps.length, optedInCount());
    } else if (!autoUpdateSettled) {
      // The initial status fetch is still in flight (it can hang for a long
      // time — see ANT-347/ANT-348): the endpoint is not "unavailable", it is
      // merely slow, so the line says so instead of claiming a failure.
      auStatus.textContent = t("apps.checkingForUpdates");
    } else {
      auStatus.textContent = t("apps.autoUpdateStatusUnavailable");
    }
  }

  async function saveAutoUpdate() {
    const enabled = auEnabled.checked;
    const intervalSeconds = Number(auInterval.value);
    auEnabled.disabled = true;
    auInterval.disabled = true;
    const r = await request("/api/admin/apps/auto_update", "PUT", { enabled, interval_seconds: intervalSeconds });
    auEnabled.disabled = false;
    auInterval.disabled = false;
    if (r.error) return toast(r.error.message, "error");
    toast(t("common.saved"));
    // Refresh the live status line (and control values) from the job status.
    // When that endpoint isn't available on this build, keep the just-saved
    // values in the controls instead of snapping back to the defaults.
    const s = await request("/api/admin/apps/auto_update_status", "GET");
    if (!s.error && s.data) {
      autoUpdateState = s.data;
    } else {
      autoUpdateState = autoUpdateState ? { ...autoUpdateState, enabled, interval_seconds: intervalSeconds } : { enabled, interval_seconds: intervalSeconds };
    }
    applyAutoUpdateControls();
  }

  applyAutoUpdateControls();
  if (auEnabled) auEnabled.addEventListener("change", saveAutoUpdate);
  if (auInterval) auInterval.addEventListener("change", saveAutoUpdate);
  // The panel is painted. While the initial status fetch is still in flight
  // (ANT-347: it can hang for a long time while the server's auto-update job
  // holds its lock), make the wait visible: a sticky "Checking for updates…"
  // toast in the lower-right corner (util.js toast({ ttl: null })) — removed
  // the moment the fetch settles, success or error — and the status line
  // itself reads "checking" until then. If the fetch already settled before
  // this point, skip the toast so a fast response never causes a flash. A
  // failure (or a hang) leaves the controls on their code defaults, and a
  // late arrival never clobbers a value the admin already saved while the
  // fetch was in flight.
  let pendingToast = null;
  if (!autoUpdateSettled) {
    pendingToast = toast(t("apps.checkingForUpdates"), "", { ttl: null });
  }
  try {
    const autoUpdateRes = await autoUpdatePromise;
    if (autoUpdateState === null && autoUpdateRes && !autoUpdateRes.error && autoUpdateRes.data) {
      autoUpdateState = autoUpdateRes.data;
    }
  } catch (_) {
    /* status unavailable — controls keep their defaults */
  } finally {
    autoUpdateSettled = true;
    if (pendingToast) pendingToast.remove();
    applyAutoUpdateControls();
  }
}

// Follow the aggregate "update all" pull: poll the installed-apps list until
// no app is still pulling/queued, then report the outcome.
async function trackAllPulls() {
  const deadline = Date.now() + 30 * 60 * 1000;
  let anyFailed = false;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 1500));
    let list;
    try {
      const r = await request("/api/admin/apps/installed", "GET");
      if (r.error) continue;
      list = r.data || [];
    } catch (_) {
      continue;
    }
    const inFlight = list.filter((a) => a.pull_status === "pulling" || a.pull_status === "queued");
    if (inFlight.length) continue;
    anyFailed = list.some((a) => a.pull_status === "pull_failed" || (typeof a.pull_status === "string" && a.pull_status.startsWith("error")));
    toast(anyFailed ? t("apps.somePullsFailed") : t("apps.allPullsComplete"), anyFailed ? "error" : "success");
    render();
    return;
  }
  toast(t("apps.pullTimedOut"), "error");
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
      toast(`${t("apps.pullFailed")}: ${(app.pull_progress || {}).status || t("apps.unknownError")}`, "error");
    } else {
      toast(t("apps.pullComplete"));
    }
    render();
    return;
  }
  toast(t("apps.pullTimedOut"), "error");
}

export { renderApps };

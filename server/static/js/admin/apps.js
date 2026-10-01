// Apps section: the installed-apps table (with live pull progress), the
// "Install app from store" browser, and the install/edit modals.
//
// The small DOM helpers ($, esc, toast, request, setPanel) are local copies so
// this file is self-contained; `render` comes from admin.js.
import { $, esc, toast, request } from "../util.js";
import { confirmModal, openCustomModal } from "../modal.js";
import { t } from "../i18n.js";
import { render } from "../admin.js";
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
    // only refreshed on a full table re-render).
    const badge = wrap.closest("td")?.querySelector(".badge");
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
      const t = text();
      if (t) t.textContent = `${t("apps.downloading")}${prog.status ? ` (${prog.status})` : ""}`.trim();
      continue;
    }
    if (ps === "pull_failed" || (typeof ps === "string" && ps.startsWith("error"))) {
      const t = text();
      if (t) t.textContent = t("apps.downloadFailed");
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
    const t = text();
    if (t) t.textContent = t("apps.downloadComplete");
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
  const tplOptions = (templates.length ? templates : [{ name: "default" }])
    .map((t) => `<option value="${esc(t.name)}">${esc(t.name)}</option>`)
    .join("");
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
      <label>${t("apps.appTemplate")}
        <select id="install-template">${tplOptions}</select>
      </label>
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
  const opt = (value, checked) =>
    ` <label class="chip" ><input type="checkbox" value="${esc(value)}"${checked ? " checked" : ""}> ${esc(value)}</label>`;
  const users = (userNames || []).map((u) => opt(u, !all && su.includes(u))).join("");
  const groups = (groupNames || []).map((g) => opt(g, !all && sg.includes(g))).join("");
  return `
     <label class="row row-center gap-6 fw-600" ><input type="checkbox" id="${prefix}-all"${all ? " checked" : ""}> ${t("apps.allUsers")}</label>
     <div class="mt-6 row row-wrap" id="${prefix}-users" >${users || `<span class="muted">${t("apps.noUsers")}</span>`}</div>
     <div class="mt-6 row row-wrap" id="${prefix}-groups" >${groups || `<span class="muted">${t("apps.noGroups")}</span>`}</div>
    <p class="error mt-8" id="${prefix}-warn" hidden >${t("apps.noOneSelected")}</p>`;
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

async function renderApps() {
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
      const statusBadge = pulling
        ? `<span class="badge badge-pull">${a.pull_status === "queued" ? t("apps.waitingToStart") : t("apps.pulling")}</span>`
        : a.pull_status === "pull_failed"
        ? `<span class="badge badge-warn">${t("apps.pullFailed")}</span>`
        : a.image_sha
        ? `<span class="badge badge-ok">${t("apps.upToDate")}</span>`
        : `<span class="badge">${t("apps.unknown")}</span>`;
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
        <td><span class="badge">${shortSha(a.image_sha)}</span> ${statusBadge}${progress}</td>
        <td class="admin-actions">
          <button class="btn btn-sm btn-ghost" data-act="edit" data-id="${esc(a.id)}">${t("common.edit")}</button>
          <button class="btn btn-sm btn-ghost" data-act="check" data-id="${esc(a.id)}">${t("apps.check")}</button>
          <button class="btn btn-sm btn-ghost" data-act="pull" data-id="${esc(a.id)}">${t("apps.pull")}</button>
          <button class="btn btn-sm btn-danger" data-act="del" data-id="${esc(a.id)}">${t("common.delete")}</button>
        </td>
      </tr>`;
    })
    .join("");

  setPanel(`
    <div class="card">
      <div class="row row-between mb-12">
        <h3 class="mb-0">${t("apps.installedTitle")}</h3>
        <div class="row gap-8">
          <button class="btn btn-sm btn-ghost" id="apps-check-all" type="button">${t("apps.checkAll")}</button>
          <button class="btn btn-sm btn-ghost" id="apps-update-all" type="button">${t("apps.updateAll")}</button>
        </div>
      </div>
      <table class="admin-table">
        <thead><tr><th>${t("apps.app")}</th><th>${t("apps.source")}</th><th>${t("apps.homeDirs")}</th><th>${t("apps.users")}</th><th>${t("apps.groups")}</th><th>${t("apps.image")}</th><th></th></tr></thead>
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

// Vreckan web client — wires index.html to auth.js + api.js (E2EE).
import { login, logout, me, changePassword } from "./auth.js";
import { secureFetch } from "./api.js";
import { resetSession, ensureSession, arrayBufferToBase64 } from "./crypto.js";
import { initFilesView, showFilesTab, loadHomedirs, selectHomeDir } from "./files.js";
import { initAdminView, showAdminTab, activateAdminSection } from "./admin.js";
import { openCustomModal, confirmModal, promptModal, escModal as esc } from "./modal.js";

// ---------------------------------------------------------------------------
// Curated language list (subset of browser_extension/languages.js).
// Default is en_US.UTF-8; matching tries the browser language first.
// ---------------------------------------------------------------------------
const SUPPORTED_LANGS = [
  "en_US.UTF-8",
  "en_GB.UTF-8",
  "en_AU.UTF-8",
  "en_CA.UTF-8",
  "en_NZ.UTF-8",
  "es_ES.UTF-8",
  "es_MX.UTF-8",
  "es_AR.UTF-8",
  "es_US.UTF-8",
  "fr_FR.UTF-8",
  "fr_CA.UTF-8",
  "fr_BE.UTF-8",
  "de_DE.UTF-8",
  "de_AT.UTF-8",
  "de_CH.UTF-8",
  "pt_BR.UTF-8",
  "pt_PT.UTF-8",
  "it_IT.UTF-8",
  "nl_NL.UTF-8",
  "be_BY.UTF-8",
  "pl_PL.UTF-8",
  "cs_CZ.UTF-8",
  "sv_SE.UTF-8",
  "no_NO.UTF-8",
  "da_DK.UTF-8",
  "fi_FI.UTF-8",
  "tr_TR.UTF-8",
  "ru_RU.UTF-8",
  "uk_UA.UTF-8",
  "el_GR.UTF-8",
  "he_IL.UTF-8",
  "ar_SA.UTF-8",
  "ar_EG.UTF-8",
  "hi_IN.UTF-8",
  "bn_BD.UTF-8",
  "ja_JP.UTF-8",
  "ko_KR.UTF-8",
  "zh_CN.UTF-8",
  "zh_TW.UTF-8",
];

function guessBrowserLang() {
  const nav = (navigator.language || "en").toUpperCase();
  if (SUPPORTED_LANGS.includes(`${nav}.UTF-8`)) return `${nav}.UTF-8`;
  const primary = nav.split("_")[0];
  const match = SUPPORTED_LANGS.find((l) => l.startsWith(`${primary}_`));
  return match || "en_US.UTF-8";
}

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------
const $ = (id) => document.getElementById(id);

function toast(message, kind = "success") {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.textContent = message;
  $("toasts").appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

function showError(el, message) {
  el.textContent = message;
  el.hidden = false;
}
function clearError(el) {
  el.textContent = "";
  el.hidden = true;
}

// Feat 5: the persistent sidebar highlights the active destination. For the
// admin view we track the selected admin section here so the correct sidebar
// item stays highlighted (admin.js owns the actual section state).
let currentAdminSection = "overview";

function setSidebarActive(view) {
  const nav = $("sidebar-nav");
  if (!nav) return;
  nav.querySelectorAll(".nav-link").forEach((b) => b.classList.remove("active"));
  if (view === "main") $("tab-apps")?.classList.add("active");
  else if (view === "files") $("tab-files")?.classList.add("active");
  else if (view === "admin") {
    const btn = nav.querySelector(`.nav-link[data-admin-section="${currentAdminSection}"]`);
    if (btn) btn.classList.add("active");
  }
}

function showView(name) {
  $("view-login").hidden = name !== "login";
  const shell = $("shell");
  if (shell) shell.hidden = name === "login";
  $("view-main").hidden = name !== "main";
  $("view-files").hidden = name !== "files";
  $("view-admin").hidden = name !== "admin";
  setSidebarActive(name);
}

// Switch the visible view. For the admin view, also (re)render the requested
// admin section so a sidebar click lands on the right panel. The main and
// files views re-fetch their data on activation, so switching pages never
// shows a stale list (e.g. a session that started while you were on another
// page). The launch-options form is intentionally left alone so in-progress
// selections survive a page switch.
function activateView(view, adminSection) {
  if (adminSection) currentAdminSection = adminSection;
  // Remember where the user is so a refresh can restore this page. The admin
  // section is tracked separately because the sidebar only stores the top-level
  // view, not which admin panel is open.
  try {
    localStorage.setItem(
      "vreckan.lastView",
      JSON.stringify({ view, adminSection: view === "admin" ? currentAdminSection : undefined })
    );
  } catch (e) {
    /* storage unavailable — refresh will just land on Apps */
  }
  showView(view);
  if (view === "admin") activateAdminSection(currentAdminSection);
  else if (view === "main") refreshMainLists();
  else if (view === "files") loadHomedirs(true);
}

// Re-fetch the main view's data lists (app grid, active sessions, pinned
// launch options) without touching the launch-options form.
async function refreshMainLists() {
  try {
    const [appsData, sessionsData] = await Promise.all([
      secureFetch("/api/applications", { method: "POST", body: {} }),
      secureFetch("/api/sessions", { method: "GET" }),
    ]);
    installedApps = Array.isArray(appsData) ? appsData : [];
    renderAppApps(installedApps);
    renderSessions(Array.isArray(sessionsData) ? sessionsData : []);
    await loadPinned();
  } catch (e) {
    /* non-fatal — the lists refresh on the next full load */
  }
}

// ---------------------------------------------------------------------------
// App logo (mirrors popup.js formatLogoSrc)
// ---------------------------------------------------------------------------
async function formatLogoSrc(app) {
  const logo = app.logo || "";
  if (logo.startsWith("http://") || logo.startsWith("https://")) return logo;
  if (logo.startsWith("/api/app_icon/")) {
    try {
      const data = await secureFetch(logo, { method: "GET" });
      if (data && data.icon_data_b64) return `data:image/png;base64,${data.icon_data_b64}`;
    } catch (e) {
      /* fall through to default */
    }
  }
  return "/img/icon128.png";
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------
function renderAppCards(apps) {
  const grid = $("app-grid");
  grid.innerHTML = "";
  if (!apps.length) {
    grid.innerHTML = '<p class="muted">No apps installed.</p>';
    return;
  }
  for (const app of apps) {
    const card = document.createElement("div");
    card.className = "app-tile";
    card.dataset.appid = app.id;

    const img = document.createElement("img");
    img.alt = `${app.name} logo`;
    img.src = "/img/icon128.png";
    card.appendChild(img);
    formatLogoSrc(app).then((src) => {
      img.src = src;
    });

    const name = document.createElement("div");
    name.className = "app-name";
    name.textContent = app.name;
    card.appendChild(name);

    const meta = document.createElement("div");
    meta.className = "app-meta";
    if (app.is_meta_app) {
      const b = document.createElement("span");
      b.className = "badge lab";
      b.textContent = "Laboratory";
      meta.appendChild(b);
    }
    if (app.nvidia_support || app.dri3_support) {
      const b = document.createElement("span");
      b.className = "badge nvidia";
      b.textContent = "GPU";
      meta.appendChild(b);
    }
    if (app.url_support) {
      const b = document.createElement("span");
      b.className = "badge url";
      b.textContent = "URL";
      meta.appendChild(b);
    }
    if (app.home_directories) {
      const b = document.createElement("span");
      b.className = "badge files";
      b.textContent = "Files";
      meta.appendChild(b);
    }
    if (meta.children.length) card.appendChild(meta);

    card.addEventListener("click", () => handleAppSelection(app));
    grid.appendChild(card);
  }
}

function renderSessions(sessions) {
  const list = $("sessions-list");
  list.innerHTML = "";
  if (!sessions.length) {
    list.innerHTML = '<p class="muted">No active sessions.</p>';
    return;
  }
  for (const s of sessions) {
    const item = document.createElement("div");
    item.className = "session-item";

    const img = document.createElement("img");
    img.alt = "app logo";
    img.src = s.app_logo || "/img/icon128.png";
    if (s.app_logo && s.app_logo.startsWith("/api/app_icon/")) {
      formatLogoSrc({ logo: s.app_logo }).then((src) => {
        img.src = src;
      });
    }
    item.appendChild(img);

    const info = document.createElement("div");
    info.className = "session-info";
    const name = document.createElement("div");
    name.className = "app-name";
    name.textContent = s.name ? `${s.app_name} - ${s.name}` : s.app_name;
    info.appendChild(name);
    const metaLine = document.createElement("div");
    metaLine.className = "session-meta";
    const when = new Date(s.created_at * 1000);
    metaLine.textContent = when.toLocaleString();
    if (s.is_collaboration) metaLine.textContent += " · room mode";
    info.appendChild(metaLine);
    item.appendChild(info);

    const actions = document.createElement("div");
    actions.className = "session-actions";

    const open = document.createElement("a");
    open.className = "btn btn-ghost btn-sm";
    open.href = s.session_url;
    open.target = "_blank";
    open.rel = "noopener";
    open.textContent = "Open";
    actions.appendChild(open);

    const sendFile = document.createElement("button");
    sendFile.className = "btn btn-ghost btn-sm";
    sendFile.textContent = "Send File";
    sendFile.title = "Upload a file from your computer into this session";
    sendFile.addEventListener("click", () => pickFileForSession(s.session_id, sendFile));
    actions.appendChild(sendFile);

    const kill = document.createElement("button");
    kill.className = "btn btn-danger btn-sm";
    kill.textContent = "Kill";
    kill.addEventListener("click", async () => {
      try {
        await secureFetch(`/api/sessions/${s.session_id}`, { method: "DELETE" });
        toast("Session terminated");
        loadSessions();
      } catch (e) {
        toast(e.message, "error");
      }
    });
    actions.appendChild(kill);
    item.appendChild(actions);

    list.appendChild(item);
  }
}

async function loadSessions() {
  try {
    const sessions = await secureFetch("/api/sessions", { method: "GET" });
    renderSessions(Array.isArray(sessions) ? sessions : []);
  } catch (e) {
    if (!e.message.includes("401")) toast(`Sessions: ${e.message}`, "error");
  }
}

// ---------------------------------------------------------------------------
// Feat 9 — send a file to an active session
// ---------------------------------------------------------------------------
// "Send File" on a session row: open the shared file picker, then chunk-upload
// the chosen file and hand it to the session via /api/sessions/{id}/send_file.
// The button doubles as the progress indicator (Uploading i/n…).
function pickFileForSession(sessionId, button) {
  const input = $("session-send-file");
  input.value = "";
  input.onchange = () => {
    const file = input.files && input.files[0];
    input.onchange = null;
    if (!file) return;
    sendFileToSession(sessionId, file, button);
  };
  input.click();
}

async function sendFileToSession(sessionId, file, button) {
  const original = button.textContent;
  button.disabled = true;
  try {
    const { upload_id, total_chunks } = await uploadFileChunks(file, (done, total) => {
      button.textContent = `Uploading ${done}/${total}…`;
    });
    button.textContent = "Sending…";
    const res = await secureFetch(`/api/sessions/${sessionId}/send_file`, {
      method: "POST",
      body: { filename: file.name, upload_id, total_chunks },
    });
    toast(res && res.message ? res.message : `File '${file.name}' sent to session.`);
  } catch (e) {
    toast(e.message, "error");
  } finally {
    button.disabled = false;
    button.textContent = original;
  }
}

// ---------------------------------------------------------------------------
// Feat 10 — upload a file to shared storage
// ---------------------------------------------------------------------------
// "Upload a file to shared storage" in the launch-options card: pick a file
// from the computer, chunk-upload it, and have the server place it in the
// user's _vreckan_shared_files (usable as the launch "server file").
function pickFileForStorageUpload(button) {
  const input = $("storage-upload-input");
  input.value = "";
  input.onchange = () => {
    const file = input.files && input.files[0];
    input.onchange = null;
    if (!file) return;
    uploadToStorage(file, button);
  };
  input.click();
}

async function uploadToStorage(file, button) {
  const original = button.textContent;
  button.disabled = true;
  try {
    const { upload_id, total_chunks } = await uploadFileChunks(file, (done, total) => {
      button.textContent = `Uploading ${done}/${total}…`;
    });
    button.textContent = "Finalizing…";
    const res = await secureFetch("/api/upload/to_storage", {
      method: "POST",
      body: {
        filename: file.name,
        upload_id,
        total_chunks,
        home_name: "_vreckan_shared_files",
      },
    });
    toast(res && res.message ? res.message : `File '${file.name}' uploaded.`);
  } catch (e) {
    toast(e.message, "error");
  } finally {
    button.disabled = false;
    button.textContent = original;
  }
}

// ---------------------------------------------------------------------------
// Launch options + launch
// ---------------------------------------------------------------------------
let currentUser = null;
let availableGpus = [];

function populateLanguageDropdown() {
  const sel = $("opt-language");
  sel.innerHTML = "";
  for (const lang of SUPPORTED_LANGS) {
    const opt = document.createElement("option");
    opt.value = lang;
    opt.textContent = lang;
    sel.appendChild(opt);
  }
  sel.value = guessBrowserLang();
  if (!sel.value) sel.value = "en_US.UTF-8";
}

function populateGpuDropdown() {
  const field = $("field-gpu");
  const sel = $("opt-gpu");
  if (!availableGpus.length) {
    field.hidden = true;
    return;
  }
  sel.innerHTML = "";
  const none = document.createElement("option");
  none.value = "none";
  none.textContent = "Auto (no specific GPU)";
  sel.appendChild(none);
  for (const gpu of availableGpus) {
    const opt = document.createElement("option");
    opt.value = gpu.device;
    opt.textContent = `${gpu.device}${gpu.driver ? ` (${gpu.driver})` : ""}`;
    sel.appendChild(opt);
  }
  field.hidden = false;
}

function currentLaunchPayload(applicationId, extra) {
  return {
    application_id: applicationId,
    home_name: $("opt-home").value === "auto" ? "auto" : $("opt-home").value || null,
    language: $("opt-language").value,
    selected_gpu: $("field-gpu").hidden ? null : $("opt-gpu").value === "none" ? null : $("opt-gpu").value,
    launch_in_room_mode: $("opt-room-mode").checked,
    wayland_mode: $("opt-wayland").checked,
    ...(extra || {}),
  };
}

function openSession(result) {
  if (result && result.session_url) {
    window.open(result.session_url, "_blank", "noopener");
    toast("Session launched");
  } else {
    toast("Launch failed: no session URL returned", "error");
  }
  setTimeout(loadSessions, 1500);
}

async function handleAppSelection(app) {
  try {
    if (app.url_support) {
      const url = await promptModal({
        title: "Enter the URL to open",
        label: "URL",
        defaultValue: "https://",
      });
      if (!url) return;
      const result = await secureFetch("/api/launch/url", {
        method: "POST",
        body: currentLaunchPayload(app.id, { url }),
      });
      openSession(result);
      return;
    }

    if (app.home_directories) {
      // File-backed launch (Feat 12): open the launch modal so the user can
      // upload a local file to open, or open a file already in shared storage.
      openLaunchModal(app);
      return;
    }

    const result = await secureFetch("/api/launch/simple", {
      method: "POST",
      body: currentLaunchPayload(app.id),
    });
    openSession(result);
  } catch (e) {
    toast(e.message, "error");
  }
}

// ---------------------------------------------------------------------------
// Feat 12 — open file on launch
// ---------------------------------------------------------------------------
// Chunk size must match the server's upload CHUNK_SIZE (2 MiB).
const LAUNCH_CHUNK_SIZE = 2 * 1024 * 1024;

// The app the launch modal is currently open for (set by openLaunchModal).
let launchModalApp = null;

// Upload a File in 2 MiB chunks via /api/upload/initiate + /api/upload/chunk.
// Returns { upload_id, total_chunks } — the server reassembles from upload_id
// when /api/launch/file is called. (Unlike files.js's uploadFile, this does
// NOT call upload_to_dir: the launch endpoint consumes the upload directly.)
async function uploadFileChunks(file, onProgress) {
  const init = await secureFetch("/api/upload/initiate", {
    method: "POST",
    body: { filename: file.name, total_size: file.size },
  });
  const upload_id = init.upload_id;
  const totalChunks = Math.max(1, Math.ceil(file.size / LAUNCH_CHUNK_SIZE));
  for (let i = 0; i < totalChunks; i++) {
    const start = i * LAUNCH_CHUNK_SIZE;
    const end = Math.min(start + LAUNCH_CHUNK_SIZE, file.size);
    const buf = await file.slice(start, end).arrayBuffer();
    await secureFetch("/api/upload/chunk", {
      method: "POST",
      body: {
        upload_id,
        chunk_index: i, // 0-based
        chunk_data_b64: arrayBufferToBase64(buf),
      },
    });
    if (onProgress) onProgress(i + 1, totalChunks);
  }
  return { upload_id, total_chunks: totalChunks };
}

// Resolve the "auto" home selection to a concrete per-app persistent home
// (auto-<sanitized-app-name>), creating it if needed — mirroring the
// reference client. The backend 404s on a literal "auto" for file-backed
// apps, so the client must send a real dir name. Meta apps are skipped (the
// backend provisions their auto home from a template) and so is cleanroom /
// disabled persistent storage (the backend forces ephemeral storage then).
async function resolveAutoHome(app, homeName) {
  if (homeName !== "auto") return homeName;
  if (!currentSettings.persistent_storage) return homeName;
  if (app.is_meta_app) return homeName;
  const sanitized = app.name
    .toLowerCase()
    .replace(/[\s_]+/g, "-")
    .replace(/[^a-z0-9-]/g, "");
  const autoHomeName = `auto-${sanitized}`;
  const { home_dirs } = await secureFetch("/api/homedirs", { method: "GET" });
  if (!(home_dirs || []).includes(autoHomeName)) {
    await secureFetch("/api/homedirs", {
      method: "POST",
      body: { home_name: autoHomeName },
    });
  }
  return autoHomeName;
}

// Open the launch modal for a file-backed app, resetting its inputs.
function openLaunchModal(app) {
  launchModalApp = app;
  $("launch-title").textContent = `Launch ${app.name}`;
  $("launch-file").value = "";
  $("launch-server-file").value = "";
  $("launch-name").value = "";
  $("launch-open-on-launch").checked = false;
  syncLaunchFileFields();
  $("launch-error").hidden = true;
  $("launch-upload-status").hidden = true;
  $("launch-submit").disabled = false;
  $("modal-launch").hidden = false;
  // The default is a plain launch (no file), so focus the primary
  // action; the file pickers stay hidden until the toggle is on.
  $("launch-submit").focus();
}

// Show/hide the file pickers (and the subtitle) to match the
// "Open file on launch" toggle: off means there is no file to open,
// so the pickers are hidden and a launch is a plain app launch.
function syncLaunchFileFields() {
  const on = $("launch-open-on-launch").checked;
  $("launch-file-fields").hidden = !on;
  $("launch-sub").textContent = on
    ? "Pick a file to open, or enter a filename already in shared storage."
    : 'The app will launch without opening a file. Check "Open file on launch" to pick one.';
}

// Confirm the launch modal: upload the picked local file (if any) and launch,
// or launch with a file already in shared storage.
async function handleLaunchConfirm(app) {
  const fileInput = $("launch-file");
  const serverFileInput = $("launch-server-file");
  const openOnLaunch = $("launch-open-on-launch").checked;
  // The pickers are hidden while the toggle is off; ignore any stale
  // values so an unchecked launch is always a plain app launch.
  const file = openOnLaunch && fileInput.files ? fileInput.files[0] : null;
  const serverFile = openOnLaunch ? serverFileInput.value.trim() : "";
  const errEl = $("launch-error");
  const statusEl = $("launch-upload-status");
  const submitBtn = $("launch-submit");

  errEl.hidden = true;
  submitBtn.disabled = true;
  try {
    const payload = currentLaunchPayload(app.id);
    payload.session_name = $("launch-name").value.trim() || null;
    payload.home_name = await resolveAutoHome(app, payload.home_name);

    let result;
    if (file) {
      statusEl.hidden = false;
      statusEl.textContent = "Uploading…";
      const { upload_id, total_chunks } = await uploadFileChunks(file, (done, total) => {
        statusEl.textContent = `Uploading… ${done}/${total}`;
      });
      statusEl.textContent = "Launching…";
      result = await secureFetch("/api/launch/file", {
        method: "POST",
        body: {
          ...payload,
          filename: file.name,
          upload_id,
          total_chunks,
          open_file_on_launch: openOnLaunch,
        },
      });
    } else if (serverFile) {
      result = await secureFetch("/api/launch/file_path", {
        method: "POST",
        body: { ...payload, filename: serverFile },
      });
    } else {
      // No file chosen — the file is optional, so just launch the app as-is
      // (home dir mounted per the selected mode, nothing opened in it).
      result = await secureFetch("/api/launch/simple", {
        method: "POST",
        body: payload,
      });
    }
    $("modal-launch").hidden = true;
    openSession(result);
  } catch (e) {
    errEl.textContent = e.message;
    errEl.hidden = false;
  } finally {
    submitBtn.disabled = false;
    statusEl.hidden = true;
  }
}

// ---------------------------------------------------------------------------
// Feat 13 — self-service home directories
// ---------------------------------------------------------------------------
// The current user's effective settings (set in loadMain); read by the
// home-directory helpers so the Manage modal can refresh the selects.
let currentSettings = {};

// Populate the launch "Home directory" select: auto + cleanroom always, plus
// the user's persistent home dirs when persistent storage is enabled. The
// current selection is preserved if it still exists.
async function populateHomeSelect() {
  const homeSel = $("opt-home");
  if (!homeSel) return;
  const previous = homeSel.value;
  homeSel.innerHTML = "";
  const addOpt = (value, label) => {
    const opt = document.createElement("option");
    opt.value = value;
    opt.textContent = label;
    homeSel.appendChild(opt);
  };
  addOpt("auto", "Auto (default)");
  addOpt("cleanroom", "Cleanroom");
  if (currentSettings.persistent_storage) {
    try {
      const { home_dirs } = await secureFetch("/api/homedirs", { method: "GET" });
      for (const dir of home_dirs || []) {
        if (dir !== "_vreckan_shared_files" && !dir.startsWith("auto-")) {
          addOpt(dir, dir);
        }
      }
    } catch (e) {
      /* persistent storage may be unavailable; auto/cleanroom remain */
    }
  }
  if ([...homeSel.options].some((o) => o.value === previous)) {
    homeSel.value = previous;
  }
}

// Re-fetch home dirs into both the launch select and the Files view.
async function refreshHomeSelects() {
  await populateHomeSelect();
  try {
    await loadHomedirs(true);
  } catch (e) {
    /* non-fatal: the Files view refreshes on its next load */
  }
}

// Self-service modal: list, create, and delete the user's home directories.
function openHomeDirsModal() {
  const { box, close } = openCustomModal({
    title: "Home directories",
    wide: true,
    body: `
      <p class="muted">Create or delete your persistent home directories. New ones appear in the launch options and the Files view.</p>
      <div id="homedirs-list"></div>
      <hr>
      <div class="field-inline">
        <input type="text" id="homedir-new" placeholder="Name (letters, numbers, - or _)" pattern="[a-zA-Z0-9_-]+">
        <button class="btn btn-primary" id="homedir-create" type="button">Create</button>
      </div>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" data-act="homedirs-close">Close</button>
      </div>`,
  });

  const listEl = box.querySelector("#homedirs-list");
  const input = box.querySelector("#homedir-new");
  const createBtn = box.querySelector("#homedir-create");
  box.querySelector('[data-act="homedirs-close"]').addEventListener("click", () => close());

  const refresh = async () => {
    try {
      const { home_dirs } = await secureFetch("/api/homedirs", { method: "GET" });
      const dirs = (home_dirs || []).filter(
        (d) => d !== "_vreckan_shared_files" && !d.startsWith("auto-")
      );
      if (!dirs.length) {
        listEl.innerHTML = `<p class="muted">No home directories yet — create one below.</p>`;
        return;
      }
      listEl.innerHTML = dirs
        .map(
          (d) => `<div class="homedir-row">
            <span>${esc(d)}</span>
            <span class="homedir-row-actions">
              <button class="btn btn-ghost btn-sm" data-manage="${esc(d)}" type="button">Manage</button>
              <button class="btn btn-ghost btn-sm" data-del="${esc(d)}" type="button">Delete</button>
            </span>
          </div>`
        )
        .join("");
      listEl.querySelectorAll("[data-manage]").forEach((btn) => {
        btn.addEventListener("click", async () => {
          const name = btn.getAttribute("data-manage");
          close();
          try {
            await selectHomeDir(name);
            showView("files");
          } catch (e) {
            toast(`Could not open '${name}': ${e.message}`, "error");
          }
        });
      });
      listEl.querySelectorAll("[data-del]").forEach((btn) => {
        btn.addEventListener("click", async () => {
          const name = btn.getAttribute("data-del");
          const ok = await confirmModal(
            `Delete home directory '${name}'? Its files will be removed.`,
            { title: "Delete home directory", confirmLabel: "Delete", danger: true }
          );
          if (ok) {
            try {
              await secureFetch(`/api/homedirs/${encodeURIComponent(name)}`, { method: "DELETE" });
              toast(`Deleted '${name}'.`);
              await refreshHomeSelects();
            } catch (e) {
              toast(`Delete failed: ${e.message}`, "error");
            }
          }
          // The confirm dialog shares the modal root and replaces this
          // modal's content, so re-open it (refreshed) in either case.
          openHomeDirsModal();
        });
      });
    } catch (e) {
      listEl.innerHTML = `<p class="error">Failed to load: ${esc(e.message)}</p>`;
    }
  };

  const doCreate = async () => {
    const name = input.value.trim();
    if (!name) {
      toast("Enter a directory name.", "error");
      return;
    }
    if (!/^[a-zA-Z0-9_-]+$/.test(name)) {
      toast("Use letters, numbers, underscore, or hyphen only.", "error");
      return;
    }
    createBtn.disabled = true;
    try {
      await secureFetch("/api/homedirs", { method: "POST", body: { home_name: name } });
      toast(`Created '${name}'.`);
      input.value = "";
      await refresh();
      await refreshHomeSelects();
    } catch (e) {
      toast(`Create failed: ${e.message}`, "error");
    } finally {
      createBtn.disabled = false;
    }
  };

  createBtn.addEventListener("click", doCreate);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") doCreate();
  });

  refresh();
}

// ---------------------------------------------------------------------------
// Feat 7 — pinned launch options ("save these launch options")
// ---------------------------------------------------------------------------
// Installed apps (set in loadMain); used to resolve app names in the pinned
// list and to populate the save-preset app dropdown.
let installedApps = [];
// The user's saved presets (set by loadPinned).
let pinnedBehaviors = [];

// Fetch + render the user's pinned presets.
async function loadPinned() {
  try {
    pinnedBehaviors = (await secureFetch("/api/pinned", { method: "GET" })) || [];
  } catch (e) {
    pinnedBehaviors = [];
  }
  renderPinned();
}

function renderPinned() {
  const section = $("pinned-section");
  const list = $("pinned-list");
  if (!section || !list) return;
  if (!pinnedBehaviors.length) {
    section.hidden = true;
    list.innerHTML = "";
    return;
  }
  section.hidden = false;
  const appName = (id) => {
    const app = installedApps.find((a) => a.id === id);
    return app ? app.name : "Unknown app";
  };
  const triggerText = (p) => {
    if (p.trigger_type === "all_urls") return "all URLs";
    if (p.trigger_type === "file_extension") return `*.${p.trigger_value || "?"}`;
    return "manual";
  };
  list.innerHTML = pinnedBehaviors
    .map(
      (p) => `
      <div class="pinned-row">
        <div class="pinned-info">
          <strong>${esc(p.name)}${p.is_default ? ' <span class="default-badge" title="Restored on every login and used across all apps">★ Default</span>' : ''}</strong>
          <span class="muted">${p.is_default ? "All apps" : esc(appName(p.application_id))} · ${esc(triggerText(p))}</span>
        </div>
        <div class="pinned-actions">
          ${p.is_default ? "" : `<button class="btn btn-ghost btn-sm" data-makedefault="${esc(p.id)}" type="button">Make default</button>`}
          <button class="btn btn-ghost btn-sm" data-apply="${esc(p.id)}" type="button">Apply</button>
          <button class="btn btn-ghost btn-sm" data-del="${esc(p.id)}" type="button">Delete</button>
        </div>
      </div>`
    )
    .join("");
  list.querySelectorAll("[data-apply]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const p = pinnedBehaviors.find((x) => x.id === btn.getAttribute("data-apply"));
      if (p) applyPreset(p);
    });
  });
  list.querySelectorAll("[data-makedefault]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const id = btn.getAttribute("data-makedefault");
      try {
        await secureFetch(`/api/pinned/${id}/set-default`, { method: "POST" });
        await loadPinned();
        // Reflect the new default in the launch controls immediately.
        const newDefault = pinnedBehaviors.find((x) => x.id === id);
        if (newDefault) applyPreset(newDefault, true);
      } catch (e) {
        toast(`Could not set default: ${e.message}`, "error");
      }
    });
  });
  list.querySelectorAll("[data-del]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const p = pinnedBehaviors.find((x) => x.id === btn.getAttribute("data-del"));
      if (!p) return;
      const ok = await confirmModal(`Delete preset '${p.name}'?`, {
        title: "Delete preset",
        confirmLabel: "Delete",
        danger: true,
      });
      if (!ok) return;
      try {
        await secureFetch(`/api/pinned/${p.id}`, { method: "DELETE" });
        toast(`Deleted '${p.name}'`);
        await loadPinned();
      } catch (e) {
        toast(`Delete failed: ${e.message}`, "error");
      }
    });
  });
}

// Apply a preset: fill the launch-option controls with the saved values.
// Options whose saved value no longer exists (e.g. a removed home dir or GPU)
// are skipped so the controls keep a valid value. When `silent` is true the
// values are applied without a toast (used to restore the user's default
// preset on page load).
function applyPreset(p, silent = false) {
  const homeSel = $("opt-home");
  const langSel = $("opt-language");
  const gpuField = $("field-gpu");
  const gpuSel = $("opt-gpu");
  const roomChk = $("opt-room-mode");
  const waylandChk = $("opt-wayland");
  if (homeSel && p.home_name && Array.from(homeSel.options).some((o) => o.value === p.home_name)) {
    homeSel.value = p.home_name;
  }
  if (langSel && p.language && Array.from(langSel.options).some((o) => o.value === p.language)) {
    langSel.value = p.language;
  }
  if (gpuSel && gpuField && !gpuField.hidden && p.selected_gpu && Array.from(gpuSel.options).some((o) => o.value === p.selected_gpu)) {
    gpuSel.value = p.selected_gpu;
  }
  if (roomChk) roomChk.checked = !!p.launch_in_room_mode;
  if (waylandChk) waylandChk.checked = p.wayland_mode !== false;
  if (!silent) toast(`Applied preset '${p.name}'`);
}

// Open the "save these launch options" modal (captures the current launch
// options, tied to a chosen app + optional informational trigger).
function openSavePresetModal() {
  if (!installedApps.length) {
    toast("Install an app first to save a preset.", "error");
    return;
  }
  const { box, close } = openCustomModal({
    title: "Save launch options",
    body: `
      <p class="muted">Save the current launch options as a reusable preset tied to an app. Re-apply it later from the Pinned list.</p>
      <div class="field">
        <label for="preset-name">Preset name</label>
        <input type="text" id="preset-name" maxlength="128" placeholder="e.g. Calibre with my settings">
      </div>
      <div class="field">
        <label for="preset-app">App</label>
        <select id="preset-app">
          ${installedApps.map((a) => `<option value="${esc(a.id)}">${esc(a.name)}</option>`).join("")}
        </select>
      </div>
      <div class="field">
        <label for="preset-trigger">Trigger (informational)</label>
        <select id="preset-trigger">
          <option value="manual" selected>Manual (apply from the Pinned list)</option>
          <option value="all_urls">All URLs</option>
          <option value="file_extension">File extension</option>
        </select>
      </div>
      <div class="field" id="preset-ext-field" hidden>
        <label for="preset-ext">File extension</label>
        <input type="text" id="preset-ext" placeholder="e.g. epub">
      </div>
      <div class="field">
        <label class="check"><input type="checkbox" id="preset-default"> Set as my default</label>
        <p class="muted" id="preset-default-hint">Your default preset is restored on every login and its launch options (GPU, home directory, language, …) are used across <strong>all</strong> apps, not just the one it was saved for. Only one preset can be your default.</p>
      </div>
      <p id="preset-error" class="error" role="alert" hidden></p>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" data-act="preset-cancel">Cancel</button>
        <button type="button" class="btn btn-primary" data-act="preset-save">Save</button>
      </div>`,
  });
  box.querySelector('[data-act="preset-cancel"]').addEventListener("click", () => close());
  const triggerSel = box.querySelector("#preset-trigger");
  const extField = box.querySelector("#preset-ext-field");
  triggerSel.addEventListener("change", () => {
    extField.hidden = triggerSel.value !== "file_extension";
  });
  box.querySelector('[data-act="preset-save"]').addEventListener("click", async () => {
    const name = box.querySelector("#preset-name").value.trim();
    const appId = box.querySelector("#preset-app").value;
    const triggerType = triggerSel.value;
    const triggerValue =
      triggerSel.value === "file_extension" ? box.querySelector("#preset-ext").value.trim().replace(/^\./, "") : "";
    const errEl = box.querySelector("#preset-error");
    if (!name) {
      errEl.textContent = "Enter a preset name.";
      errEl.hidden = false;
      return;
    }
    if (triggerType === "file_extension" && !triggerValue) {
      errEl.textContent = "Enter a file extension.";
      errEl.hidden = false;
      return;
    }
    errEl.hidden = true;
    try {
      await secureFetch("/api/pinned", {
        method: "POST",
        body: {
          name,
          application_id: appId,
          home_name: $("opt-home").value || null,
          language: $("opt-language").value,
          selected_gpu: $("field-gpu").hidden ? null : $("opt-gpu").value === "none" ? null : $("opt-gpu").value,
          launch_in_room_mode: $("opt-room-mode").checked,
          wayland_mode: $("opt-wayland").checked,
          trigger_type: triggerType,
          trigger_value: triggerValue,
          is_default: $("preset-default").checked,
        },
      });
      toast(`Saved preset '${name}'`);
      close();
      await loadPinned();
    } catch (e) {
      errEl.textContent = e.message;
      errEl.hidden = false;
    }
  });
  box.querySelector("#preset-name").focus();
}

// ---------------------------------------------------------------------------
// Main data load (mirrors popup.js parallel load)
// ---------------------------------------------------------------------------
async function loadMain() {
  const [statusData, appsData, sessionsData] = await Promise.all([
    secureFetch("/api/admin/status", { method: "POST", body: {} }),
    secureFetch("/api/applications", { method: "POST", body: {} }),
    secureFetch("/api/sessions", { method: "GET" }),
  ]);

  const settings = (statusData && statusData.settings) || {};
  currentSettings = settings;
  availableGpus = (statusData && statusData.gpus) || [];
  installedApps = Array.isArray(appsData) ? appsData : [];

  $("user-label").textContent = statusData.username || currentUser.username;

  // Home directory dropdown: auto + cleanroom always; persistent dirs if enabled.
  await populateHomeSelect();
  // Self-service "Manage" button: only shown when the user has persistent storage.
  const manageBtn = $("btn-manage-homedirs");
  if (manageBtn) manageBtn.hidden = !settings.persistent_storage;
  // Upload-to-storage button (Feat 10): only meaningful with persistent storage.
  const uploadStorageBtn = $("btn-upload-storage");
  if (uploadStorageBtn) uploadStorageBtn.hidden = !settings.persistent_storage;

  populateGpuDropdown();
  populateLanguageDropdown();
  renderAppApps(appsData);
  renderSessions(Array.isArray(sessionsData) ? sessionsData : []);
  // Pinned launch options (Feat 7).
  await loadPinned();

  // Restore the user's default preset (if any) so their saved launch options
  // (GPU, home directory, language, …) are remembered between sessions and are
  // used across every app, not just the one the preset was originally saved for.
  const defaultPreset = pinnedBehaviors.find((p) => p.is_default);
  if (defaultPreset) {
    applyPreset(defaultPreset, true);
  } else if (
    statusData.global_default_gpu &&
    !$("field-gpu").hidden &&
    availableGpus.some((g) => g.device === statusData.global_default_gpu)
  ) {
    // No personal default preset — fall back to the admin's global default GPU.
    $("opt-gpu").value = statusData.global_default_gpu;
  }

  // File manager tab: only available when persistent storage is enabled.
  if (settings.persistent_storage) {
    showFilesTab(true);
    try {
      await loadHomedirs();
    } catch (e) {
      toast(`Failed to load home directories: ${e.message}`, "error");
    }
  } else {
    showFilesTab(false);
  }

  // Admin console: visible when the user holds at least one admin permission
  // (the "admin" super-permission expands to all of them). Each individual
  // section is then shown/hidden by its own admin.<section> permission.
  showAdminTab(
    !!statusData && showAdminTab.hasAnyAdminPermission(statusData.permissions),
    statusData && statusData.permissions
  );
}

function renderAppApps(apps) {
  renderAppCards(Array.isArray(apps) ? apps : []);
}

// ---------------------------------------------------------------------------
// Auth flow
// ---------------------------------------------------------------------------
async function enterMain() {
  try {
    await ensureSession();
  } catch (e) {
    toast(`Secure channel setup failed: ${e.message}`, "error");
    return false;
  }
  showView("main");
  try {
    await loadMain();
  } catch (e) {
    toast(`Failed to load: ${e.message}`, "error");
  }
  // loadMain() has now revealed the Files / admin sidebar entries the user is
  // allowed to see, so it is safe to restore the page they were on before the
  // refresh (see restoreLastView). A fresh login has no stored view and stays
  // on Apps.
  restoreLastView();
  return true;
}

// Remember the page the user is on so a browser refresh lands back on it
// instead of always bouncing to Apps. Stored per browser in localStorage as
// { view, adminSection } and cleared on logout. On restore we only honour a
// view the user is actually allowed to see (Files needs persistent storage,
// admin sections need the admin role) and fall back to Apps otherwise.
function restoreLastView() {
  let stored = null;
  try {
    stored = JSON.parse(localStorage.getItem("vreckan.lastView"));
  } catch (e) {
    /* corrupt / unavailable storage — just stay on Apps */
  }
  if (!stored || typeof stored.view !== "string") return;
  // The unified roster was renamed from "people" to "accounts"; remap any
  // stored value from the old name so a refresh still lands on the right page.
  if (stored.adminSection === "people") stored.adminSection = "accounts";
  const hasAnyAdmin = showAdminTab.hasAnyAdminPermission(
    currentUser?.permissions
  );
  if (stored.view === "admin") {
    if (!hasAnyAdmin) return;
    // Only restore a section the user is actually permitted to see (its
    // sidebar link is not hidden); otherwise fall back to Overview.
    const section =
      stored.adminSection &&
      $("sidebar-nav").querySelector(
        `.nav-link[data-admin-section="${stored.adminSection}"]:not([hidden])`
      )
        ? stored.adminSection
        : "overview";
    activateView("admin", section);
  } else if (stored.view === "files") {
    if ($("tab-files").hidden) return;
    activateView("files");
  }
  // "main" (or anything else) is already the default view.
}

async function checkSso() {
  // Reveal the SSO button only when the server reports OIDC is enabled.
  try {
    const res = await fetch("/api/auth/oidc/status", { credentials: "include" });
    if (!res.ok) return;
    const data = await res.json();
    if (data && data.enabled) {
      const divider = $("login-sso-divider");
      const btn = $("login-sso");
      if (divider) divider.hidden = false;
      if (btn) btn.hidden = false;
    }
  } catch (e) {
    /* network error — leave SSO hidden */
  }
}

async function init() {
  // Wire login form
  $("login-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    clearError($("login-error"));
    const btn = $("login-submit");
    btn.disabled = true;
    try {
      currentUser = await login(
        $("login-username").value.trim(),
        $("login-password").value
      );
      $("login-password").value = "";
      if (!(await enterMain())) return;
    } catch (err) {
      showError($("login-error"), err.message);
    } finally {
      btn.disabled = false;
    }
  });

  // SSO / OIDC sign-in (only shown when the server has OIDC enabled)
  const ssoBtn = $("login-sso");
  if (ssoBtn) {
    ssoBtn.addEventListener("click", () => {
      ssoBtn.disabled = true;
      ssoBtn.textContent = "Redirecting…";
      window.location.href = "/api/auth/oidc/authorize";
    });
    checkSso();
  }

  // Logout
  $("btn-logout").addEventListener("click", async () => {
    try {
      await logout();
    } catch (e) {
      /* session may already be gone */
    }
    resetSession();
    try {
      localStorage.removeItem("vreckan.lastView");
    } catch (e) {
      /* non-fatal */
    }
    showView("login");
    toast("Logged out");
  });

  // Change password modal
  $("btn-change-password").addEventListener("click", () => {
    clearError($("cp-error"));
    $("modal-password").hidden = false;
    $("cp-old").focus();
  });
  $("cp-cancel").addEventListener("click", () => {
    $("modal-password").hidden = true;
  });
  $("change-password-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    clearError($("cp-error"));
    const oldPw = $("cp-old").value;
    const newPw = $("cp-new").value;
    const confirm = $("cp-confirm").value;
    if (newPw.length < 8) return showError($("cp-error"), "New password must be at least 8 characters.");
    if (newPw !== confirm) return showError($("cp-error"), "New passwords do not match.");
    try {
      await changePassword(oldPw, newPw);
      $("modal-password").hidden = true;
      $("change-password-form").reset();
      toast("Password updated");
    } catch (err) {
      showError($("cp-error"), err.message);
    }
  });

  // File manager view (listens exist regardless of active tab)
  initFilesView();

  // Self-service home-directory management (Feat 13)
  $("btn-manage-homedirs").addEventListener("click", () => openHomeDirsModal());

  // Launch modal for file-backed apps (Feat 12 — open file on launch)
  $("launch-cancel").addEventListener("click", () => {
    $("modal-launch").hidden = true;
  });
  $("launch-open-on-launch").addEventListener("change", () => {
    syncLaunchFileFields();
  });
  $("launch-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    if (launchModalApp) await handleLaunchConfirm(launchModalApp);
  });

  // Pinned launch options (Feat 7)
  $("btn-save-preset").addEventListener("click", () => openSavePresetModal());

  // Upload to shared storage (Feat 10)
  $("btn-upload-storage").addEventListener("click", () =>
    pickFileForStorageUpload($("btn-upload-storage"))
  );

  // Files view "Open in app" launches a session — keep the session list fresh.
  window.addEventListener("vreckan:sessions-changed", () => loadSessions());

  // App Laboratory (Feat 6) creates/updates apps — keep the grid + pinned list fresh.
  window.addEventListener("vreckan:apps-changed", async () => {
    try {
      const appsData = await secureFetch("/api/applications", { method: "POST", body: {} });
      installedApps = Array.isArray(appsData) ? appsData : [];
      renderAppApps(installedApps);
      await loadPinned();
    } catch (e) {
      /* non-fatal — the grid refreshes on the next full load */
    }
  });

  // Admin console view (hidden unless admin; listeners wired regardless)
  initAdminView();

  // Persistent sidebar navigation (Feat 5) — replaces the old topbar tabs.
  const sidebarNav = $("sidebar-nav");
  if (sidebarNav) {
    sidebarNav.addEventListener("click", (e) => {
      const item = e.target.closest("button[data-view], button[data-admin-section]");
      if (!item) return;
      if (item.dataset.view) {
        if (item.id === "tab-files" && item.hidden) return;
        activateView(item.dataset.view);
      } else if (item.dataset.adminSection) {
        if (item.hidden) return;
        activateView("admin", item.dataset.adminSection);
      }
    });
  }
  // The brand header doubles as a "home" link back to the Apps screen.
  $("brand-home")?.addEventListener("click", () => activateView("main"));

  // Try to restore an existing session (cookie)
  try {
    currentUser = await me();
    // SSO users authenticate through the identity provider, so their password
    // is managed externally — disable the self-service change-password control.
    if (currentUser && currentUser.is_sso) {
      const cpBtn = $("btn-change-password");
      if (cpBtn) {
        cpBtn.disabled = true;
        cpBtn.title = "Your password is managed by your SSO provider.";
      }
    }
    await enterMain();
  } catch (e) {
    showView("login");
  }
}

init();

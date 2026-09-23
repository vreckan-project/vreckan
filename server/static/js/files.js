// File manager (Phase 2) — full parity with browser extension.
// All data calls go through secureFetch (E2EE + X-Session-ID). Chunk indices are 0-BASED.

import { secureFetch } from "./api.js";
import { arrayBufferToBase64, base64ToArrayBuffer } from "./crypto.js";
import { openCustomModal, confirmModal, promptModal, escModal } from "./modal.js";

const CHUNK_SIZE = 2 * 1024 * 1024; // must match server CHUNK_SIZE
const PER_PAGE = 50;

const state = {
  home: "",
  path: "/",
  page: 1,
  total: 0,
  items: [],
  selected: new Set(),
};

let homeDirs = [];
let persistentStorage = false;
let currentSharePath = null;
let sharesCache = [];

function $(id) {
  return document.getElementById(id);
}

function toast(message, kind) {
  const el = document.createElement("div");
  el.className = `toast ${kind || ""}`.trim();
  el.textContent = message;
  $("toasts").appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

function fmtSize(bytes) {
  if (bytes === null || bytes === undefined || bytes === "") return "";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let val = bytes;
  let u = -1;
  do {
    val /= 1024;
    u++;
  } while (val >= 1024 && u < units.length - 1);
  return `${val.toFixed(val >= 100 ? 0 : 1)} ${units[u]}`;
}

function fmtTime(mtime) {
  if (!mtime) return "";
  return new Date(mtime * 1000).toLocaleString();
}

function isExcludedHome(name) {
  return name === "_vreckan_shared_files" || name.startsWith("auto-");
}

function parentPath(path) {
  if (path === "/" || path === "") return "/";
  const trimmed = path.endsWith("/") ? path.slice(0, -1) : path;
  const idx = trimmed.lastIndexOf("/");
  if (idx <= 0) return "/";
  return trimmed.slice(0, idx);
}

let homedirsLoaded = false;
export async function loadHomedirs(force = false) {
  // `force` re-fetches even when already loaded (used after a home dir is
  // created/deleted so the dropdown stays in sync without a full page reload).
  if (!force && homedirsLoaded && homeDirs.length) return;
  homedirsLoaded = false;
  const res = await secureFetch("/api/homedirs", { method: "GET" });
  const dirs = (res && res.home_dirs) || [];
  homeDirs = dirs.filter((d) => !isExcludedHome(d));
  const sel = $("fm-home");
  const previous = state.home;
  sel.innerHTML = "";
  for (const d of homeDirs) {
    const opt = document.createElement("option");
    opt.value = d;
    opt.textContent = d;
    sel.appendChild(opt);
  }
  if (homeDirs.length) {
    // Preserve the current selection if it still exists; otherwise pick the first.
    state.home = homeDirs.includes(previous) ? previous : homeDirs[0];
    sel.value = state.home;
  } else {
    state.home = null;
  }
  homedirsLoaded = true;
  await loadFiles();
}

// Switch the Files view to a specific home directory (used by the
// self-service "Manage" button in the home-dirs modal). Populates the
// dropdown first if it doesn't already contain the target.
export async function selectHomeDir(homeName) {
  const sel = $("fm-home");
  if (![...sel.options].some((o) => o.value === homeName)) {
    state.home = homeName; // so loadHomedirs preserves the target selection
    await loadHomedirs(true);
  }
  if (![...sel.options].some((o) => o.value === homeName)) {
    throw new Error(`Home directory '${homeName}' is not available.`);
  }
  // Mirror the #fm-home change handler: switch dir and reset the view.
  state.home = homeName;
  state.path = "/";
  state.page = 1;
  state.selected.clear();
  sel.value = homeName;
  await loadFiles();
}

function renderBreadcrumb() {
  const crumbEl = $("fm-breadcrumb");
  crumbEl.innerHTML = "";

  const makeCrumb = (label, path, isLast) => {
    if (isLast) {
      const span = document.createElement("span");
      span.className = "crumb current";
      span.textContent = label;
      crumbEl.appendChild(span);
    } else {
      const a = document.createElement("a");
      a.className = "crumb";
      a.textContent = label;
      a.addEventListener("click", () => {
        state.path = path;
        state.page = 1;
        state.selected.clear();
        loadFiles();
      });
      crumbEl.appendChild(a);
      const sep = document.createElement("span");
      sep.className = "sep";
      sep.textContent = "/";
      crumbEl.appendChild(sep);
    }
  };

  if (state.path === "/") {
    makeCrumb(state.home || "/", "/", true);
    return;
  }

  makeCrumb(state.home || "/", "/", false);
  const parts = state.path.split("/").filter(Boolean);
  let acc = "";
  parts.forEach((p, i) => {
    acc += "/" + p;
    makeCrumb(p, acc, i === parts.length - 1);
  });
}

function renderTable() {
  const body = $("fm-body");
  body.innerHTML = "";
  $("fm-empty").hidden = state.items.length > 0;

  for (const item of state.items) {
    const tr = document.createElement("tr");
    if (item.is_dir) tr.className = "is-dir";

    const tdSel = document.createElement("td");
    tdSel.className = "fm-col-sel";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.disabled = item.is_dir ? false : false;
    cb.checked = state.selected.has(item.path);
    cb.addEventListener("change", () => {
      if (cb.checked) state.selected.add(item.path);
      else state.selected.delete(item.path);
      tr.classList.toggle("selected", cb.checked);
      updateSelectionButtons();
    });
    tdSel.appendChild(cb);
    tr.appendChild(tdSel);

    const tdName = document.createElement("td");
    const nameWrap = document.createElement("div");
    nameWrap.className = "fm-name";
    const icon = document.createElement("span");
    icon.className = "icon";
    icon.textContent = item.is_dir ? "📁" : "📄";
    const nm = document.createElement("span");
    nm.textContent = item.name;
    nameWrap.appendChild(icon);
    nameWrap.appendChild(nm);
    tdName.appendChild(nameWrap);
    tr.appendChild(tdName);

    const tdSize = document.createElement("td");
    tdSize.textContent = item.is_dir ? "" : fmtSize(item.size);
    tr.appendChild(tdSize);

    const tdMod = document.createElement("td");
    tdMod.textContent = item.is_dir ? "" : fmtTime(item.mtime);
    tr.appendChild(tdMod);

    if (item.is_dir) {
      tr.addEventListener("click", (e) => {
        if (e.target === cb) return;
        state.path = item.path;
        state.page = 1;
        state.selected.clear();
        loadFiles();
      });
    }
    body.appendChild(tr);
  }

  $("fm-select-all").checked =
    state.items.length > 0 && state.items.every((i) => state.selected.has(i.path));
  renderBreadcrumb();
  renderPager();
  updateSelectionButtons();
}

function renderPager() {
  const totalPages = Math.max(1, Math.ceil(state.total / PER_PAGE));
  $("fm-page-info").textContent = `Page ${state.page} of ${totalPages} · ${state.total} items`;
  $("fm-prev").disabled = state.page <= 1;
  $("fm-next").disabled = state.page >= totalPages;
}

function updateSelectionButtons() {
  const n = state.selected.size;
  $("fm-download").disabled = n !== 1;
  $("fm-share").disabled = n !== 1;
  $("fm-open-in-app").disabled = n !== 1;
  $("fm-delete").disabled = n < 1;
}

async function loadFiles() {
  if (!state.home) return;
  try {
    const qs = `path=${encodeURIComponent(state.path)}&page=${state.page}&per_page=${PER_PAGE}`;
    const res = await secureFetch(`/api/files/list/${encodeURIComponent(state.home)}?${qs}`, {
      method: "GET",
    });
    state.items = (res && res.items) || [];
    state.total = (res && res.total) || 0;
    state.selected.clear();
    renderTable();
  } catch (err) {
    toast(err.message, "error");
  }
}

// ----- Upload (0-based chunk indices) -----
function progressItem(filename) {
  const wrap = $("fm-progress");
  wrap.hidden = false;
  const item = document.createElement("div");
  item.className = "fm-progress-item";
  item.innerHTML = `
    <div class="fm-progress-label"><span>${filename}</span><span class="pct">0%</span></div>
    <div class="fm-progress-track"><div class="fm-progress-fill"></div></div>`;
  wrap.appendChild(item);
  return {
    item,
    fill: item.querySelector(".fm-progress-fill"),
    pct: item.querySelector(".pct"),
    set(p) {
      const clamped = Math.max(0, Math.min(100, p));
      this.fill.style.width = clamped + "%";
      this.pct.textContent = Math.round(clamped) + "%";
    },
    done() {
      this.item.classList.add("done");
      this.fill.style.width = "100%";
      this.pct.textContent = "100%";
    },
    error() {
      this.item.classList.add("error");
      this.fill.style.width = "100%";
      this.pct.textContent = "Failed";
    },
  };
}

async function uploadFile(file) {
  const prog = progressItem(file.name);
  try {
    const init = await secureFetch("/api/upload/initiate", {
      method: "POST",
      body: { filename: file.name, total_size: file.size },
    });
    const upload_id = init.upload_id;

    const totalChunks = Math.max(1, Math.ceil(file.size / CHUNK_SIZE));
    for (let i = 0; i < totalChunks; i++) {
      const start = i * CHUNK_SIZE;
      const end = Math.min(start + CHUNK_SIZE, file.size);
      const buf = await file.slice(start, end).arrayBuffer();
      await secureFetch("/api/upload/chunk", {
        method: "POST",
        body: {
          upload_id,
          chunk_index: i, // 0-based
          chunk_data_b64: arrayBufferToBase64(buf),
        },
      });
      prog.set(((i + 1) / totalChunks) * 100);
    }

    await secureFetch(`/api/files/upload_to_dir/${encodeURIComponent(state.home)}`, {
      method: "POST",
      body: {
        path: state.path,
        filename: file.name,
        upload_id,
        total_chunks: totalChunks,
      },
    });
    prog.done();
    toast(`Uploaded ${file.name}`, "success");
  } catch (err) {
    prog.error();
    toast(`Upload failed: ${file.name}`, "error");
  }
}

async function handleUpload(files) {
  for (const f of files) {
    await uploadFile(f);
  }
  setTimeout(() => {
    $("fm-progress").innerHTML = "";
    $("fm-progress").hidden = true;
  }, 1500);
  loadFiles();
}

// ----- Download -----
async function downloadFile(path, filename) {
  try {
    const parts = [];
    let n = 0;
    for (;;) {
      const res = await secureFetch(
        `/api/files/download/chunk/${encodeURIComponent(state.home)}?path=${encodeURIComponent(path)}&chunk_index=${n}`,
        { method: "GET" }
      );
      parts.push(base64ToArrayBuffer(res.chunk_data_b64));
      if (res.is_last_chunk) break;
      n++;
    }
    const blob = new Blob(parts);
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
  } catch (err) {
    toast(err.message, "error");
  }
}

// ----- New folder -----
async function newFolder() {
  const name = await promptModal({ title: "New folder", label: "Name" });
  if (!name) return;
  try {
    await secureFetch(`/api/files/create_folder/${encodeURIComponent(state.home)}`, {
      method: "POST",
      body: { path: state.path, folder_name: name },
    });
    toast(`Created ${name}`, "success");
    loadFiles();
  } catch (err) {
    toast(err.message, "error");
  }
}

// ----- Delete (async task polling) -----
async function deleteSelected() {
  const paths = [...state.selected];
  if (!paths.length) return;
  if (!(await confirmModal(`Delete ${paths.length} item(s)? This cannot be undone.`, { danger: true }))) return;
  let task;
  try {
    task = await secureFetch(`/api/files/delete/${encodeURIComponent(state.home)}`, {
      method: "POST",
      body: { paths },
    });
  } catch (err) {
    toast(err.message, "error");
    return;
  }
  const deadline = Date.now() + 60000;
  let status;
  while (Date.now() < deadline) {
    try {
      status = await secureFetch(`/api/files/delete_status/${encodeURIComponent(task.task_id)}`, {
        method: "GET",
      });
      if (status.status === "completed" || status.status === "error") break;
    } catch (err) {
      toast(err.message, "error");
      return;
    }
    await new Promise((r) => setTimeout(r, 1200));
  }
  if (status && status.status === "completed") {
    toast("Deleted", "success");
    state.selected.clear();
    loadFiles();
  } else if (status && status.status === "error") {
    toast(status.message || "Delete failed", "error");
  } else {
    toast("Delete timed out — check later", "error");
  }
}

// ----- Open in app (Feat 11) -----
async function openInAppModal() {
  const paths = [...state.selected];
  if (paths.length !== 1) return;
  const path = paths[0];
  const item = state.items.find((i) => i.path === path);
  if (!item) return;
  if (item.is_dir) {
    toast("Only files can be opened in an application.", "error");
    return;
  }

  let fileApps;
  try {
    const apps = await secureFetch("/api/applications", { method: "POST", body: {} });
    fileApps = (apps || []).filter((a) => a.home_directories);
  } catch (err) {
    toast(err.message, "error");
    return;
  }
  if (!fileApps.length) {
    toast("No file-backed apps installed.", "error");
    return;
  }

  const { box, close } = openCustomModal({
    title: `Open "${item.name}"`,
    body: `
      <p class="muted">Launch an installed app with this file. The file is copied into the session's storage and opened in the app.</p>
      <div class="field">
        <label for="open-in-app-select">App</label>
        <select id="open-in-app-select">
          ${fileApps.map((a) => `<option value="${escModal(a.id)}">${escModal(a.name)}</option>`).join("")}
        </select>
      </div>
      <div class="field field-row">
        <label class="check">
          <input type="checkbox" id="open-in-app-onlaunch" checked>
          <span>Open file on launch</span>
        </label>
      </div>
      <p id="open-in-app-error" class="error" role="alert" hidden></p>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" data-act="open-in-app-cancel">Cancel</button>
        <button type="button" class="btn btn-primary" data-act="open-in-app-launch">Launch</button>
      </div>
    `,
  });

  box.querySelector('[data-act="open-in-app-cancel"]').addEventListener("click", close);

  box.querySelector('[data-act="open-in-app-launch"]').addEventListener("click", async () => {
    const appId = box.querySelector("#open-in-app-select").value;
    const openOnLaunch = box.querySelector("#open-in-app-onlaunch").checked;
    const errEl = box.querySelector("#open-in-app-error");
    const launchBtn = box.querySelector('[data-act="open-in-app-launch"]');
    errEl.hidden = true;
    launchBtn.disabled = true;
    try {
      const result = await secureFetch("/api/files/launch_from_storage", {
        method: "POST",
        body: {
          home_dir: state.home,
          path,
          application_id: appId,
          open_file_on_launch: openOnLaunch,
          language: $("opt-language").value,
          selected_gpu: $("field-gpu").hidden
            ? null
            : $("opt-gpu").value === "none"
              ? null
              : $("opt-gpu").value,
          wayland_mode: $("opt-wayland").checked,
        },
      });
      close();
      const appName = fileApps.find((a) => a.id === appId)?.name || "app";
      toast(`Launched ${appName}`);
      if (result && result.session_url) window.open(result.session_url, "_blank", "noopener");
      window.dispatchEvent(new CustomEvent("vreckan:sessions-changed"));
    } catch (err) {
      errEl.textContent = err.message;
      errEl.hidden = false;
    } finally {
      launchBtn.disabled = false;
    }
  });
}

// ----- Shares -----
function openShareModal(path) {
  currentSharePath = path;
  $("file-share-path").textContent = path;
  $("fs-error").hidden = true;
  $("fs-result").hidden = true;
  $("fs-password").value = "";
  $("fs-expiry").value = "";
  $("modal-file-share").hidden = false;
}

async function createShare() {
  try {
    const password = $("fs-password").value || undefined;
    const expiry = $("fs-expiry").value ? parseInt($("fs-expiry").value, 10) : undefined;
    const res = await secureFetch("/api/files/share", {
      method: "POST",
      body: {
        home_dir: state.home,
        path: currentSharePath,
        password,
        expiry_hours: expiry,
      },
    });
    const url = location.origin + res.url;
    $("fs-url").textContent = url;
    $("fs-result").hidden = false;
    $("fs-copy").onclick = async () => {
      try {
        await navigator.clipboard.writeText(url);
        toast("Copied", "success");
      } catch (_) {
        toast("Copy failed", "error");
      }
    };
  } catch (err) {
    $("fs-error").textContent = err.message;
    $("fs-error").hidden = false;
  }
}

function fmtBytesShare(b) {
  return fmtSize(b);
}

function renderShares() {
  const list = $("fm-shares-list");
  list.innerHTML = "";
  if (!sharesCache.length) {
    list.innerHTML = '<p class="muted">No active shares.</p>';
    return;
  }
  for (const s of sharesCache) {
    const row = document.createElement("div");
    row.className = "share-row";
    const url = location.origin + s.url;

    const meta = document.createElement("div");
    meta.className = "share-meta";
    meta.innerHTML = `
      <span>${s.original_filename}</span>
      <span class="muted">${fmtBytesShare(s.size_bytes)} · created ${new Date(s.created_at).toLocaleString()}${
        s.expiry_timestamp ? " · expires " + new Date(s.expiry_timestamp * 1000).toLocaleString() : ""
      }${s.has_password ? " · 🔒" : ""}</span>`;
    row.appendChild(meta);

    const urlWrap = document.createElement("div");
    urlWrap.className = "share-url";
    const code = document.createElement("code");
    code.textContent = url;
    urlWrap.appendChild(code);
    const copyBtn = document.createElement("button");
    copyBtn.className = "btn btn-ghost btn-sm";
    copyBtn.textContent = "Copy";
    copyBtn.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(url);
        toast("Copied", "success");
      } catch (_) {
        toast("Copy failed", "error");
      }
    });
    urlWrap.appendChild(copyBtn);
    row.appendChild(urlWrap);

    const revokeBtn = document.createElement("button");
    revokeBtn.className = "btn btn-danger btn-sm";
    revokeBtn.textContent = "Revoke";
    revokeBtn.addEventListener("click", async () => {
      if (!(await confirmModal("Revoke this share?", { danger: true }))) return;
      try {
        await secureFetch(`/api/files/share/${encodeURIComponent(s.share_id)}`, {
          method: "DELETE",
        });
        toast("Share revoked", "success");
        loadShares();
      } catch (err) {
        toast(err.message, "error");
      }
    });
    row.appendChild(revokeBtn);

    list.appendChild(row);
  }
}

async function loadShares() {
  try {
    sharesCache = (await secureFetch("/api/files/shares", { method: "GET" })) || [];
  } catch (err) {
    toast(err.message, "error");
    sharesCache = [];
  }
  renderShares();
}

// ----- Selection -----
function toggleSelectAll(checked) {
  for (const item of state.items) {
    if (checked) state.selected.add(item.path);
    else state.selected.delete(item.path);
  }
  renderTable();
}

// ----- Public API -----
export function showFilesTab(visible) {
  $("tab-files").hidden = !visible;
}

export async function initFilesView() {
  $("fm-home").addEventListener("change", (e) => {
    state.home = e.target.value;
    state.path = "/";
    state.page = 1;
    state.selected.clear();
    loadFiles();
  });

  $("fm-up").addEventListener("click", () => {
    state.path = parentPath(state.path);
    state.page = 1;
    state.selected.clear();
    loadFiles();
  });

  $("fm-refresh").addEventListener("click", loadFiles);
  $("fm-new-folder").addEventListener("click", newFolder);
  $("fm-upload").addEventListener("click", () => $("fm-upload-input").click());
  $("fm-upload-input").addEventListener("change", (e) => {
    const files = Array.from(e.target.files || []);
    if (files.length) handleUpload(files);
    e.target.value = "";
  });

  $("fm-download").addEventListener("click", () => {
    const path = [...state.selected][0];
    const item = state.items.find((i) => i.path === path);
    if (item) downloadFile(path, item.name);
  });

  $("fm-share").addEventListener("click", () => {
    const path = [...state.selected][0];
    openShareModal(path);
  });

  $("fm-open-in-app").addEventListener("click", openInAppModal);

  $("fm-delete").addEventListener("click", deleteSelected);
  $("fm-select-all").addEventListener("change", (e) => toggleSelectAll(e.target.checked));

  $("fm-prev").addEventListener("click", () => {
    if (state.page > 1) {
      state.page--;
      loadFiles();
    }
  });
  $("fm-next").addEventListener("click", () => {
    state.page++;
    loadFiles();
  });

  $("fm-shares-btn").addEventListener("click", () => {
    const panel = $("fm-shares-panel");
    panel.hidden = !panel.hidden;
    if (!panel.hidden) loadShares();
  });

  // Share modal
  $("file-share-form").addEventListener("submit", (e) => {
    e.preventDefault();
    createShare();
  });
  $("fs-cancel").addEventListener("click", () => {
    $("modal-file-share").hidden = true;
  });
}

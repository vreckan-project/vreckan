// Launch modal + launch (unified per-app modal — ANT-228 Option A; Feat 12 —
// open file on launch; Feat 10 — upload to shared storage). Owns the
// launch-modal state; reads the shared app state (availableGpus,
// currentSettings, globalDefaultGpu) from app.js and the user's presets from
// pinned.js (live bindings; the import cycle is safe because both modules
// only read the other's state at call time, not during evaluation).
import { $, toast, uploadFileChunks } from "../util.js";
import { secureFetch } from "../api.js";
import { availableGpus, currentSettings, globalDefaultGpu } from "../app.js";
import { loadSessions } from "./sessions.js";
import { applyPreset, pinnedBehaviors } from "./pinned.js";
import { t } from "../i18n.js";

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

// Fill the GPU select's options. Visibility of the field itself is decided
// per app when the modal opens (an app must be GPU-capable AND the server
// must report GPUs), so this only manages the option list.
function populateGpuDropdown() {
  const sel = $("opt-gpu");
  sel.innerHTML = "";
  if (!availableGpus.length) return;
  const none = document.createElement("option");
  none.value = "none";
  none.textContent = t("launch.gpuAuto");
  sel.appendChild(none);
  for (const gpu of availableGpus) {
    const opt = document.createElement("option");
    opt.value = gpu.device;
    opt.textContent = `${gpu.device}${gpu.driver ? ` (${gpu.driver})` : ""}`;
    sel.appendChild(opt);
  }
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
    toast(t("launch.sessionLaunched"));
  } else {
    toast(t("launch.noSessionUrl"), "error");
  }
  setTimeout(loadSessions, 1500);
}

// Every app type opens the unified launch modal (ANT-228 decision D2):
// browser apps get Session Name + URL, file apps get the file section, plain
// apps get Session Name + launch options. The modal's submit handler
// (handleLaunchConfirm) posts to the endpoint matching the app's
// capabilities.
function handleAppSelection(app) {
  openLaunchModal(app);
}

// ---------------------------------------------------------------------------
// Feat 12 — open file on launch
// ---------------------------------------------------------------------------
// The app the launch modal is currently open for (set by openLaunchModal).
export let launchModalApp = null;

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

// The standing GPU choice for an app: the default preset's saved GPU if it
// is still on the server, otherwise the admin's global default GPU,
// otherwise null (Auto). Apps that are not GPU-capable — or a server with no
// GPUs — always resolve to null. Exported so the Files view's "Open in app"
// flow applies the same defaults.
export function resolveLaunchGpu(app) {
  if (!(app.nvidia_support || app.dri3_support) || !availableGpus.length) return null;
  const defaultPreset = (pinnedBehaviors || []).find((p) => p.is_default);
  if (
    defaultPreset &&
    defaultPreset.selected_gpu &&
    availableGpus.some((g) => g.device === defaultPreset.selected_gpu)
  ) {
    return defaultPreset.selected_gpu;
  }
  if (globalDefaultGpu && availableGpus.some((g) => g.device === globalDefaultGpu)) {
    return globalDefaultGpu;
  }
  return null;
}

// Re-apply the user's standing launch defaults to the modal's controls: the
// default preset (if any) wins; otherwise the admin's global default GPU,
// but only when this app can actually use a GPU.
function applyModalDefaults(app) {
  const defaultPreset = (pinnedBehaviors || []).find((p) => p.is_default);
  if (defaultPreset) {
    applyPreset(defaultPreset, true);
    return;
  }
  if ($("field-gpu").hidden) return;
  const gpu = resolveLaunchGpu(app);
  if (gpu) $("opt-gpu").value = gpu;
}

// Open the unified launch modal for any app type (ANT-228 Option A),
// resetting the inputs and showing exactly the sections the app's
// capabilities call for. The app's optional `type` ("app" | "browser" |
// "desktop", added by ANT-229) is not required: capability booleans drive
// the layout until that field lands in the applications payload.
function openLaunchModal(app) {
  launchModalApp = app;
  $("launch-title").textContent = t("launch.launchApp", { name: app.name });

  // Per-app section visibility (the ANT-228 field matrix).
  $("field-launch-url").hidden = !app.url_support;
  $("launch-file-section").hidden = !app.home_directories;
  $("field-gpu").hidden =
    !(app.nvidia_support || app.dri3_support) || !availableGpus.length;
  // Both side actions only make sense with persistent storage.
  $("btn-manage-homedirs").hidden = !currentSettings.persistent_storage;
  $("btn-upload-storage").hidden = !currentSettings.persistent_storage;

  // Reset the per-launch inputs.
  $("launch-name").value = "";
  $("launch-url").value = "";
  $("launch-file").value = "";
  $("launch-server-file").value = "";
  $("launch-open-on-launch").checked = false;
  syncLaunchFileFields();
  $("launch-error").hidden = true;
  $("launch-upload-status").hidden = true;
  $("launch-submit").disabled = false;

  // Standing defaults (default preset / global default GPU) pre-fill the
  // options each time the modal opens.
  applyModalDefaults(app);

  $("modal-launch").hidden = false;
  // Focus the first input: the URL for apps that open one (mirroring the old
  // URL prompt), otherwise the session name (shown for every app type).
  if (!$("field-launch-url").hidden) $("launch-url").focus();
  else $("launch-name").focus();
}

// Show/hide the file pickers (and the subtitle) to match the
// "Open file on launch" toggle: off means there is no file to open,
// so the pickers are hidden and a launch is a plain app launch. The
// subtitle only exists for file-backed apps; other app types have no
// file note to show.
function syncLaunchFileFields() {
  const on = $("launch-open-on-launch").checked;
  $("launch-file-fields").hidden = !on;
  const sub = $("launch-sub");
  if (launchModalApp && launchModalApp.home_directories) {
    sub.textContent = on
      ? t("launch.pickFileOrServerFile")
      : t("launch.subtitle");
    sub.hidden = false;
  } else {
    sub.hidden = true;
  }
}

// File-backed launch (Feat 12): upload a local file, open a file already in
// shared storage, or launch with nothing open. Only used for apps with a
// file section (home_directories).
async function launchFileApp(app, payload, statusEl) {
  const fileInput = $("launch-file");
  const serverFileInput = $("launch-server-file");
  const openOnLaunch = $("launch-open-on-launch").checked;
  // The pickers are hidden while the toggle is off; ignore any stale
  // values so an unchecked launch is always a plain app launch.
  const file = openOnLaunch && fileInput.files ? fileInput.files[0] : null;
  const serverFile = openOnLaunch ? serverFileInput.value.trim() : "";
  payload.home_name = await resolveAutoHome(app, payload.home_name);

  if (file) {
    statusEl.hidden = false;
    statusEl.textContent = t("launch.uploading");
    const { upload_id, total_chunks } = await uploadFileChunks(file, (done, total) => {
      statusEl.textContent = t("launch.uploadingProgress", { done, total });
    });
    statusEl.textContent = t("launch.launching");
    return secureFetch("/api/launch/file", {
      method: "POST",
      body: {
        ...payload,
        filename: file.name,
        upload_id,
        total_chunks,
        open_file_on_launch: openOnLaunch,
      },
    });
  }
  if (serverFile) {
    return secureFetch("/api/launch/file_path", {
      method: "POST",
      body: { ...payload, filename: serverFile },
    });
  }
  // No file chosen — the file is optional, so just launch the app as-is
  // (home dir mounted per the selected mode, nothing opened in it).
  return secureFetch("/api/launch/simple", { method: "POST", body: payload });
}

// Confirm the launch modal. The endpoint follows the app's capabilities:
// a URL is opened via /api/launch/url when the user enters one; file-capable
// apps post /api/launch/file, /api/launch/file_path, or /api/launch/simple
// depending on the file inputs; everything else posts /api/launch/simple
// (a blank URL on a URL-capable app launches it as-is, with $VRECKAN_URL
// left unset server-side). The session name is sent on every branch.
async function handleLaunchConfirm(app) {
  const errEl = $("launch-error");
  const statusEl = $("launch-upload-status");
  const submitBtn = $("launch-submit");

  const payload = currentLaunchPayload(app.id);
  payload.session_name = $("launch-name").value.trim() || null;

  errEl.hidden = true;
  submitBtn.disabled = true;
  try {
    let result;
    if (app.url_support) {
      const url = $("launch-url").value.trim();
      if (url) {
        result = await secureFetch("/api/launch/url", {
          method: "POST",
          body: { ...payload, url },
        });
      } else if (app.home_directories) {
        // URL-capable app that also has a file section: no URL entered, so
        // fall back to the file flow (open a file or launch plain).
        result = await launchFileApp(app, payload, statusEl);
      } else {
        // URL-capable app without a file section, no URL entered: launch the
        // app as-is (mirrors the plain-app branch).
        result = await secureFetch("/api/launch/simple", {
          method: "POST",
          body: payload,
        });
      }
    } else if (app.home_directories) {
      result = await launchFileApp(app, payload, statusEl);
    } else {
      // Plain app: nothing to pick, just launch with the selected options.
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
// Feat 10 — upload a file to shared storage
// ---------------------------------------------------------------------------
// "Upload a file to shared storage" (in the launch modal's file section):
// pick a file from the computer, chunk-upload it, and have the server place
// it in the user's _vreckan_shared_files (usable as the launch "server
// file").
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
      button.textContent = t("launch.uploadingProgress", { done, total });
    });
    button.textContent = t("launch.finalizing");
    const res = await secureFetch("/api/upload/to_storage", {
      method: "POST",
      body: {
        filename: file.name,
        upload_id,
        total_chunks,
        home_name: "_vreckan_shared_files",
      },
    });
    toast(res && res.message ? res.message : t("launch.fileUploaded", { name: file.name }));
  } catch (e) {
    toast(e.message, "error");
  } finally {
    button.disabled = false;
    button.textContent = original;
  }
}

export {
  populateLanguageDropdown,
  populateGpuDropdown,
  currentLaunchPayload,
  openSession,
  handleAppSelection,
  openLaunchModal,
  syncLaunchFileFields,
  handleLaunchConfirm,
  pickFileForStorageUpload,
  uploadToStorage,
};

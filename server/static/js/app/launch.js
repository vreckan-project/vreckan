// Launch options + launch (Feat 12 — open file on launch; Feat 10 — upload to
// shared storage). Owns the launch-modal state; reads the shared app state
// (availableGpus, currentSettings) from app.js.
import { $, toast, uploadFileChunks } from "../util.js";
import { secureFetch } from "../api.js";
import { promptModal } from "../modal.js";
import { availableGpus, currentSettings } from "../app.js";
import { loadSessions } from "./sessions.js";
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
  none.textContent = t("launch.gpuAuto");
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
    toast(t("launch.sessionLaunched"));
  } else {
    toast(t("launch.noSessionUrl"), "error");
  }
  setTimeout(loadSessions, 1500);
}

async function handleAppSelection(app) {
  try {
    if (app.url_support) {
      const url = await promptModal({
        title: t("launch.enterUrl"),
        label: t("launch.url"),
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

// Open the launch modal for a file-backed app, resetting its inputs.
function openLaunchModal(app) {
  launchModalApp = app;
  $("launch-title").textContent = t("launch.launchApp", { name: app.name });
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
    ? t("launch.pickFileOrServerFile")
    : t("launch.subtitle");
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
      statusEl.textContent = t("launch.uploading");
      const { upload_id, total_chunks } = await uploadFileChunks(file, (done, total) => {
        statusEl.textContent = t("launch.uploadingProgress", { done, total });
      });
      statusEl.textContent = t("launch.launching");
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

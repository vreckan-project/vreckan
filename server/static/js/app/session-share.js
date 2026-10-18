// Session share modal (ANT-307 / D6) — one surface, two states.
//
// Opens as "create a link" (role + file-access presets + optional password /
// expiry) and morphs in place into "manage links" after a link is created (or
// when the session row's "(N links)" badge is clicked). Clicking a link row
// pre-fills the form for inline editing; the primary button becomes "Save
// changes" and issues a PATCH (no delete-and-recreate).
//
// The API is the owner session-share contract from ANT-305:
//   POST   /api/sessions/{id}/shares            { role, caps?, password?, expiry_hours? }
//   GET    /api/sessions/{id}/shares            -> [SessionShareInfo]
//   PATCH  /api/sessions/{id}/shares/{share_id} { role?, caps?, password?, expiry_hours? }
//   DELETE /api/sessions/{id}/shares/{share_id}
// Every call goes through secureFetch (E2EE). The `input` cap is implied by
// the role (full = input on; view_only = input off), so it is not a checkbox;
// the file caps (browse / download / upload) are.

import { secureFetch } from "../api.js";
import { $, toast } from "../util.js";
import { t } from "../i18n.js";

// File-cap presets (the role's `input` is handled separately).
const PRESETS = {
  standard: { file_browse: true, file_download: true, file_upload: false },
  none: { file_browse: false, file_download: false, file_upload: false },
  full: { file_browse: true, file_download: true, file_upload: true },
};
// Which preset (if any) a set of file caps matches exactly, else "custom".
function presetForCaps(caps) {
  for (const [name, p] of Object.entries(PRESETS)) {
    if (
      p.file_browse === !!caps.file_browse &&
      p.file_download === !!caps.file_download &&
      p.file_upload === !!caps.file_upload
    ) {
      return name;
    }
  }
  return "custom";
}

function nowSecs() {
  return Date.now() / 1000;
}

// A share is "active" while it is not revoked and not past its expiry.
function shareIsActive(s) {
  if (s.revoked) return false;
  if (s.expiry_timestamp && s.expiry_timestamp < nowSecs()) return false;
  return true;
}

// Human-readable capability summary for a share (used in the manage list).
function capSummary(s) {
  const parts = [s.role === "full" ? t("shareSession.pvUsesApp") : t("shareSession.pvWatches")];
  const c = s.caps || {};
  const fileParts = [];
  if (c.file_browse) fileParts.push(t("shareSession.capBrowse"));
  if (c.file_download) fileParts.push(t("shareSession.capDownload"));
  if (c.file_upload) fileParts.push(t("shareSession.capUpload"));
  parts.push(fileParts.length ? fileParts.join(" & ") : t("shareSession.pvNoFiles"));
  parts.push(s.has_password ? t("shareSession.sumPassword") : t("shareSession.sumNoPassword"));
  if (s.expiry_timestamp) {
    parts.push(
      s.expiry_timestamp < nowSecs()
        ? t("shareSession.sumExpired")
        : t("shareSession.sumExpiresIn", { hours: Math.max(1, Math.round((s.expiry_timestamp - nowSecs()) / 3600)) })
    );
  } else {
    parts.push(t("shareSession.sumNoExpiry"));
  }
  if (s.revoked) parts.push(t("shareSession.sumRevoked"));
  return parts.join(" · ");
}

// The live recipient-preview line under the create/edit controls.
function previewLine(role, fileCaps, password, expiryHours) {
  const parts = [role === "full" ? t("shareSession.pvUsesApp") : t("shareSession.pvWatches")];
  if (role === "full") {
    parts.push(t("shareSession.pvAllFiles"));
  } else {
    const fileParts = [];
    if (fileCaps.file_browse) fileParts.push(t("shareSession.capBrowse"));
    if (fileCaps.file_download) fileParts.push(t("shareSession.capDownload"));
    if (fileCaps.file_upload) fileParts.push(t("shareSession.capUpload"));
    parts.push(fileParts.length ? t("shareSession.pvCanFiles", { caps: fileParts.join(" & ") }) : t("shareSession.pvNoFiles"));
  }
  parts.push(password ? t("shareSession.sumPassword") : t("shareSession.sumNoPassword"));
  parts.push(expiryHours ? t("shareSession.pvExpiresIn", { hours: expiryHours }) : t("shareSession.sumNoExpiry"));
  return t("shareSession.preview", { parts: parts.join(" · ") });
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

let ctx = null; // { sessionId, sessionName, manage, shares, editingShareId, newestShareId }
let escListener = null;
let langListener = null;

function el(id) {
  return $(id);
}

function roleValue() {
  const r = document.querySelector('input[name="ss-role"]:checked');
  return r ? r.value : "view_only";
}

function fileCapsFromForm() {
  return {
    file_browse: el("ss-cap-browse").checked,
    file_download: el("ss-cap-download").checked,
    file_upload: el("ss-cap-upload").checked,
  };
}

function fullCapsFromForm() {
  const role = roleValue();
  const file = fileCapsFromForm();
  return {
    input: role === "full",
    file_browse: role === "full" ? true : file.file_browse,
    file_download: role === "full" ? true : file.file_download,
    file_upload: role === "full" ? true : file.file_upload,
  };
}

function expiryHoursFromForm() {
  const r = document.querySelector('input[name="ss-expiry"]:checked');
  if (!r) return null;
  if (r.value === "") return null;
  if (r.value === "custom") {
    const h = parseInt(el("ss-expiry-hours").value, 10);
    return h > 0 ? h : null;
  }
  const h = parseInt(r.value, 10);
  return h > 0 ? h : null;
}

function syncPreview() {
  const password = el("ss-password").value.trim() || null;
  el("ss-preview").textContent = previewLine(roleValue(), fileCapsFromForm(), password, expiryHoursFromForm());
}

function markPresetActive(name) {
  for (const [preset, btnId] of Object.entries({
    standard: "ss-preset-standard",
    none: "ss-preset-none",
    full: "ss-preset-full",
  })) {
    el(btnId).classList.toggle("is-active", preset === name);
  }
}

function setFileCaps(caps, presetName) {
  el("ss-cap-browse").checked = !!caps.file_browse;
  el("ss-cap-download").checked = !!caps.file_download;
  el("ss-cap-upload").checked = !!caps.file_upload;
  markPresetActive(presetName || presetForCaps(caps));
}

function refreshRoleDependent() {
  const role = roleValue();
  // D2 invariant: a Full share has every file cap on, so the file section is
  // not a meaningful choice there — hide it (recommended default; the
  // question card on ANT-307 captures the alternative).
  el("ss-files-section").hidden = role === "full";
  el("ss-role-desc").textContent =
    role === "full" ? t("shareSession.fullDesc") : t("shareSession.viewOnlyDesc");
  syncPreview();
}

function refreshOptionsVisibility() {
  const custom = document.querySelector('input[name="ss-expiry"]:checked')?.value === "custom";
  el("ss-expiry-hours").hidden = !custom;
  if (custom && !el("ss-expiry-hours").hidden) {
    el("ss-expiry-hours").focus();
  }
}

// ---------------------------------------------------------------------------
// Open / close
// ---------------------------------------------------------------------------

function sessionDisplayName(session) {
  return session.name ? `${session.app_name} - ${session.name}` : session.app_name || session.session_id;
}

export function openSessionShareModal(session, { manage = false, editShare = null } = {}) {
  if (!ctx) wireModalOnce();
  ctx = {
    sessionId: session.session_id,
    sessionName: sessionDisplayName(session),
    manage: manage || !!editShare,
    shares: editShare ? [editShare] : [],
    editingShareId: editShare ? editShare.share_id : null,
    newestShareId: null,
  };
  el("ss-title").textContent = t("shareSession.title", { name: ctx.sessionName });
  el("ss-error").hidden = true;

  if (editShare) {
    prefillFromShare(editShare);
    setMode("create");
  } else if (manage) {
    setMode("manage");
    loadShares(() => el("ss-new-link").focus());
  } else {
    resetFormToDefaults();
    setMode("create");
    el("ss-role-viewonly").focus();
  }

  el("modal-session-share").hidden = false;
  attachEsc();
  attachLang();
}

function attachEsc() {
  detachEsc();
  escListener = (e) => {
    if (e.key === "Escape") closeSessionShareModal();
  };
  document.addEventListener("keydown", escListener);
}

function detachEsc() {
  if (escListener) {
    document.removeEventListener("keydown", escListener);
    escListener = null;
  }
}

function attachLang() {
  detachLang();
  langListener = () => {
    // Re-render dynamic strings if the user switches language mid-modal.
    if (el("modal-session-share").hidden) return;
    el("ss-title").textContent = t("shareSession.title", { name: ctx.sessionName });
    if (!el("ss-manage").hidden) renderShareList();
    else {
      refreshRoleDependent();
      el("ss-submit").textContent = ctx.editingShareId
        ? t("shareSession.saveChanges")
        : t("shareSession.create");
    }
  };
  document.addEventListener("vreckan:langchange", langListener);
}

function detachLang() {
  if (langListener) {
    document.removeEventListener("vreckan:langchange", langListener);
    langListener = null;
  }
}

export function closeSessionShareModal() {
  el("modal-session-share").hidden = true;
  detachEsc();
  detachLang();
  ctx = null;
}

function setMode(mode) {
  el("session-share-form").hidden = mode !== "create";
  el("ss-manage").hidden = mode !== "manage";
}

// ---------------------------------------------------------------------------
// Form defaults + prefill
// ---------------------------------------------------------------------------

function resetFormToDefaults() {
  el("ss-role-viewonly").checked = true;
  el("ss-role-full").checked = false;
  setFileCaps(PRESETS.standard, "standard");
  el("ss-cap-row").hidden = true;
  el("ss-customize").classList.remove("is-active");
  el("ss-options-body").hidden = true;
  el("ss-options-toggle").classList.remove("open");
  el("ss-password").value = "";
  el("ss-clear-pw").checked = false;
  el("ss-pw-hint").hidden = true;
  el("ss-clear-pw-wrap").hidden = true;
  el("ss-expiry-none").checked = true;
  el("ss-expiry-hours").value = "";
  el("ss-expiry-hours").hidden = true;
  ctx.editingShareId = null;
  el("ss-submit").textContent = t("shareSession.create");
  refreshRoleDependent();
}

function prefillFromShare(share) {
  if (!share) return;
  ctx.editingShareId = share.share_id;
  el("ss-role-viewonly").checked = share.role !== "full";
  el("ss-role-full").checked = share.role === "full";
  setFileCaps(share.caps || {});
  el("ss-cap-row").hidden = false;
  el("ss-customize").classList.add("is-active");

  // Password: we only know whether one is set, not the value.
  const hasPw = !!share.has_password;
  el("ss-password").value = "";
  el("ss-clear-pw").checked = false;
  el("ss-pw-hint").hidden = !hasPw;
  el("ss-clear-pw-wrap").hidden = !hasPw;

  // Expiry: map the stored timestamp back onto a choice.
  setExpiryFromShare(share);
  el("ss-options-body").hidden = false;
  el("ss-options-toggle").classList.add("open");

  el("ss-submit").textContent = t("shareSession.saveChanges");
  refreshRoleDependent();
}

function setExpiryFromShare(share) {
  el("ss-expiry-hours").hidden = true;
  if (!share.expiry_timestamp) {
    el("ss-expiry-none").checked = true;
    return;
  }
  const remainingH = (share.expiry_timestamp - nowSecs()) / 3600;
  // Snap to a standard choice when the remaining time is close to it
  // (with a little tolerance); otherwise fall back to Custom with the
  // exact remaining hours.
  for (const [hours, radioId] of [
    [1, "ss-expiry-1h"],
    [24, "ss-expiry-24h"],
    [168, "ss-expiry-7d"],
  ]) {
    if (Math.abs(remainingH - hours) <= Math.max(1, hours * 0.05)) {
      el(radioId).checked = true;
      return;
    }
  }
  el("ss-expiry-custom").checked = true;
  el("ss-expiry-hours").value = String(Math.max(1, Math.ceil(remainingH)));
  el("ss-expiry-hours").hidden = false;
}

// ---------------------------------------------------------------------------
// Share list (manage state)
// ---------------------------------------------------------------------------

async function loadShares(onReady) {
  try {
    ctx.shares = (await secureFetch(`/api/sessions/${ctx.sessionId}/shares`, { method: "GET" })) || [];
  } catch (err) {
    toast(err.message, "error");
    ctx.shares = [];
  }
  renderShareList();
  if (onReady) onReady();
}

function renderShareList() {
  const list = el("ss-links-list");
  list.innerHTML = "";
  if (!ctx.shares.length) {
    list.innerHTML = `<p class="muted">${t("shareSession.noLinks")}</p>`;
    return;
  }
  const ordered = [...ctx.shares].sort((a, b) => b.created_at - a.created_at);
  for (const s of ordered) {
    list.appendChild(buildShareRow(s));
  }
}

function buildShareRow(s) {
  const url = location.origin + s.url;
  const row = document.createElement("div");
  row.className = "share-row ss-share-row" + (s.share_id === ctx.newestShareId ? " is-new" : "") + (shareIsActive(s) ? "" : " is-dead");

  const left = document.createElement("div");
  left.className = "share-meta";
  const summary = document.createElement("span");
  summary.textContent = capSummary(s);
  left.appendChild(summary);
  const urlWrap = document.createElement("div");
  urlWrap.className = "share-url";
  const code = document.createElement("code");
  code.textContent = url;
  urlWrap.appendChild(code);
  const copyBtn = document.createElement("button");
  copyBtn.type = "button";
  copyBtn.className = "btn btn-ghost btn-sm";
  copyBtn.textContent = t("common.copy");
  copyBtn.addEventListener("click", () => copyShareUrl(url));
  urlWrap.appendChild(copyBtn);
  left.appendChild(urlWrap);
  row.appendChild(left);

  const actions = document.createElement("div");
  actions.className = "share-ss-actions";

  const editBtn = document.createElement("button");
  editBtn.type = "button";
  editBtn.className = "btn btn-ghost btn-sm";
  editBtn.textContent = t("shareSession.edit");
  editBtn.addEventListener("click", () => {
    el("ss-error").hidden = true;
    prefillFromShare(s);
    setMode("create");
    el("ss-role-viewonly").focus();
  });
  actions.appendChild(editBtn);

  const revokeBtn = document.createElement("button");
  revokeBtn.type = "button";
  revokeBtn.className = "btn btn-danger btn-sm";
  revokeBtn.textContent = t("shareSession.revoke");
  let armed = false;
  let armTimer = null;
  revokeBtn.addEventListener("click", async () => {
    if (!armed) {
      armed = true;
      revokeBtn.textContent = t("shareSession.confirmRevoke");
      armTimer = setTimeout(() => {
        armed = false;
        revokeBtn.textContent = t("shareSession.revoke");
      }, 4000);
      return;
    }
    clearTimeout(armTimer);
    armed = false;
    revokeBtn.disabled = true;
    try {
      await secureFetch(`/api/sessions/${ctx.sessionId}/shares/${s.share_id}`, { method: "DELETE" });
      toast(t("shareSession.linkDead"), "success");
      if (ctx.editingShareId === s.share_id) closeSessionShareModal();
      loadShares();
      notifySessionsChanged();
    } catch (err) {
      revokeBtn.disabled = false;
      revokeBtn.textContent = t("shareSession.revoke");
      toast(err.message, "error");
    }
  });
  actions.appendChild(revokeBtn);

  row.appendChild(actions);
  return row;
}

async function copyShareUrl(url) {
  try {
    await navigator.clipboard.writeText(url);
    toast(t("shareSession.copied"), "success");
  } catch (_) {
    toast(t("shareSession.copyFailed"), "error");
  }
}

function notifySessionsChanged() {
  window.dispatchEvent(new CustomEvent("vreckan:sessions-changed"));
}

// ---------------------------------------------------------------------------
// Submit (create or save-changes)
// ---------------------------------------------------------------------------

async function handleSubmit() {
  const role = roleValue();
  const caps = fullCapsFromForm();

  // Password handling differs between create and edit.
  const pwInput = el("ss-password").value.trim();
  let password;
  if (ctx.editingShareId) {
    const editingShare = ctx.shares.find((s) => s.share_id === ctx.editingShareId);
    const hadPw = !!(editingShare && editingShare.has_password);
    if (pwInput) password = pwInput;
    else if (hadPw && el("ss-clear-pw").checked) password = null;
    else password = undefined; // absent -> server keeps the current value
  } else {
    password = pwInput || undefined;
  }
  const expiryHours = expiryHoursFromForm();

  const body = { role, caps };
  if (password !== undefined) body.password = password;
  if (ctx.editingShareId) {
    // On edit the form is the source of truth for expiry too.
    body.expiry_hours = expiryHours;
  } else if (expiryHours != null) {
    body.expiry_hours = expiryHours;
  }

  const isEdit = !!ctx.editingShareId;
  const path = isEdit
    ? `/api/sessions/${ctx.sessionId}/shares/${ctx.editingShareId}`
    : `/api/sessions/${ctx.sessionId}/shares`;
  const method = isEdit ? "PATCH" : "POST";

  const btn = el("ss-submit");
  btn.disabled = true;
  el("ss-error").hidden = true;
  try {
    const res = await secureFetch(path, { method, body });
    if (isEdit) {
      const idx = ctx.shares.findIndex((s) => s.share_id === ctx.editingShareId);
      if (res && idx !== -1) ctx.shares[idx] = res;
      ctx.editingShareId = null;
      setMode("manage");
      renderShareList();
      toast(t("shareSession.saved"), "success");
    } else {
      if (res) {
        ctx.newestShareId = res.share_id;
        ctx.shares = [res, ...ctx.shares.filter((s) => s.share_id !== res.share_id)];
      }
      setMode("manage");
      renderShareList();
      toast(t("shareSession.linkCreated"), "success");
    }
    notifySessionsChanged();
    loadShares();
  } catch (err) {
    el("ss-error").textContent = err.message;
    el("ss-error").hidden = false;
  } finally {
    btn.disabled = false;
  }
}

// ---------------------------------------------------------------------------
// One-time wiring
// ---------------------------------------------------------------------------

function wireModalOnce() {
  if (window.__ssWired) return;
  window.__ssWired = true;

  // Role segmented control.
  for (const r of document.querySelectorAll('input[name="ss-role"]')) {
    r.addEventListener("change", refreshRoleDependent);
  }

  // File-access presets.
  const presetMap = {
    "ss-preset-standard": "standard",
    "ss-preset-none": "none",
    "ss-preset-full": "full",
  };
  for (const [btnId, preset] of Object.entries(presetMap)) {
    el(btnId).addEventListener("click", () => {
      setFileCaps(PRESETS[preset], preset);
      el("ss-cap-row").hidden = true;
      el("ss-customize").classList.remove("is-active");
      syncPreview();
    });
  }

  // Customize... expands the per-capability checkboxes.
  el("ss-customize").addEventListener("click", () => {
    const open = el("ss-cap-row").hidden;
    el("ss-cap-row").hidden = !open;
    el("ss-customize").classList.toggle("is-active", open);
  });

  // Capability checkboxes -> custom mode.
  for (const id of ["ss-cap-browse", "ss-cap-download", "ss-cap-upload"]) {
    el(id).addEventListener("change", () => {
      markPresetActive(presetForCaps(fileCapsFromForm()));
      el("ss-cap-row").hidden = false;
      el("ss-customize").classList.add("is-active");
      syncPreview();
    });
  }

  // Collapsible Options section.
  el("ss-options-toggle").addEventListener("click", () => {
    const open = el("ss-options-body").hidden;
    el("ss-options-body").hidden = !open;
    el("ss-options-toggle").classList.toggle("open", open);
  });

  // Expiry choices (custom reveals the hours input).
  for (const r of document.querySelectorAll('input[name="ss-expiry"]')) {
    r.addEventListener("change", () => {
      refreshOptionsVisibility();
      syncPreview();
    });
  }
  el("ss-expiry-hours").addEventListener("input", syncPreview);
  el("ss-password").addEventListener("input", syncPreview);

  // Actions.
  el("session-share-form").addEventListener("submit", (e) => {
    e.preventDefault();
    handleSubmit();
  });
  el("ss-cancel").addEventListener("click", closeSessionShareModal);
  el("ss-close").addEventListener("click", closeSessionShareModal);
  el("ss-new-link").addEventListener("click", () => {
    resetFormToDefaults();
    setMode("create");
    el("ss-role-viewonly").focus();
  });
}

export { loadShares as reloadSessionShares, presetForCaps };

// modal.js — in-app modal helpers for the Vreckan web UI.
//
// The UI must never use native browser dialogs (window.alert / confirm /
// prompt): they block the whole page, cannot be styled, and are suppressed
// in some embedded contexts. These helpers render equivalent in-page modals
// instead and return Promises, so call sites read like the native versions:
//
//   if (!(await confirmModal("Delete user 'bob'?"))) return;
//   const name = await promptModal({ title: "New folder", label: "Name" });
//   if (!name) return;
//
// A single shared backdrop (#modal-root) is created on first use and reused
// for every modal, so it does not need to exist in index.html.

function esc(value) {
  return String(value == null ? "" : value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

const ROOT_ID = "modal-root";

function ensureRoot() {
  let root = document.getElementById(ROOT_ID);
  if (!root) {
    root = document.createElement("div");
    root.id = ROOT_ID;
    root.className = "modal-backdrop";
    root.hidden = true;
    root.innerHTML = `<div class="modal card" id="modal-root-box"></div>`;
    document.body.appendChild(root);
  }
  return root;
}

function boxEl() {
  const root = ensureRoot();
  let box = document.getElementById("modal-root-box");
  if (!box) {
    box = document.createElement("div");
    box.className = "modal card";
    box.id = "modal-root-box";
    root.appendChild(box);
  }
  return box;
}

// Render `html` into the shared modal box and show it.
// Returns { box, close }. close() hides the modal, clears the box and
// invokes onDismiss (if given) exactly once — including when the user clicks
// the dark backdrop outside the modal.
function show(html, { wide = false, onDismiss } = {}) {
  const root = ensureRoot();
  const box = boxEl();
  box.classList.toggle("modal-wide", wide);
  box.innerHTML = html;
  root.hidden = false;
  let dismissed = false;
  const close = () => {
    if (dismissed) return;
    dismissed = true;
    root.hidden = true;
    box.innerHTML = "";
    root.onclick = null;
    if (onDismiss) onDismiss();
  };
  root.onclick = (e) => {
    if (e.target === root) close();
  };
  return { root, box, close };
}

// Hide the shared modal (no-op if nothing is open).
export function closeModal() {
  const root = document.getElementById(ROOT_ID);
  if (!root) return;
  root.hidden = true;
  const box = document.getElementById("modal-root-box");
  if (box) box.innerHTML = "";
  root.onclick = null;
}

// Promise-based confirm. Resolves true when confirmed, false on Cancel /
// Escape / backdrop click.
export function confirmModal(
  message,
  { title = "Are you sure?", confirmLabel = "Confirm", danger = false } = {}
) {
  return new Promise((resolve) => {
    let settled = false;
    const onKey = (e) => {
      if (e.key === "Escape") finish(false);
    };
    const finish = (val) => {
      if (settled) return;
      settled = true;
      document.removeEventListener("keydown", onKey);
      close();
      resolve(val);
    };
    const { box, close } = show(
      `<h3>${esc(title)}</h3>
      <div class="modal-body"><p class="modal-message">${esc(message)}</p></div>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" data-act="cancel">Cancel</button>
        <button type="button" class="btn ${danger ? "btn-danger" : "btn-primary"}" data-act="ok">${esc(confirmLabel)}</button>
      </div>`,
      { onDismiss: () => finish(false) }
    );
    box.querySelector('[data-act="cancel"]').addEventListener("click", () => finish(false));
    box.querySelector('[data-act="ok"]').addEventListener("click", () => finish(true));
    document.addEventListener("keydown", onKey);
    box.querySelector('[data-act="ok"]').focus();
  });
}

// Promise-based prompt. Resolves the input value on OK / Enter, or null on
// Cancel / Escape / backdrop click.
export function promptModal({
  title = "Please enter",
  label = "Value",
  defaultValue = "",
  placeholder = "",
} = {}) {
  return new Promise((resolve) => {
    let settled = false;
    let input;
    const onKey = (e) => {
      if (e.key === "Escape") finish(null);
      else if (e.key === "Enter" && input) finish(input.value);
    };
    const finish = (val) => {
      if (settled) return;
      settled = true;
      document.removeEventListener("keydown", onKey);
      close();
      resolve(val);
    };
    const { box, close } = show(
      `<h3>${esc(title)}</h3>
      <div class="modal-body">
        <label>${esc(label)}
          <input type="text" id="modal-prompt-input" value="${esc(defaultValue)}" placeholder="${esc(placeholder)}">
        </label>
      </div>
      <div class="modal-actions">
        <button type="button" class="btn btn-ghost" data-act="cancel">Cancel</button>
        <button type="button" class="btn btn-primary" data-act="ok">OK</button>
      </div>`,
      { onDismiss: () => finish(null) }
    );
    input = box.querySelector("#modal-prompt-input");
    box.querySelector('[data-act="cancel"]').addEventListener("click", () => finish(null));
    box.querySelector('[data-act="ok"]').addEventListener("click", () => finish(input.value));
    document.addEventListener("keydown", onKey);
    input.focus();
    input.select();
  });
}

// Open a fully custom modal body. Returns { box, close, setBody } so the
// caller can drive dynamic content (e.g. a live progress bar) inside the
// modal. onDismiss fires (once) if the user dismisses via the backdrop.
export function openCustomModal({ title, body, wide = false, onDismiss } = {}) {
  const { box, close } = show(
    `<h3>${esc(title)}</h3><div class="modal-body">${body || ""}</div>`,
    { wide, onDismiss }
  );
  return {
    box,
    close,
    setBody: (html) => {
      const el = box.querySelector(".modal-body");
      if (el) el.innerHTML = html;
    },
  };
}

export { esc as escModal };

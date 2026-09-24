// util.js — small shared helpers used across the Vreckan web client.
//
// These were previously copy-pasted into every module (admin.js, app.js,
// files.js, and each admin/*.js section file). They live here so there is a
// single source of truth. This module only depends on api.js + crypto.js
// (both leaf modules), so importing it never creates a cycle.

import { secureFetch } from "./api.js";
import { arrayBufferToBase64 } from "./crypto.js";

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------
export function $(id) {
  return document.getElementById(id);
}

// Append a transient toast to #toasts. `kind` is a CSS modifier
// ("success" | "error" | ""); the toast removes itself after 4 seconds.
export function toast(message, kind = "success") {
  const container = $("toasts");
  if (!container) return;
  const el = document.createElement("div");
  el.className = `toast ${kind || ""}`.trim();
  el.textContent = message;
  container.appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

// Escape ALL five HTML-special chars. The div.textContent->innerHTML
// technique only escapes & < > (text-node context) and leaves " and '
// literal — which corrupts any value placed inside a quoted attribute
// (e.g. data-install="{"...") because the first " terminates the attr.
export function esc(value) {
  return String(value == null ? "" : value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------
// secureFetch wrapper that returns { data, error } instead of throwing.
// `error` is { message, status } (status is null when not an HTTP error).
export async function request(path, method = "GET", body) {
  const opts = { method };
  if (body !== undefined) opts.body = body;
  try {
    return { data: await secureFetch(path, opts), error: null };
  } catch (e) {
    const status = /status:\s*(\d+)/.exec(String(e.message));
    return { data: null, error: { message: e.message, status: status ? Number(status[1]) : null } };
  }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------
// Human-readable byte size (B/KB/MB/GB/TB).
export function formatBytes(n) {
  if (n == null || n === "") return "";
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let val = n;
  let u = -1;
  do {
    val /= 1024;
    u++;
  } while (val >= 1024 && u < units.length - 1);
  return `${val.toFixed(val >= 100 ? 0 : 1)} ${units[u]}`;
}

// Resolve an app logo to an <img> src (fetches custom icons as data URLs).
// Mirrors the browser extension's formatLogoSrc.
export async function formatLogoSrc(app) {
  const logo = app.logo || "";
  if (logo.startsWith("http://") || logo.startsWith("https://")) return logo;
  if (logo.startsWith("/api/app_icon/")) {
    const res = await request(logo, "GET");
    if (res.data && res.data.icon_data_b64) return `data:image/png;base64,${res.data.icon_data_b64}`;
  }
  return "/img/icon128.png";
}

// ---------------------------------------------------------------------------
// Chunked upload
// ---------------------------------------------------------------------------
// Upload a File in 2 MiB chunks via /api/upload/initiate + /api/upload/chunk.
// Returns { upload_id, total_chunks } — the server reassembles from
// upload_id. `onProgress(done, total)` is called after each chunk. The chunk
// size must match the server's upload CHUNK_SIZE.
export const UPLOAD_CHUNK_SIZE = 2 * 1024 * 1024;

export async function uploadFileChunks(file, onProgress) {
  const init = await secureFetch("/api/upload/initiate", {
    method: "POST",
    body: { filename: file.name, total_size: file.size },
  });
  const upload_id = init.upload_id;
  const totalChunks = Math.max(1, Math.ceil(file.size / UPLOAD_CHUNK_SIZE));
  for (let i = 0; i < totalChunks; i++) {
    const start = i * UPLOAD_CHUNK_SIZE;
    const end = Math.min(start + UPLOAD_CHUNK_SIZE, file.size);
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

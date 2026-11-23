// secureFetch for E2EE endpoints. Mirrors browser_extension/background.js secureFetchInBackground.
// Differences: web client uses the HttpOnly cookie (credentials:'include') — no JWT.

import {
  ensureSession,
  resetSession,
  encryptAesGcm,
  decryptAesGcm,
  getSession,
} from "./crypto.js";

// True when `parsed` is an E2EE response envelope ({ iv, ciphertext }).
function isEncryptedEnvelope(parsed) {
  return parsed && typeof parsed === "object" && parsed.iv && parsed.ciphertext;
}

// Best-effort readable form of an HTTP error body. E2EE routes encrypt the
// error payload ({"detail": ...} inside the envelope) before it reaches the
// client, so re-throwing the raw body would surface base64 ciphertext in
// toasts. Decrypt it with the same session key as the success path and
// surface the server's human-readable message; fall back to the raw text
// when the body is not JSON, not an envelope, or decryption fails (stale
// session — the 400 retry below still re-handshakes and recovers).
async function readableErrorBody(text, sess) {
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    return text;
  }
  if (!isEncryptedEnvelope(parsed)) return text;
  try {
    const decrypted = await decryptAesGcm(sess.key, parsed.iv, parsed.ciphertext, sess.id);
    try {
      const inner = JSON.parse(decrypted);
      if (inner && typeof inner === "object") {
        if (typeof inner.detail === "string" && inner.detail) return inner.detail;
        if (typeof inner.error === "string" && inner.error) return inner.error;
      }
    } catch {
      // Decrypted payload is not JSON; surface the plaintext as-is.
    }
    return decrypted;
  } catch {
    return text;
  }
}

export async function secureFetch(path, options = {}) {
  const { method = "GET", body } = options;
  let attempt = 0;

  while (true) {
    try {
      const sess = await ensureSession();
      const url = `${sess.baseUrl}${path}`;

      const headers = { "X-Session-ID": sess.id };
      let payload;
      if (body !== undefined && body !== null) {
        // Bind the ciphertext to the session via AAD (the session id), matching
        // the server, so it cannot be replayed against another session's key.
        const encrypted = await encryptAesGcm(sess.key, JSON.stringify(body), sess.id);
        headers["Content-Type"] = "application/json";
        payload = JSON.stringify(encrypted);
      }

      const response = await fetch(url, { method, headers, credentials: "include", body: payload });

      // Empty-body responses (e.g. 204 DELETE) pass through unencrypted.
      if (
        response.status === 204 ||
        (response.status === 200 && response.headers.get("Content-Length") === "0")
      ) {
        return null;
      }

      const text = await response.text();
      if (!response.ok) {
        throw new Error(
          `HTTP error! status: ${response.status} - ${await readableErrorBody(text, sess)}`
        );
      }

      const parsed = JSON.parse(text);
      if (isEncryptedEnvelope(parsed)) {
        const decrypted = await decryptAesGcm(sess.key, parsed.iv, parsed.ciphertext, sess.id);
        return JSON.parse(decrypted);
      }
      return parsed;
    } catch (error) {
      const msg = String(error.message || error);
      const recoverable =
        (msg.includes("atob") || msg.includes("decryption") || msg.includes("HTTP error! status: 400")) &&
        attempt === 0;
      if (recoverable) {
        attempt += 1;
        resetSession();
        continue;
      }
      throw error;
    }
  }
}

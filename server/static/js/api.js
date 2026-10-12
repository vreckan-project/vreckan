// secureFetch for E2EE endpoints. Mirrors browser_extension/background.js secureFetchInBackground.
// Differences: web client uses the HttpOnly cookie (credentials:'include') — no JWT.

import {
  ensureSession,
  resetSession,
  encryptAesGcm,
  decryptAesGcm,
  getSession,
} from "./crypto.js";

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
        throw new Error(`HTTP error! status: ${response.status} - ${text}`);
      }

      const parsed = JSON.parse(text);
      if (parsed && typeof parsed === "object" && parsed.iv && parsed.ciphertext) {
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

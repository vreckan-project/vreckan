// Plaintext auth endpoints (not E2EE). Relies on the HttpOnly `vreckan_auth` cookie
// via credentials:'include'. These are exempt from EncryptedRoute on the server.

async function handleAuthError(res) {
  let detail = `HTTP ${res.status}`;
  try {
    const body = await res.json();
    if (body && body.detail) detail = typeof body.detail === "string" ? body.detail : JSON.stringify(body.detail);
  } catch (e) {
    /* not JSON */
  }
  return detail;
}

export async function login(username, password) {
  const res = await fetch("/api/auth/login", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    credentials: "include",
    body: JSON.stringify({ username, password }),
  });
  if (!res.ok) throw new Error(await handleAuthError(res));
  return res.json();
}

export async function me() {
  const res = await fetch("/api/auth/me", { credentials: "include" });
  if (!res.ok) throw new Error(await handleAuthError(res));
  return res.json();
}

export async function logout() {
  const res = await fetch("/api/auth/logout", { method: "POST", credentials: "include" });
  if (!res.ok) throw new Error(await handleAuthError(res));
  return res.json().catch(() => ({}));
}

export async function changePassword(oldPassword, newPassword) {
  const res = await fetch("/api/auth/change_password", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    credentials: "include",
    body: JSON.stringify({ old_password: oldPassword, new_password: newPassword }),
  });
  if (!res.ok) throw new Error(await handleAuthError(res));
  return res.json().catch(() => ({}));
}

export async function setUserPassword(targetUsername, password) {
  // Admin: set or reset a user's web-login password. Plaintext endpoint
  // (exempt from E2EE) — the password crosses the wire inside TLS only.
  const res = await fetch(`/api/auth/set_password?target=${encodeURIComponent(targetUsername)}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    credentials: "include",
    body: JSON.stringify({ password }),
  });
  if (!res.ok) throw new Error(await handleAuthError(res));
  return res.json().catch(() => ({}));
}

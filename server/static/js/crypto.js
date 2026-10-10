// E2EE crypto helpers (WebCrypto). Mirrors browser_extension/crypto-utils.js + background.js.
// Session state: { key: CryptoKey|null, id: string|null, baseUrl: string|null }

let session = { key: null, id: null, baseUrl: null };

export function getSession() {
  return session;
}

export function resetSession() {
  session = { key: null, id: null, baseUrl: null };
}

export function arrayBufferToBase64(buffer) {
  const bytes = new Uint8Array(buffer);
  let binary = "";
  for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
  return btoa(binary);
}

export function base64ToArrayBuffer(base64) {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes.buffer;
}

export function pemToArrayBuffer(pem) {
  const base64 = pem
    .replace(/-----BEGIN (PUBLIC|PRIVATE) KEY-----/, "")
    .replace(/-----END (PUBLIC|PRIVATE) KEY-----/, "")
    .replace(/\s/g, "");
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes.buffer;
}

async function importRsaPublicKey(pem) {
  return {
    verifyKey: await crypto.subtle.importKey(
      "spki",
      pemToArrayBuffer(pem),
      { name: "RSA-PSS", hash: "SHA-256" },
      true,
      ["verify"]
    ),
    oaeKey: await crypto.subtle.importKey(
      "spki",
      pemToArrayBuffer(pem),
      { name: "RSA-OAEP", hash: "SHA-256" },
      false,
      ["encrypt"]
    ),
  };
}

async function performHandshake(baseUrl) {
  // 1. Server public key
  const pubRes = await fetch(`${baseUrl}/api/handshake/public_key`);
  if (!pubRes.ok) throw new Error(`Failed to fetch public key: ${pubRes.status}`);
  const pub = await pubRes.json();

  const { verifyKey, oaeKey } = await importRsaPublicKey(pub.server_public_key);

  // 2. Nonce + signature (server identity proof)
  const initRes = await fetch(`${baseUrl}/api/handshake/initiate`, { method: "POST" });
  if (!initRes.ok) throw new Error(`Handshake initiate failed: ${initRes.status}`);
  const { nonce, signature } = await initRes.json();

  const nonceBuf = base64ToArrayBuffer(nonce);
  const signatureBuf = base64ToArrayBuffer(signature);
  const valid = await crypto.subtle.verify(
    { name: "RSA-PSS", saltLength: 32 },
    verifyKey,
    signatureBuf,
    nonceBuf
  );
  if (!valid) throw new Error("Server identity verification failed");

  // 3. AES-GCM session key
  const aesKey = await crypto.subtle.generateKey(
    { name: "AES-GCM", length: 256 },
    true,
    ["encrypt", "decrypt"]
  );

  // 4. Wrap with RSA-OAEP
  const rawAes = await crypto.subtle.exportKey("raw", aesKey);
  const encryptedAes = await crypto.subtle.encrypt({ name: "RSA-OAEP" }, oaeKey, rawAes);

  // 5. Exchange
  const exRes = await fetch(`${baseUrl}/api/handshake/exchange`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ encrypted_session_key: arrayBufferToBase64(encryptedAes) }),
  });
  if (!exRes.ok) throw new Error(`Handshake exchange failed: ${exRes.status}`);
  const { session_id } = await exRes.json();

  return { key: aesKey, id: session_id, baseUrl };
}

let handshakePromise = null;

export async function ensureSession() {
  if (session.key && session.id) return session;
  if (!handshakePromise) {
    handshakePromise = performHandshake("").catch((e) => {
      handshakePromise = null;
      throw e;
    });
  }
  session = await handshakePromise;
  handshakePromise = null;
  return session;
}

export async function encryptAesGcm(key, plaintext, aad) {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const encoder = new TextEncoder();
  const data = encoder.encode(plaintext);
  const params = { name: "AES-GCM", iv };
  if (aad) params.additionalData = encoder.encode(aad);
  const ciphertext = await crypto.subtle.encrypt(params, key, data);
  return { iv: arrayBufferToBase64(iv.buffer), ciphertext: arrayBufferToBase64(ciphertext) };
}

export async function decryptAesGcm(key, ivB64, ciphertextB64, aad) {
  const iv = new Uint8Array(base64ToArrayBuffer(ivB64));
  const ct = base64ToArrayBuffer(ciphertextB64);
  const encoder = new TextEncoder();
  const params = { name: "AES-GCM", iv };
  if (aad) params.additionalData = encoder.encode(aad);
  const plaintext = await crypto.subtle.decrypt(params, key, ct);
  return new TextDecoder().decode(plaintext);
}

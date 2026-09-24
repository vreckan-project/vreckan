// SSO / OIDC section: connect Vreckan to an OpenID Connect provider. The
// admin/user group lists use a small tag input; each field shows a badge for
// its current value source (db / env / default).
//
// The small DOM helpers ($, esc, toast, request) come from util.js; `render`
// comes from admin.js.
import { $, esc, toast, request } from "../util.js";
import { t } from "../i18n.js";
import { render } from "../admin.js";

function setPanel(html) {
  const panel = $("admin-section");
  if (panel) panel.innerHTML = html;
}

// --- SSO / OIDC -----------------------------------------------------------
// A small tag input: chips for the current values, a text box to add more
// (Enter or comma), and an × on each chip to remove it. Used for the SSO
// admin/user group lists.
function tagInputHtml(id, values) {
  const chips = (values || [])
    .map((v) => `<span class="tag-chip" data-name="${esc(v)}">${esc(v)}<button type="button" class="tag-remove" title="${t("common.remove")}">×</button></span>`)
    .join("");
  return `<div class="tag-input" id="${id}">${chips}<input type="text" placeholder="${t("sso.tagPlaceholder")}"></div>`;
}

function wireTagInput(id) {
  const box = $(id);
  if (!box) return;
  const input = box.querySelector("input");
  const add = (raw) => {
    const v = raw.trim().replace(/,+$/, "");
    if (!v) return;
    if (Array.from(box.querySelectorAll(".tag-chip")).some((c) => c.dataset.name === v)) return;
    const chip = document.createElement("span");
    chip.className = "tag-chip";
    chip.dataset.name = v;
    chip.innerHTML = `${esc(v)}<button type="button" class="tag-remove" title="${t("common.remove")}">×</button>`;
    box.insertBefore(chip, input);
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      add(input.value);
      input.value = "";
    } else if (e.key === "Backspace" && !input.value) {
      const chips = box.querySelectorAll(".tag-chip");
      if (chips.length) chips[chips.length - 1].remove();
    }
  });
  box.addEventListener("click", (e) => {
    if (e.target.classList.contains("tag-remove")) e.target.closest(".tag-chip").remove();
  });
}

function readTagInput(id) {
  const box = $(id);
  if (!box) return [];
  return Array.from(box.querySelectorAll(".tag-chip")).map((c) => c.dataset.name);
}

// A small source badge showing where a value currently comes from.
function ssoSourceBadge(source) {
  const label = source === "db" ? t("sso.srcDb") : source === "env" ? t("sso.srcEnv") : t("sso.srcDefault");
  return `<span class="src-badge${source === "db" ? " db" : ""}">${label}</span>`;
}

async function renderSso() {
  const res = await request("/api/admin/sso", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>${t("sso.heading")}</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const v = res.data.values || {};
  const src = res.data.sources || {};
  const badge = (name) => ssoSourceBadge(src[name] || "default");

  setPanel(`
    <div class="card">
      <h3>${t("sso.heading")}</h3>
      <p class="muted">${t("sso.help")}</p>

      <div class="field" style="margin-top:12px;">
        <label>${t("sso.enableLabel")} ${badge("oidc_enabled")}</label>
        <label class="check"><input type="checkbox" id="sso-enabled" ${v.oidc_enabled ? "checked" : ""}> ${t("sso.enabled")}</label>
      </div>

      <div class="field" style="margin-top:12px;">
        <label>${t("sso.issuerLabel")} ${badge("oidc_issuer")}</label>
        <input type="text" id="sso-issuer" value="${esc(v.oidc_issuer || "")}">
        <p class="muted" style="margin-top:4px;">${t("sso.issuerHelp")}</p>
      </div>

      <div class="admin-form-row" style="margin-top:12px;">
        <div class="field" style="flex:1;min-width:220px;">
          <label>${t("sso.clientIdLabel")} ${badge("oidc_client_id")}</label>
          <input type="text" id="sso-client-id" value="${esc(v.oidc_client_id || "")}">
        </div>
        <div class="field" style="flex:1;min-width:220px;">
          <label>${t("sso.clientSecretLabel")} ${badge("oidc_client_secret")}</label>
          <input type="password" id="sso-client-secret" value="" placeholder="${v.secret_set ? t("sso.secretSet", { length: v.secret_length }) : t("sso.secretOptional")}">
        </div>
      </div>

      <div class="admin-form-row" style="margin-top:12px;">
        <div class="field" style="flex:1;min-width:220px;">
          <label>${t("sso.redirectUriLabel")} ${badge("oidc_redirect_uri")}</label>
          <input type="text" id="sso-redirect-uri" value="${esc(v.oidc_redirect_uri || "")}" placeholder="${t("sso.redirectUriPlaceholder")}">
        </div>
        <div class="field" style="flex:1;min-width:220px;">
          <label>${t("sso.scopesLabel")} ${badge("oidc_scopes")}</label>
          <input type="text" id="sso-scopes" value="${esc(v.oidc_scopes || "")}">
        </div>
      </div>

      <div class="admin-form-row" style="margin-top:12px;">
        <div class="field" style="flex:1;min-width:220px;">
          <label>${t("sso.allowSignupLabel")} ${badge("oidc_allow_signup")}</label>
          <label class="check"><input type="checkbox" id="sso-allow-signup" ${v.oidc_allow_signup ? "checked" : ""}> ${t("sso.allowSignupHelp")}</label>
        </div>
        <div class="field" style="flex:1;min-width:220px;">
          <label>${t("sso.stateTtlLabel")} ${badge("oidc_state_ttl_seconds")}</label>
          <input type="number" id="sso-state-ttl" value="${v.oidc_state_ttl_seconds ?? 600}">
        </div>
      </div>

      <div style="margin-top:14px;display:flex;gap:8px;flex-wrap:wrap;align-items:center;">
        <button class="btn btn-ghost" id="sso-test" type="button">${t("sso.testButton")}</button>
        <span class="test-result" id="sso-test-result"></span>
      </div>
    </div>

    <div class="card">
      <h3>${t("sso.groupMappingTitle")}</h3>
      <p class="muted">${t("sso.groupMappingHelp", { groups: t("nav.groups") })}</p>

      <div class="field" style="margin-top:12px;">
        <label>${t("sso.adminGroupsLabel")} ${badge("oidc_admin_groups")}</label>
        ${tagInputHtml("sso-admin-groups", v.oidc_admin_groups || [])}
        <p class="muted" style="margin-top:4px;">${t("sso.adminGroupsHelp")}</p>
      </div>

      <div class="field" style="margin-top:12px;">
        <label>${t("sso.userGroupsLabel")} ${badge("oidc_user_groups")}</label>
        ${tagInputHtml("sso-user-groups", v.oidc_user_groups || [])}
        <p class="muted" style="margin-top:4px;">${t("sso.userGroupsHelp")}</p>
      </div>
    </div>

    <div style="margin-top:14px;display:flex;gap:8px;justify-content:flex-end;">
      <button class="btn btn-primary" id="sso-save" type="button">${t("sso.save")}</button>
    </div>
  `);

  wireTagInput("sso-admin-groups");
  wireTagInput("sso-user-groups");

  $("sso-test").addEventListener("click", async () => {
    const out = $("sso-test-result");
    out.className = "test-result";
    out.textContent = t("sso.testing");
    const r = await request("/api/admin/sso/test", "POST", {});
    if (r.error) {
      out.classList.add("err");
      out.textContent = "✗ " + r.error.message;
      return;
    }
    out.classList.add("ok");
    out.innerHTML = `✓ ${t("sso.connected", { issuer: esc(r.data.issuer || "") })}
      <span class="muted">${t("sso.endpoints", { authorization: esc(r.data.authorization_endpoint || "—"), userinfo: esc(r.data.userinfo_endpoint || "—"), token: esc(r.data.token_endpoint || "—") })}</span>`;
  });

  $("sso-save").addEventListener("click", async () => {
    const payload = {
      oidc_enabled: $("sso-enabled").checked,
      oidc_issuer: $("sso-issuer").value,
      oidc_client_id: $("sso-client-id").value,
      oidc_client_secret: $("sso-client-secret").value,
      oidc_redirect_uri: $("sso-redirect-uri").value,
      oidc_scopes: $("sso-scopes").value,
      oidc_allow_signup: $("sso-allow-signup").checked,
      oidc_state_ttl_seconds: Number($("sso-state-ttl").value) || 600,
      oidc_admin_groups: readTagInput("sso-admin-groups"),
      oidc_user_groups: readTagInput("sso-user-groups"),
    };
    const r = await request("/api/admin/sso", "PUT", payload);
    if (r.error) return toast(r.error.message, "error");
    toast(t("sso.saved"));
    render();
  });
}

export { renderSso };

// Certificates section: configure Let's Encrypt (certbot) issuance and show
// the current certificate status. Mirrors the SSO page: each field shows a
// source badge (db / env / default), the Cloudflare token is a password field
// (blank = keep the existing one), and domains use a tag input (stored
// comma-joined).
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

// --- Tag input (domains) ----------------------------------------------------
function tagInputHtml(id, values) {
  const chips = (values || [])
    .map((v) => `<span class="tag-chip" data-name="${esc(v)}">${esc(v)}<button type="button" class="tag-remove" title="${t("common.remove")}">×</button></span>`)
    .join("");
  return `<div class="tag-input" id="${id}">${chips}<input type="text" placeholder="${t("certificates.domainsPlaceholder")}"></div>`;
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

function sourceBadge(source) {
  const label = source === "db" ? t("certificates.srcDb") : source === "env" ? t("certificates.srcEnv") : t("certificates.srcDefault");
  return `<span class="src-badge${source === "db" ? " db" : ""}">${label}</span>`;
}

// Render the current certificate status card.
function statusHtml(status) {
  if (!status || !status.present) {
    return `<div class="card">
      <h3>${t("certificates.statusTitle")}</h3>
      <p class="muted">${t("certificates.noCert")}</p>
    </div>`;
  }
  const days = status.days_remaining;
  const daysLabel =
    days === null
      ? t("certificates.daysUnknown")
      : days < 0
        ? t("certificates.expired", { days: Math.abs(days) })
        : t("certificates.daysRemaining", { days: days });
  const validCls = status.valid ? "ok" : "err";
  const validLabel = status.valid ? t("certificates.valid") : t("certificates.invalid");
  return `<div class="card">
    <h3>${t("certificates.statusTitle")}</h3>
    <div class="field mt-8"><label>${t("certificates.source")}</label><div>${esc(status.source || "—")}</div></div>
    <div class="field mt-8"><label>${t("certificates.expiry")}</label><div>${esc(status.expiry || "—")}</div></div>
    <div class="field mt-8"><label>${t("certificates.daysRemainingLabel")}</label><div>${daysLabel}</div></div>
    <div class="mt-8"><span class="test-result ${validCls}">${validLabel}</span></div>
  </div>`;
}

async function renderCertificates() {
  const res = await request("/api/admin/certificates", "GET");
  if (res.error) {
    setPanel(`<div class="card"><h3>${t("nav.certificates")}</h3><div class="error">${esc(res.error.message)}</div></div>`);
    return;
  }
  const v = res.data.values || {};
  const src = res.data.sources || {};
  const status = res.data.status || {};
  const badge = (name) => sourceBadge(src[name] || "default");

  setPanel(`
    ${statusHtml(status)}

    <div class="card">
      <h3>${t("nav.certificates")}</h3>
      <p class="muted">${t("certificates.help")}</p>

      <div class="field mt-12">
        <label>${t("certificates.enableLabel")} ${badge("le_enabled")}</label>
        <label class="check"><input type="checkbox" id="le-enabled" ${v.le_enabled ? "checked" : ""}> ${t("certificates.enabled")}</label>
      </div>

      <div class="field mt-12">
        <label>${t("certificates.domainsLabel")} ${badge("le_domains")}</label>
        ${tagInputHtml("le-domains", v.le_domains || [])}
        <p class="muted mt-4">${t("certificates.domainsHelp")}</p>
      </div>

      <div class="admin-form-row mt-12">
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.authLabel")} ${badge("le_auth")}</label>
          <select id="le-auth">
            <option value="dns-cloudflare" ${v.le_auth === "dns-cloudflare" ? "selected" : ""}>${t("certificates.authDns")}</option>
            <option value="http-01" ${v.le_auth === "http-01" ? "selected" : ""}>${t("certificates.authHttp")}</option>
          </select>
          <p class="muted mt-4">${t("certificates.authHelp")}</p>
        </div>
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.stagingLabel")} ${badge("le_staging")}</label>
          <label class="check"><input type="checkbox" id="le-staging" ${v.le_staging ? "checked" : ""}> ${t("certificates.staging")}</label>
          <p class="muted mt-4">${t("certificates.stagingHelp")}</p>
        </div>
      </div>

      <div class="admin-form-row mt-12">
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.emailLabel")} ${badge("le_cloudflare_email")}</label>
          <input type="text" id="le-email" value="${esc(v.le_cloudflare_email || "")}">
          <p class="muted mt-4">${t("certificates.emailHelp")}</p>
        </div>
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.tokenLabel")} ${badge("le_cloudflare_token")}</label>
          <input type="password" id="le-token" value="" placeholder="${v.token_set ? t("certificates.tokenSet", { length: v.token_length }) : t("certificates.tokenOptional")}">
          <p class="muted mt-4">${t("certificates.tokenHelp")}</p>
        </div>
      </div>

      <div class="admin-form-row mt-12">
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.autoRenewLabel")} ${badge("le_auto_renew")}</label>
          <label class="check"><input type="checkbox" id="le-auto-renew" ${v.le_auto_renew ? "checked" : ""}> ${t("certificates.autoRenew")}</label>
        </div>
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.checkHoursLabel")} ${badge("le_renew_check_hours")}</label>
          <input type="number" id="le-check-hours" value="${v.le_renew_check_hours ?? 6}">
          <p class="muted mt-4">${t("certificates.checkHoursHelp")}</p>
        </div>
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.renewBeforeLabel")} ${badge("le_renew_before_days")}</label>
          <input type="number" id="le-renew-before" value="${v.le_renew_before_days ?? 30}">
          <p class="muted mt-4">${t("certificates.renewBeforeHelp")}</p>
        </div>
      </div>

      <div class="mt-14 row gap-8 row-end">
        <button class="btn btn-ghost" id="le-renew" type="button">${t("certificates.renewNow")}</button>
        <button class="btn btn-primary" id="le-save" type="button">${t("certificates.save")}</button>
      </div>
      <div class="mt-8"><span class="test-result" id="le-renew-result"></span></div>
    </div>
  `);

  wireTagInput("le-domains");

  $("le-save").addEventListener("click", async () => {
    const payload = {
      le_enabled: $("le-enabled").checked,
      le_domains: readTagInput("le-domains"),
      le_auth: $("le-auth").value,
      le_cloudflare_email: $("le-email").value,
      le_cloudflare_token: $("le-token").value,
      le_staging: $("le-staging").checked,
      le_auto_renew: $("le-auto-renew").checked,
      le_renew_check_hours: Number($("le-check-hours").value) || 6,
      le_renew_before_days: Number($("le-renew-before").value) || 30,
    };
    const r = await request("/api/admin/certificates", "PUT", payload);
    if (r.error) return toast(r.error.message, "error");
    toast(t("certificates.saved"));
    render();
  });

  $("le-renew").addEventListener("click", async () => {
    const out = $("le-renew-result");
    out.className = "test-result";
    out.textContent = t("certificates.renewing");
    const r = await request("/api/admin/certificates/renew", "POST", {});
    if (r.error) {
      out.classList.add("err");
      out.textContent = "✗ " + r.error.message;
      return;
    }
    out.classList.add("ok");
    out.textContent = "✓ " + t("certificates.renewDone");
    render();
  });
}

export { renderCertificates };

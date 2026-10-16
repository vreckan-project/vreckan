// Certificates section: configure ACME issuance (native acmex client) and
// show the current certificate status. Mirrors the SSO page: each field shows
// a source badge (db / env / default), the Cloudflare token is a password
// field (blank = keep the existing one), and domains use a tag input (stored
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

// --- Select option builders -------------------------------------------------
function caOptions(selected) {
  const cas = [
    { id: "letsencrypt", label: t("certificates.caLetsencrypt") },
    { id: "zerossl", label: t("certificates.caZerossl") },
    { id: "google", label: t("certificates.caGoogle") },
  ];
  if (selected && !cas.some((c) => c.id === selected)) cas.unshift({ id: selected, label: selected });
  return cas.map((c) => `<option value="${esc(c.id)}" ${c.id === selected ? "selected" : ""}>${esc(c.label)}</option>`).join("");
}

function dnsProviderOptions(providers, selected) {
  const known =
    Array.isArray(providers) && providers.length
      ? providers.map((p) => ({ id: String(p.id || ""), label: p.label || p.id }))
      : [{ id: "cloudflare", label: "Cloudflare" }];
  if (selected && !known.some((p) => p.id === selected)) known.unshift({ id: selected, label: selected });
  return known.map((p) => `<option value="${esc(p.id)}" ${p.id === selected ? "selected" : ""}>${esc(p.label)}</option>`).join("");
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
  const ca = v.acme_ca || "letsencrypt";
  const provider = v.acme_dns_provider || "cloudflare";

  setPanel(`
    ${statusHtml(status)}

    <div class="card">
      <h3>${t("nav.certificates")}</h3>
      <p class="muted">${t("certificates.help")}</p>

      <div class="field mt-12">
        <label>${t("certificates.enableLabel")} ${badge("acme_enabled")}</label>
        <label class="check"><input type="checkbox" id="acme-enabled" ${v.acme_enabled ? "checked" : ""}> ${t("certificates.enabled")}</label>
      </div>

      <div class="field mt-12">
        <label>${t("certificates.domainsLabel")} ${badge("acme_domains")}</label>
        ${tagInputHtml("acme-domains", v.acme_domains || [])}
        <p class="muted mt-4">${t("certificates.domainsHelp")}</p>
      </div>

      <div class="admin-form-row mt-12">
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.caLabel")} ${badge("acme_ca")}</label>
          <select id="acme-ca">${caOptions(ca)}</select>
          <label class="check mt-4"><input type="checkbox" id="acme-allow-http-ca" ${v.acme_allow_http_ca ? "checked" : ""}> ${t("certificates.allowHttpCa")}</label>
          <p class="muted mt-4">${t("certificates.caHelp")}</p>
          <p class="muted mt-4">${t("certificates.allowHttpCaHelp")}</p>
        </div>
        <div class="field flex-1 min-w-220" id="acme-staging-field">
          <label>${t("certificates.stagingLabel")} ${badge("acme_staging")}</label>
          <label class="check"><input type="checkbox" id="acme-staging" ${v.acme_staging ? "checked" : ""}> ${t("certificates.staging")}</label>
          <p class="muted mt-4">${t("certificates.stagingHelp")}</p>
        </div>
      </div>

      <div class="admin-form-row mt-12">
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.authLabel")} ${badge("acme_auth")}</label>
          <select id="acme-auth">
            <option value="dns-cloudflare" ${v.acme_auth === "dns-cloudflare" ? "selected" : ""}>${t("certificates.authDns")}</option>
            <option value="http-01" ${v.acme_auth === "http-01" ? "selected" : ""}>${t("certificates.authHttp")}</option>
          </select>
          <p class="muted mt-4">${t("certificates.authHelp")}</p>
        </div>
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.dnsProviderLabel")} ${badge("acme_dns_provider")}</label>
          <select id="acme-dns-provider">${dnsProviderOptions(res.data.dns_providers, provider)}</select>
          <p class="muted mt-4">${t("certificates.dnsProviderHelp")}</p>
        </div>
      </div>

      <div class="admin-form-row mt-12" id="acme-cloudflare-fields">
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.zoneIdLabel")} ${badge("acme_cloudflare_zone_id")}</label>
          <input type="text" id="acme-zone-id" value="${esc(v.acme_cloudflare_zone_id || "")}">
          <p class="muted mt-4">${t("certificates.zoneIdHelp")}</p>
        </div>
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.emailLabel")} ${badge("acme_cloudflare_email")}</label>
          <input type="text" id="acme-email" value="${esc(v.acme_cloudflare_email || "")}">
          <p class="muted mt-4">${t("certificates.emailHelp")}</p>
        </div>
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.tokenLabel")} ${badge("acme_cloudflare_token")}</label>
          <input type="password" id="acme-token" value="" placeholder="${v.token_set ? t("certificates.tokenSet", { length: v.token_length }) : t("certificates.tokenOptional")}">
          <p class="muted mt-4">${t("certificates.tokenHelp")}</p>
        </div>
      </div>

      <div class="field mt-12" id="acme-provider-config-field">
        <label>${t("certificates.providerConfigLabel")} ${badge("acme_dns_provider_config")}</label>
        <textarea id="acme-provider-config" rows="6" placeholder="${esc(v.dns_provider_config_set ? t("certificates.providerConfigSet", { length: v.dns_provider_config_length }) : t("certificates.providerConfigPlaceholder"))}">${esc(v.acme_dns_provider_config || "")}</textarea>
        <p class="muted mt-4">${t("certificates.providerConfigHelp")}</p>
      </div>

      <div class="admin-form-row mt-12">
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.autoRenewLabel")} ${badge("acme_auto_renew")}</label>
          <label class="check"><input type="checkbox" id="acme-auto-renew" ${v.acme_auto_renew ? "checked" : ""}> ${t("certificates.autoRenew")}</label>
        </div>
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.checkHoursLabel")} ${badge("acme_renew_check_hours")}</label>
          <input type="number" id="acme-check-hours" value="${v.acme_renew_check_hours ?? 6}">
          <p class="muted mt-4">${t("certificates.checkHoursHelp")}</p>
        </div>
        <div class="field flex-1 min-w-220">
          <label>${t("certificates.renewBeforeLabel")} ${badge("acme_renew_before_days")}</label>
          <input type="number" id="acme-renew-before" value="${v.acme_renew_before_days ?? 30}">
          <p class="muted mt-4">${t("certificates.renewBeforeHelp")}</p>
        </div>
      </div>

      <div class="mt-14 row gap-8 row-end">
        <button class="btn btn-ghost" id="acme-renew" type="button">${t("certificates.renewNow")}</button>
        <button class="btn btn-primary" id="acme-save" type="button">${t("certificates.save")}</button>
      </div>
      <div class="mt-8"><span class="test-result" id="acme-renew-result"></span></div>
      <pre class="key-block mt-8" id="acme-renew-detail" hidden></pre>
    </div>
  `);

  wireTagInput("acme-domains");

  // Staging only exists for Let's Encrypt; the provider credentials only
  // matter for the DNS-01 challenge.
  function updateVisibility() {
    const caSel = $("acme-ca");
    const authSel = $("acme-auth");
    const provSel = $("acme-dns-provider");
    if (!caSel || !authSel || !provSel) return;
    const stagingOnly = caSel.value !== "letsencrypt";
    const stagingField = $("acme-staging-field");
    if (stagingField) stagingField.hidden = stagingOnly;
    const stagingBox = $("acme-staging");
    if (stagingBox) stagingBox.disabled = stagingOnly;
    const dns = authSel.value !== "http-01";
    const cfFields = $("acme-cloudflare-fields");
    if (cfFields) cfFields.hidden = !dns || provSel.value !== "cloudflare";
    const pcField = $("acme-provider-config-field");
    if (pcField) pcField.hidden = !dns || provSel.value === "cloudflare";
  }
  for (const id of ["acme-ca", "acme-auth", "acme-dns-provider"]) {
    $(id).addEventListener("change", updateVisibility);
  }
  updateVisibility();

  $("acme-save").addEventListener("click", async () => {
    const payload = {
      acme_enabled: $("acme-enabled").checked,
      acme_ca: $("acme-ca").value,
      acme_allow_http_ca: $("acme-allow-http-ca").checked,
      acme_domains: readTagInput("acme-domains"),
      acme_auth: $("acme-auth").value,
      acme_dns_provider: $("acme-dns-provider").value,
      acme_cloudflare_zone_id: $("acme-zone-id").value,
      acme_dns_provider_config: $("acme-provider-config").value,
      acme_cloudflare_email: $("acme-email").value,
      acme_cloudflare_token: $("acme-token").value,
      acme_staging: $("acme-staging").checked,
      acme_auto_renew: $("acme-auto-renew").checked,
      acme_renew_check_hours: Number($("acme-check-hours").value) || 6,
      acme_renew_before_days: Number($("acme-renew-before").value) || 30,
    };
    const r = await request("/api/admin/certificates", "PUT", payload);
    if (r.error) return toast(r.error.message, "error");
    toast(t("certificates.saved"));
    render();
  });

  $("acme-renew").addEventListener("click", async () => {
    const out = $("acme-renew-result");
    const detail = $("acme-renew-detail");
    out.className = "test-result";
    out.textContent = t("certificates.renewing");
    detail.hidden = true;
    detail.textContent = "";
    const r = await request("/api/admin/certificates/renew", "POST", {});
    if (r.error) {
      out.classList.add("err");
      out.textContent = "✗ " + r.error.message;
      render();
      return;
    }
    const d = r.data || {};
    const issued = d.issued === true || d.renewed === true;
    const error = typeof d.error === "string" && d.error.trim() ? d.error : "";
    const logs = typeof d.logs === "string" && d.logs.trim() ? d.logs : "";
    if (issued) {
      out.classList.add("ok");
      out.textContent = "✓ " + t("certificates.renewDone");
    } else if (error) {
      out.classList.add("err");
      out.textContent = "✗ " + t("certificates.renewFailed");
    } else {
      out.textContent = "– " + t("certificates.renewNoop");
    }
    const parts = [];
    if (error) parts.push(`${t("certificates.renewErrorLabel")}\n${error}`);
    if (logs) parts.push(logs);
    if (parts.length) {
      detail.textContent = parts.join("\n\n");
      detail.hidden = false;
    }
    render();
  });
}

export { renderCertificates };

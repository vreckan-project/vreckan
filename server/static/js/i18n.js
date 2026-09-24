// Vreckan i18n runtime.
//
// Ported from the upstream SealSkin client's lib/i18n.js, adapted for the
// hand-written SPA (no build step, no __I18N_FILES__ injection): the language
// files are served from /i18n/<lang>.json. Each non-English file is a *partial*
// dictionary (it omits keys it doesn't translate), so every load deep-merges
// the chosen language over the English base — mirroring what the upstream
// esbuild step did at build time.
//
// Usage:
//   import { loadTranslator, applyTranslations, t, setLanguage, SUPPORTED_UI_LANGS } from "./i18n.js";
//   await loadTranslator(navigator.language);   // fetch + merge + activate
//   applyTranslations(document.body);           // translate data-i18n* nodes
//   t("common.cancel");                          // translate a JS string
//
// The room page (collaboration/) keeps its own self-contained translation.js;
// this module is for the main SPA only.

const I18N_DIR = "/i18n/";
const cache = new Map(); // lang -> Promise<mergedDict>

// The 18 languages that ship a translation file.
export const SUPPORTED_UI_LANGS = [
  { code: "en", label: "English" },
  { code: "ar", label: "العربية" },
  { code: "da", label: "Dansk" },
  { code: "de", label: "Deutsch" },
  { code: "es", label: "Español" },
  { code: "fil", label: "Filipino" },
  { code: "fr", label: "Français" },
  { code: "hi", label: "हिन्दी" },
  { code: "it", label: "Italiano" },
  { code: "ja", label: "日本語" },
  { code: "ko", label: "한국어" },
  { code: "nl", label: "Nederlands" },
  { code: "pt", label: "Português" },
  { code: "ru", label: "Русский" },
  { code: "th", label: "ไทย" },
  { code: "tr", label: "Türkçe" },
  { code: "vi", label: "Tiếng Việt" },
  { code: "zh", label: "中文" },
];

// Reduce a locale (e.g. "pt-BR", "en_US") to a supported language code.
export function resolveLanguage(locale) {
  const base = String(locale || "en").split(/[-_]/)[0].toLowerCase();
  return SUPPORTED_UI_LANGS.some((l) => l.code === base) ? base : "en";
}

function lookup(dict, key) {
  return key.split(".").reduce((obj, k) => (obj && obj[k] !== undefined ? obj[k] : undefined), dict);
}

// Build the `t` function over one dictionary. Keeps the upstream plural and
// placeholder semantics: `{count, plural, one {..} other {..}}` then `{name}`.
function makeT(dict) {
  return (key, variables = {}) => {
    let value = lookup(dict, key);
    if (value === undefined) {
      if (typeof console !== "undefined" && console.warn) console.warn(`[i18n] missing key: ${key}`);
      return key;
    }
    if (typeof value !== "string") return value;
    let out = value.replace(/\{(\w+),\s*plural,\s*(.*)\}/g, (match, varName, rulesStr) => {
      if (!Object.prototype.hasOwnProperty.call(variables, varName)) return match;
      const count = variables[varName];
      const rules = {};
      const ruleRegex = /(\w+)\s*\{((?:[^{}]|{[^{}]*})*)\}/g;
      let m;
      while ((m = ruleRegex.exec(rulesStr)) !== null) rules[m[1]] = m[2];
      if (count === 1 && rules.one) return rules.one;
      if (rules.other) return rules.other;
      return match;
    });
    for (const placeholder in variables) {
      out = out.split(`{${placeholder}}`).join(String(variables[placeholder]));
    }
    return out;
  };
}

function deepMerge(base, extra) {
  for (const k in extra) {
    if (extra[k] && typeof extra[k] === "object" && !Array.isArray(extra[k]) && base[k] && typeof base[k] === "object") {
      deepMerge(base[k], extra[k]);
    } else {
      base[k] = extra[k];
    }
  }
  return base;
}

async function fetchJson(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`Failed to load ${url}: ${res.status}`);
  return res.json();
}

async function loadDict(lang) {
  if (cache.has(lang)) return cache.get(lang);
  const promise = (async () => {
    const en = await fetchJson(I18N_DIR + "en.json");
    if (lang === "en") return en;
    try {
      const partial = await fetchJson(I18N_DIR + lang + ".json");
      return deepMerge(en, partial);
    } catch (e) {
      console.error(`[i18n] falling back to English for '${lang}':`, e);
      return en;
    }
  })();
  cache.set(lang, promise);
  return promise;
}

// --- Active translator (module-level so `t()` always uses the current lang) ---
let currentDict = {};
let currentT = makeT(currentDict);
let activeLang = "en";

// Translate a key with the *currently active* language. Safe to call before
// loadTranslator resolves (falls back to the key itself).
export function t(key, variables) {
  return currentT(key, variables);
}

export function activeLanguage() {
  return activeLang;
}

/**
 * Load + activate the translator for a locale and return its `t` function.
 * Unknown locales and fetch failures fall back to English.
 */
export async function loadTranslator(locale) {
  const lang = resolveLanguage(locale);
  currentDict = await loadDict(lang);
  currentT = makeT(currentDict);
  activeLang = lang;
  return currentT;
}

/**
 * Apply `data-i18n`, `data-i18n-placeholder`, and `data-i18n-title` attributes
 * within a scope using the active translator.
 */
export function applyTranslations(scope = document.body) {
  scope.querySelectorAll("[data-i18n]").forEach((el) => {
    // Replace only the element's own text nodes so nested children
    // (e.g. <input> inside a <label>) survive translation.
    const translated = t(el.getAttribute("data-i18n"));
    let replaced = false;
    for (const node of Array.from(el.childNodes)) {
      if (node.nodeType === Node.TEXT_NODE) {
        if (node.nodeValue.trim()) {
          node.nodeValue = translated;
          replaced = true;
        } else {
          node.remove();
        }
      }
    }
    if (!replaced) el.insertBefore(document.createTextNode(translated), el.firstChild);
  });
  scope.querySelectorAll("[data-i18n-placeholder]").forEach((el) => {
    el.setAttribute("placeholder", t(el.getAttribute("data-i18n-placeholder")));
  });
  scope.querySelectorAll("[data-i18n-title]").forEach((el) => {
    el.setAttribute("title", t(el.getAttribute("data-i18n-title")));
  });
}

/**
 * Switch the active language: reload the dictionary, re-translate the static
 * DOM, and dispatch a `vreckan:langchange` event so JS-rendered views can
 * re-render themselves.
 */
export async function setLanguage(locale) {
  await loadTranslator(locale);
  applyTranslations(document.body);
  document.documentElement.lang = activeLang;
  document.dispatchEvent(new CustomEvent("vreckan:langchange", { detail: { lang: activeLang } }));
  return activeLang;
}

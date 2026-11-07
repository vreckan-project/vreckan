import type { PluginContext } from "@paperclipai/plugin-sdk";
import {
  DEFAULT_GATE_CONFIG,
  normalizeGateConfig,
  type CompanySnapshot,
  type DeferralRecord,
  type GateConfig,
} from "./types.js";

/**
 * Durable per-company state over the host's `plugin_state` store.
 *
 * Keys (all company-scoped, plugin namespace "wip-gate"):
 * - "config"        — GateConfig
 * - "snapshot"      — the last successfully computed CompanySnapshot
 * - "deferrals"     — Record<runId, DeferralRecord> (age-out bookkeeping)
 *
 * The worker mirrors everything into an in-memory map on read and writes
 * back after mutations, so the hot path (schedulingGate consults) never
 * touches the database: it only ever reads/writes the in-memory mirror. The
 * state writes happen on refresh and on decision, which are already
 * background-tolerant paths.
 */

const NAMESPACE = "wip-gate";
const CONFIG_KEY = "config";
const SNAPSHOT_KEY = "snapshot";
const DEFERRALS_KEY = "deferrals";

function scope(companyId: string, stateKey: string) {
  return { scopeKind: "company" as const, scopeId: companyId, namespace: NAMESPACE, stateKey };
}

export interface CompanyState {
  config: GateConfig;
  snapshot: CompanySnapshot | null;
  deferrals: Record<string, DeferralRecord>;
}

export async function loadCompanyState(
  ctx: Pick<PluginContext, "state">,
  companyId: string,
): Promise<CompanyState> {
  const [configRaw, snapshotRaw, deferralsRaw] = await Promise.all([
    ctx.state.get(scope(companyId, CONFIG_KEY)),
    ctx.state.get(scope(companyId, SNAPSHOT_KEY)),
    ctx.state.get(scope(companyId, DEFERRALS_KEY)),
  ]);
  return {
    config: normalizeGateConfig(configRaw),
    snapshot: isSnapshot(snapshotRaw) ? snapshotRaw : null,
    deferrals: normalizeDeferrals(deferralsRaw),
  };
}

export async function persistCompanyState(
  ctx: Pick<PluginContext, "state">,
  companyId: string,
  state: Pick<CompanyState, "config" | "snapshot" | "deferrals">,
): Promise<void> {
  await Promise.all([
    ctx.state.set(scope(companyId, CONFIG_KEY), state.config),
    ctx.state.set(scope(companyId, SNAPSHOT_KEY), state.snapshot),
    ctx.state.set(scope(companyId, DEFERRALS_KEY), state.deferrals),
  ]);
}

export function isSnapshot(value: unknown): value is CompanySnapshot {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const record = value as Record<string, unknown>;
  if (typeof record.companyId !== "string") return false;
  if (typeof record.refreshedAtMs !== "number") return false;
  if (!record.agents || typeof record.agents !== "object") return false;
  if (!record.issues || typeof record.issues !== "object") return false;
  return true;
}

export function normalizeDeferrals(value: unknown): Record<string, DeferralRecord> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  const out: Record<string, DeferralRecord> = {};
  for (const [runId, raw] of Object.entries(value as Record<string, unknown>)) {
    if (!raw || typeof raw !== "object") continue;
    const record = raw as Record<string, unknown>;
    if (typeof record.firstDeferredAtMs !== "number") continue;
    out[runId] = {
      firstDeferredAtMs: record.firstDeferredAtMs,
      lastReason: typeof record.lastReason === "string" ? record.lastReason : "",
    };
  }
  return out;
}

export const DEFAULT_CONFIG = DEFAULT_GATE_CONFIG;

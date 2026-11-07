/**
 * SchedulingGate — the host-enforced, company-wide in-flight run cap.
 *
 * The gate makes `inFlight + reserved <= maxInFlight` hold at claim time,
 * even under a burst of concurrent claim passes across different agents
 * (per-agent start locks do not serialize across agents). A claim pass
 * reserves slots up front, converts a reservation to in-flight when the
 * execution actually starts, and releases it when the claimed run never
 * starts. The cap only limits starts; in-flight runs run to completion.
 *
 * Counters live in process memory, shared across heartbeat service
 * instances exactly like the module-level `activeRunExecutions` set in
 * heartbeat.ts. They are seeded from `heartbeat_runs WHERE status =
 * 'running'` at startup so runs re-adopted by restart recovery — which
 * bypass the add/remove mirror sites — are counted, and the seed keeps
 * the mirror idempotent (a re-executed seeded run is not double-counted).
 *
 * The optional plugin voice is fail-open: a plugin that does not
 * implement the `schedulingGate` method is invisible to the gate (zero
 * behavior change), and any consultation failure — timeout, MethodNotFound,
 * worker down, malformed result — leaves the host count standing. The cap
 * itself is host-enforced and never degrades because of a plugin.
 *
 * @see PLUGIN_SPEC.md §13.11 — `schedulingGate`
 */
import {
  COMPANY_SCHEDULING_DEFAULTS,
  COMPANY_SCHEDULING_MAX_INFLIGHT_CAP,
  type CompanySchedulingConfig,
} from "@paperclipai/shared";
import { agents, companies, heartbeatRuns, type Db } from "@paperclipai/db";
import { and, eq, sql } from "drizzle-orm";
import type { PluginWorkerManager } from "../plugin-worker-manager.js";
import { logger } from "../../middleware/logger.js";

const log = logger.child({ service: "scheduling-gate" });

/** Explicit RPC budget for one plugin schedulingGate consultation. */
export const SCHEDULING_GATE_CONSULT_TIMEOUT_MS = 2_000;

/** The run summary passed to plugin schedulingGate consultations. */
export interface SchedulingGateRunSummary {
  runId: string;
  agentId: string;
  companyId: string;
  status: string;
  issueId: string | null;
  invocationSource: string | null;
  triggerDetail: string | null;
  createdAt: string;
}

export interface SchedulingGateConsultParams {
  companyId: string;
  agentId: string;
  run: SchedulingGateRunSummary;
  hostSlotsRemaining: number;
}

export interface ResolvedSchedulingConfig {
  enabled: boolean;
  maxInFlight: number;
}

/** The plugin that voiced on a gate decision, for logs and the status view. */
export interface SchedulingGatePluginDescriptor {
  pluginId: string;
  slots?: number;
  defer?: boolean;
  reason?: string;
}

export interface SchedulingGateConsultResult {
  /** Slots the plugin wants granted (may only decrease hostSlotsRemaining). */
  slots?: number;
  /** True to defer this claim entirely for now. */
  defer?: boolean;
  reason?: string;
}

/** Live gate state for one company, as reported by the board API. */
export interface SchedulingGateStatus {
  /** Defaults-applied config, i.e. what the gate is actually enforcing. */
  config: ResolvedSchedulingConfig;
  inFlight: number;
  reserved: number;
  maxInFlight: number;
  /** null while the gate is disabled (no cap, so no meaningful remainder). */
  slotsRemaining: number | null;
  queuedByAgent: Array<{ agentId: string; agentName: string | null; queued: number }>;
  lastDecision: {
    at: string;
    agentId: string;
    runId: string | null;
    perAgentSlots: number;
    hostSlotsRemaining: number | null;
    granted: number;
    plugin: SchedulingGatePluginDescriptor | null;
    outcome: string;
  } | null;
}

interface TrackedRun {
  state: "reserved" | "active";
  companyId: string;
}

interface GateDecisionRecord {
  at: Date;
  companyId: string;
  agentId: string;
  runId: string | null;
  perAgentSlots: number;
  hostSlotsRemaining: number | null;
  granted: number;
  plugin: SchedulingGatePluginDescriptor | null;
  outcome: string;
}

function normalizeSchedulingConfig(
  stored: CompanySchedulingConfig | null | undefined,
): ResolvedSchedulingConfig {
  const config: ResolvedSchedulingConfig = { ...COMPANY_SCHEDULING_DEFAULTS };
  if (stored && typeof stored === "object") {
    if (typeof stored.enabled === "boolean") config.enabled = stored.enabled;
    if (
      typeof stored.maxInFlight === "number" &&
      Number.isInteger(stored.maxInFlight) &&
      stored.maxInFlight >= 1 &&
      stored.maxInFlight <= COMPANY_SCHEDULING_MAX_INFLIGHT_CAP
    ) {
      config.maxInFlight = stored.maxInFlight;
    }
  }
  return config;
}

/** Validate a board PATCH body. Throws an Error with a user-facing message. */
export function validateSchedulingConfigPatch(
  patch: unknown,
): Partial<{ enabled: boolean; maxInFlight: number }> {
  if (patch === null || typeof patch !== "object" || Array.isArray(patch)) {
    throw new Error("scheduling config must be a JSON object");
  }
  const record = patch as Record<string, unknown>;
  const result: Partial<{ enabled: boolean; maxInFlight: number }> = {};
  for (const key of Object.keys(record)) {
    if (key === "enabled") {
      if (typeof record.enabled !== "boolean") {
        throw new Error("enabled must be a boolean");
      }
      result.enabled = record.enabled;
    } else if (key === "maxInFlight") {
      if (
        typeof record.maxInFlight !== "number" ||
        !Number.isInteger(record.maxInFlight) ||
        record.maxInFlight < 1 ||
        record.maxInFlight > COMPANY_SCHEDULING_MAX_INFLIGHT_CAP
      ) {
        throw new Error(
          `maxInFlight must be an integer between 1 and ${COMPANY_SCHEDULING_MAX_INFLIGHT_CAP}`,
        );
      }
      result.maxInFlight = record.maxInFlight;
    } else {
      throw new Error(`unknown scheduling config field: ${key}`);
    }
  }
  return result;
}

export class SchedulingGate {
  private db: Db | null = null;
  private pluginWorkerManager: PluginWorkerManager | null = null;
  private inFlightByCompany = new Map<string, number>();
  private reservedByCompany = new Map<string, number>();
  private trackedRuns = new Map<string, TrackedRun>();
  private configCache = new Map<string, ResolvedSchedulingConfig>();
  // Per-company so a multi-company host never shows company A the last gate
  // decision of company B.
  private lastDecisionByCompany = new Map<string, GateDecisionRecord>();
  private seeded = false;
  private seededCount = 0;

  /**
   * Bind the process database (and, when available, the plugin worker
   * manager for schedulingGate consultations). Idempotent: several
   * heartbeatService instances bind the same process state.
   */
  bind(db: Db, pluginWorkerManager?: PluginWorkerManager | null): void {
    if (!this.db) this.db = db;
    if (pluginWorkerManager) this.pluginWorkerManager = pluginWorkerManager;
  }

  /** Test seam: clear all in-memory state. */
  reset(): void {
    this.db = null;
    this.pluginWorkerManager = null;
    this.inFlightByCompany.clear();
    this.reservedByCompany.clear();
    this.trackedRuns.clear();
    this.configCache.clear();
    this.lastDecisionByCompany.clear();
    this.seeded = false;
    this.seededCount = 0;
  }

  /**
   * Seed the counters from `heartbeat_runs WHERE status = 'running'`.
   * Call once at startup, before any claim pass or restart re-adoption can
   * run. Idempotent: later calls return the recorded count. Runs counted
   * here are marked active so the mirror sites (which fire when recovery
   * re-executes a re-adopted run) do not double-count them.
   */
  async seedFromDatabase(): Promise<number> {
    if (this.seeded) return this.seededCount;
    const db = this.db;
    if (!db) return 0;
    const rows = await db
      .select({ id: heartbeatRuns.id, companyId: heartbeatRuns.companyId })
      .from(heartbeatRuns)
      .where(eq(heartbeatRuns.status, "running"));
    this.inFlightByCompany.clear();
    this.reservedByCompany.clear();
    this.trackedRuns.clear();
    for (const row of rows) {
      this.inFlightByCompany.set(
        row.companyId,
        (this.inFlightByCompany.get(row.companyId) ?? 0) + 1,
      );
      this.trackedRuns.set(row.id, { state: "active", companyId: row.companyId });
    }
    this.seeded = true;
    this.seededCount = rows.length;
    log.info(
      { seeded: rows.length, companies: this.inFlightByCompany.size },
      "scheduling gate seeded from running heartbeat_runs",
    );
    return rows.length;
  }

  /**
   * Resolved per-company config (defaults applied to absent keys). Cached
   * after the first successful read; setConfig refreshes it live. A read
   * failure falls back to the defaults for this call only — the defaults are
   * not cached, so a transient DB error cannot shadow the persisted config
   * until the next PATCH or restart — and the claim path never breaks
   * because of the gate.
   */
  async configFor(companyId: string): Promise<ResolvedSchedulingConfig> {
    const cached = this.configCache.get(companyId);
    if (cached) return cached;
    const db = this.db;
    if (!db) return { ...COMPANY_SCHEDULING_DEFAULTS };
    try {
      const [row] = await db
        .select({ scheduling: companies.scheduling })
        .from(companies)
        .where(eq(companies.id, companyId));
      const resolved = normalizeSchedulingConfig(row?.scheduling);
      this.configCache.set(companyId, resolved);
      return resolved;
    } catch (err) {
      log.warn(
        { err, companyId },
        "scheduling gate config read failed; falling back to defaults for this read",
      );
      return { ...COMPANY_SCHEDULING_DEFAULTS };
    }
  }

  /** Persist a config patch; the change applies live, no restart. */
  async setConfig(
    companyId: string,
    patch: Partial<{ enabled: boolean; maxInFlight: number }>,
  ): Promise<ResolvedSchedulingConfig> {
    const db = this.db;
    if (!db) throw new Error("scheduling gate is not bound to a database");
    const current = await this.configFor(companyId);
    const next: ResolvedSchedulingConfig = { ...current, ...patch };
    const [row] = await db
      .update(companies)
      .set({ scheduling: next, updatedAt: new Date() })
      .where(eq(companies.id, companyId))
      .returning({ id: companies.id });
    if (!row) throw new Error("company not found");
    this.configCache.set(companyId, next);
    log.info({ companyId, ...next }, "scheduling gate config updated");
    return next;
  }

  inFlightCount(companyId: string): number {
    return this.inFlightByCompany.get(companyId) ?? 0;
  }

  reservedCount(companyId: string): number {
    return this.reservedByCompany.get(companyId) ?? 0;
  }

  /**
   * Slots a new claim pass may take: `maxInFlight - inFlight - reserved`,
   * floored at zero. Disabled gate (or unbound gate defaults) reports no
   * limit. Await this before reserving so the config cache is warm.
   */
  async remainingSlots(companyId: string): Promise<number> {
    const config = await this.configFor(companyId);
    if (!config.enabled) return Number.POSITIVE_INFINITY;
    const used =
      (this.inFlightByCompany.get(companyId) ?? 0) +
      (this.reservedByCompany.get(companyId) ?? 0);
    return Math.max(0, config.maxInFlight - used);
  }

  /**
   * Synchronously reserve up to `requested` slots for one claim pass.
   * Single-threaded event loop: no torn reads between the remainingSlots
   * check and this reservation. Returns the number actually granted, which
   * may be smaller when the config changed between the two calls.
   */
  reserve(companyId: string, requested: number): number {
    const config = this.configCache.get(companyId) ?? { ...COMPANY_SCHEDULING_DEFAULTS };
    if (requested <= 0) return 0;
    if (!config.enabled) return requested;
    const used =
      (this.inFlightByCompany.get(companyId) ?? 0) +
      (this.reservedByCompany.get(companyId) ?? 0);
    const allowed = Math.min(requested, Math.max(0, config.maxInFlight - used));
    if (allowed > 0) {
      this.reservedByCompany.set(companyId, (this.reservedByCompany.get(companyId) ?? 0) + allowed);
    }
    return allowed;
  }

  /**
   * Release slots reserved by a claim pass that will not consume them
   * (reserved up front, fewer runs actually claimed).
   */
  release(companyId: string, count: number): void {
    if (count <= 0) return;
    const current = this.reservedByCompany.get(companyId) ?? 0;
    this.reservedByCompany.set(companyId, Math.max(0, current - count));
  }

  /** Record a claimed run as holding one of the pass's reserved slots. */
  markClaimedRun(runId: string, companyId: string): void {
    const existing = this.trackedRuns.get(runId);
    if (existing) {
      existing.companyId = companyId;
      return;
    }
    this.trackedRuns.set(runId, { state: "reserved", companyId });
  }

  /**
   * Release a claimed run's reservation. Only applies while the run is
   * still reserved (never reached the execution-start site). Idempotent.
   */
  releaseClaimedRun(runId: string): void {
    const entry = this.trackedRuns.get(runId);
    if (!entry || entry.state !== "reserved") return;
    this.reservedByCompany.set(
      entry.companyId,
      Math.max(0, (this.reservedByCompany.get(entry.companyId) ?? 0) - 1),
    );
    this.trackedRuns.delete(runId);
  }

  /**
   * Mirror of `activeRunExecutions.add`: the execution has started. A
   * reserved claim converts to in-flight here; a direct start (one that
   * bypassed the queue claim, e.g. a recovery re-dispatch) only adds to
   * in-flight. Already-active runs (a re-execution of the same run id) do
   * not double-count, matching the Set semantics of activeRunExecutions.
   */
  onExecutionStarted(runId: string, companyId: string): void {
    const entry = this.trackedRuns.get(runId);
    if (entry) {
      // A re-execution of a run that is already active (e.g. a native
      // session resume re-entering the executor) must not double-count,
      // matching the Set semantics of activeRunExecutions.
      if (entry.state === "active") return;
      if (entry.state === "reserved") {
        // The reservation converts to in-flight: the slot moves counters,
        // so the inFlight + reserved total is unchanged by the conversion.
        this.reservedByCompany.set(
          entry.companyId,
          Math.max(0, (this.reservedByCompany.get(entry.companyId) ?? 0) - 1),
        );
      }
      entry.state = "active";
      entry.companyId = companyId;
      this.inFlightByCompany.set(companyId, (this.inFlightByCompany.get(companyId) ?? 0) + 1);
      return;
    }
    this.trackedRuns.set(runId, { state: "active", companyId });
    this.inFlightByCompany.set(companyId, (this.inFlightByCompany.get(companyId) ?? 0) + 1);
  }

  /** Mirror of `activeRunExecutions.delete`: the executor's finally ran. */
  onExecutionFinished(runId: string): void {
    const entry = this.trackedRuns.get(runId);
    if (!entry) return;
    if (entry.state === "active") {
      this.inFlightByCompany.set(
        entry.companyId,
        Math.max(0, (this.inFlightByCompany.get(entry.companyId) ?? 0) - 1),
      );
    } else {
      this.reservedByCompany.set(
        entry.companyId,
        Math.max(0, (this.reservedByCompany.get(entry.companyId) ?? 0) - 1),
      );
    }
    this.trackedRuns.delete(runId);
  }

  /** Record the last gate decision for the board status view. */
  recordDecision(record: Omit<GateDecisionRecord, "at" | "hostSlotsRemaining"> & {
    companyId: string;
    at?: Date;
    hostSlotsRemaining: number | null;
  }): void {
    // A disabled gate reports Infinity remaining; store null so the status
    // JSON stays finite.
    this.lastDecisionByCompany.set(record.companyId, {
      at: record.at ?? new Date(),
      ...record,
      hostSlotsRemaining:
        record.hostSlotsRemaining === null || Number.isFinite(record.hostSlotsRemaining)
          ? record.hostSlotsRemaining
          : null,
    });
  }

  async status(companyId: string): Promise<SchedulingGateStatus> {
    const config = await this.configFor(companyId);
    const inFlight = this.inFlightCount(companyId);
    const reserved = this.reservedCount(companyId);
    let queuedByAgent: SchedulingGateStatus["queuedByAgent"] = [];
    if (this.db) {
      const queuedRows = await this.db
        .select({
          agentId: heartbeatRuns.agentId,
          agentName: agents.name,
          queued: sql<number>`count(*)`,
        })
        .from(heartbeatRuns)
        .innerJoin(agents, eq(heartbeatRuns.agentId, agents.id))
        .where(
          and(
            eq(heartbeatRuns.companyId, companyId),
            eq(heartbeatRuns.status, "queued"),
          ),
        )
        .groupBy(heartbeatRuns.agentId, agents.name);
      queuedByAgent = queuedRows
        .map((row) => ({
          agentId: row.agentId,
          agentName: row.agentName,
          queued: Number(row.queued),
        }))
        .sort((a, b) => b.queued - a.queued || a.agentId.localeCompare(b.agentId));
    }
    return {
      config,
      inFlight,
      reserved,
      maxInFlight: config.maxInFlight,
      slotsRemaining: config.enabled ? Math.max(0, config.maxInFlight - inFlight - reserved) : null,
      queuedByAgent,
      lastDecision: (() => {
        const decision = this.lastDecisionByCompany.get(companyId);
        return decision
          ? {
              at: decision.at.toISOString(),
              agentId: decision.agentId,
              runId: decision.runId,
              perAgentSlots: decision.perAgentSlots,
              hostSlotsRemaining: decision.hostSlotsRemaining,
              granted: decision.granted,
              plugin: decision.plugin,
              outcome: decision.outcome,
            }
          : null;
      })(),
    };
  }

  /**
   * Consult every plugin worker that advertises the `schedulingGate` method
   * (capability `scheduling.gate`). Fail-open on every failure mode: a
   * plugin absent from the manager, a worker that is not running, an RPC
   * error, a timeout beyond the explicit 2 s budget, or a malformed result
   * all leave the host count standing. Consultations run in parallel; the
   * tightest `slots` wins, any `defer` defers the whole claim.
   */
  async consultPlugins(
    params: SchedulingGateConsultParams,
  ): Promise<{
    grantedSlots: number;
    defer: boolean;
    plugin: SchedulingGatePluginDescriptor | null;
  }> {
    const manager = this.pluginWorkerManager;
    const hostSlots = Number.isFinite(params.hostSlotsRemaining)
      ? params.hostSlotsRemaining
      : Number.POSITIVE_INFINITY;
    if (!manager || typeof manager.listWorkers !== "function") {
      return { grantedSlots: hostSlots, defer: false, plugin: null };
    }
    const workers = manager
      .listWorkers()
      .filter((worker) => worker.status === "running" && worker.supportedMethods.includes("schedulingGate"));
    if (workers.length === 0) {
      return { grantedSlots: hostSlots, defer: false, plugin: null };
    }
    const outcomes = await Promise.all(
      workers.map(async (worker) => {
        try {
          const result = await worker.call("schedulingGate", params, SCHEDULING_GATE_CONSULT_TIMEOUT_MS);
          return { workerId: worker.pluginId, result, error: null as unknown };
        } catch (error) {
          log.debug(
            { err: error, pluginId: worker.pluginId, companyId: params.companyId },
            "schedulingGate consultation failed; host count stands",
          );
          return { workerId: worker.pluginId, result: null, error };
        }
      }),
    );
    let grantedSlots = hostSlots;
    let defer = false;
    let plugin: SchedulingGatePluginDescriptor | null = null;
    const deferring: SchedulingGatePluginDescriptor[] = [];
    for (const { workerId, result } of outcomes) {
      // A null result is a failed consultation (timeout, MethodNotFound,
      // worker down): fail open, the host count stands.
      if (result === null || typeof result !== "object") continue;
      const candidate = result as SchedulingGateConsultResult;
      const slots =
        typeof candidate.slots === "number" &&
        Number.isInteger(candidate.slots) &&
        candidate.slots >= 0
          ? Math.min(candidate.slots, hostSlots)
          : undefined;
      const wantsDefer = candidate.defer === true;
      if (slots === undefined && !wantsDefer) continue;
      const descriptor = {
        pluginId: workerId,
        ...(slots !== undefined ? { slots } : {}),
        ...(wantsDefer ? { defer: true } : {}),
        ...(typeof candidate.reason === "string" ? { reason: candidate.reason } : {}),
      };
      if (slots !== undefined && slots < grantedSlots) {
        grantedSlots = slots;
        plugin = descriptor;
      }
      if (wantsDefer) {
        defer = true;
        deferring.push(descriptor);
      }
    }
    if (defer && deferring.length > 0) {
      plugin = deferring[deferring.length - 1];
    }
    return { grantedSlots, defer, plugin };
  }
}

/**
 * The process-wide gate instance. heartbeat.ts mirrors execution
 * start/finish into it, queue.ts consults it at claim time, and the board
 * scheduling routes read/write it.
 */
export const schedulingGate = new SchedulingGate();

import {
  definePlugin,
  runWorker,
  type PluginApiRequestInput,
  type PluginContext,
  type PluginEvent,
  type PluginJobContext,
} from "@paperclipai/plugin-sdk";
import manifest, { PLUGIN_ID, RECONCILE_JOB_KEY } from "./manifest.js";
import {
  AGENTS_SQL,
  ISSUES_SQL,
  QUEUED_RUNS_SQL,
  RELATIONS_SQL,
  buildSnapshotFromRows,
  type AgentRow,
  type IssueRow,
  type QueuedRunRow,
  type RelationRow,
} from "./queries.js";
import {
  DEFAULT_GATE_CONFIG,
  decideGate,
  priorityRankFor,
  type CompanySnapshot,
} from "./types.js";
import {
  loadCompanyState,
  persistCompanyState,
  type CompanyState,
} from "./store.js";

/**
 * Fail-open staleness bound: a snapshot older than this is treated as cold
 * (no opinion) and triggers a background refresh. Deliberately equal to the
 * default age-out so a stale snapshot and an aged-out deferral both decay on
 * the same clock.
 */
const MAX_SNAPSHOT_AGE_MS = 5 * 60_000;

/**
 * How long the first post-restart consult may wait on durable-state
 * hydration before failing open. Comfortably inside the host's 2 s
 * consultation budget, leaving room for the decision itself.
 */
const HYDRATE_TIMEOUT_MS = 1_500;

interface CompanyRuntime {
  companyId: string;
  state: CompanyState;
  /** In-memory coalescing flag for background refreshes. */
  refreshing: boolean;
  /**
   * The in-flight refresh pass, shared by concurrent callers so a
   * `?refresh=1` status read awaits the pass already running instead of
   * observing the pre-refresh snapshot.
   */
  refreshInFlight: Promise<void> | undefined;
  /** True once the durable state has been loaded (or the load was attempted). */
  hydrated: boolean;
  /** In-flight hydration promise shared by concurrent callers. */
  hydrating: Promise<void> | undefined;
}

const runtimes = new Map<string, CompanyRuntime>();
let activeCtx: PluginContext | null = null;

/**
 * Test seam: clear module-level runtime state so a test starts from a cold
 * worker. Not used in production.
 */
export function __resetForTests(): void {
  runtimes.clear();
  activeCtx = null;
}

function requireCtx(): PluginContext {
  if (!activeCtx) throw new Error("global-wip-gate plugin has not been set up");
  return activeCtx;
}

function ensureRuntime(companyId: string): CompanyRuntime {
  let rt = runtimes.get(companyId);
  if (!rt) {
    rt = {
      companyId,
      state: { config: { ...DEFAULT_GATE_CONFIG }, snapshot: null, deferrals: {} },
      refreshing: false,
      refreshInFlight: undefined,
      hydrated: false,
      hydrating: undefined,
    };
    runtimes.set(companyId, rt);
  }
  return rt;
}

/**
 * Load durable state for a company into the in-memory runtime (idempotent,
 * coalesced). The persisted snapshot (if fresh enough) makes the first
 * post-restart consult decision-capable; otherwise consults fail open until
 * the first refresh lands.
 */
function hydrate(companyId: string): Promise<void> {
  const rt = ensureRuntime(companyId);
  if (rt.hydrated) return Promise.resolve();
  if (!rt.hydrating) {
    rt.hydrating = (async () => {
      try {
        const ctx = requireCtx();
        rt.state = await loadCompanyState(ctx, companyId);
        ctx.logger.debug(
          "global-wip-gate: hydrated durable state",
          {
            companyId,
            snapshotAgeMs: rt.state.snapshot
              ? Date.now() - rt.state.snapshot.refreshedAtMs
              : null,
            deferredRuns: Object.keys(rt.state.deferrals).length,
          },
        );
      } catch (err) {
        requireCtx().logger.warn(
          "global-wip-gate: state hydration failed; failing open until next refresh",
          { err: err instanceof Error ? err.message : String(err), companyId },
        );
      } finally {
        rt.hydrated = true;
        rt.hydrating = undefined;
      }
    })();
  }
  return rt.hydrating;
}

/** Await a promise but never longer than timeoutMs. */
function withTimeout(promise: Promise<void>, timeoutMs: number): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("timeout")), timeoutMs);
    promise.then(
      () => {
        clearTimeout(timer);
        resolve();
      },
      (err) => {
        clearTimeout(timer);
        reject(err);
      },
    );
  });
}

/**
 * Full snapshot refresh for one company: four whitelisted SELECTs, then
 * recompute + persist. Coalesced (one in flight per company — concurrent
 * callers await the running pass instead of observing the pre-refresh
 * snapshot, which is what makes `?refresh=1` a freshness barrier) and
 * failure-tolerant (a failed refresh keeps the previous snapshot).
 */
async function refreshCompany(companyId: string): Promise<void> {
  const rt = ensureRuntime(companyId);
  if (rt.refreshInFlight) {
    await rt.refreshInFlight;
    return;
  }
  rt.refreshing = true;
  const pass = (async () => {
    const ctx = requireCtx();
    try {
      const [queuedRuns, agents, issues, relations] = await Promise.all([
        ctx.db.query<QueuedRunRow>(QUEUED_RUNS_SQL, [companyId]),
        ctx.db.query<AgentRow>(AGENTS_SQL, [companyId]),
        ctx.db.query<IssueRow>(ISSUES_SQL, [companyId]),
        ctx.db.query<RelationRow>(RELATIONS_SQL, [companyId]),
      ]);
      const snapshot: CompanySnapshot = buildSnapshotFromRows({
        companyId,
        nowMs: Date.now(),
        queuedRuns,
        agents,
        issues,
        relations,
      });
      // Drop deferral bookkeeping for runs that are no longer queued.
      const queuedRunIds = new Set(
        Object.values(snapshot.agents).flatMap((agent) =>
          agent.queued.map((run) => run.runId),
        ),
      );
      rt.state = {
        ...rt.state,
        snapshot,
        deferrals: Object.fromEntries(
          Object.entries(rt.state.deferrals).filter(([runId]) => queuedRunIds.has(runId)),
        ),
      };
      ctx.logger.debug(
        "global-wip-gate: snapshot refreshed",
        {
          companyId,
          queuedRuns: queuedRunIds.size,
          agents: Object.keys(snapshot.agents).length,
          issues: Object.keys(snapshot.issues).length,
        },
      );
    } catch (err) {
      ctx.logger.warn(
        "global-wip-gate: snapshot refresh failed; keeping previous snapshot",
        { err: err instanceof Error ? err.message : String(err), companyId },
      );
      return;
    }
    // Durable persist is best-effort here: a scope-denied write (e.g. the
    // reconcile job for a company the plugin is not configured for yet) must
    // not mark the refresh failed — the in-memory snapshot already carries
    // the decision, and the next scoped consult or event persists it.
    try {
      await persistCompanyState(ctx, companyId, rt.state);
    } catch (err) {
      ctx.logger.debug(
        "global-wip-gate: durable snapshot persist skipped",
        { err: err instanceof Error ? err.message : String(err), companyId },
      );
    }
  })();
  rt.refreshInFlight = pass;
  try {
    await pass;
  } finally {
    rt.refreshing = false;
    rt.refreshInFlight = undefined;
  }
}

/** Fire-and-forget: hydrate (first time) and refresh, bounded by coalescing. */
function scheduleRefresh(companyId: string): void {
  void hydrate(companyId).then(() => refreshCompany(companyId));
}

const REFRESH_EVENTS = [
  "issue.created",
  "issue.updated",
  "issue.relations.updated",
  "agent.updated",
  "agent.status_changed",
  "agent.run.started",
  "agent.run.finished",
  "agent.run.failed",
  "agent.run.cancelled",
] as const;

function handleRefreshEvent(event: PluginEvent): Promise<void> {
  const companyId = event.companyId;
  if (typeof companyId !== "string" || companyId.length === 0) return Promise.resolve();
  scheduleRefresh(companyId);
  return Promise.resolve();
}

function statusBodyFor(rt: CompanyRuntime): Record<string, unknown> {
  const snapshot = rt.state.snapshot;
  const config = rt.state.config;
  if (!snapshot) {
    return {
      pluginId: PLUGIN_ID,
      enabled: config.enabled,
      snapshot: null,
      message: "snapshot not loaded yet; the first consults fail open until the first refresh lands",
      deferrals: Object.entries(rt.state.deferrals).map(([runId, record]) => ({
        runId,
        firstDeferredAtMs: record.firstDeferredAtMs,
        lastReason: record.lastReason,
      })),
      config,
    };
  }
  const ranked = Object.values(snapshot.agents)
    .flatMap((agent) =>
      agent.queued.map((run) => {
        const issue = run.issueId ? snapshot.issues[run.issueId] ?? null : null;
        const overrideKey = run.routineOriginId ?? run.issueId ?? "";
        const override = overrideKey ? config.routineOverrides[overrideKey] : undefined;
        const routine = run.routine && override !== "first";
        return {
          runId: run.runId,
          agentId: agent.agentId,
          agentName: agent.name,
          agentStatus: agent.status,
          issueId: run.issueId,
          invocationSource: run.invocationSource,
          triggerDetail: run.triggerDetail,
          createdAt: new Date(run.createdAtMs).toISOString(),
          tier: routine ? 1 : 0,
          routine,
          routineOverride: override ?? null,
          blockingImpact:
            Math.max(0, issue?.blockedCount ?? 0) *
            Math.max(0.01, config.weights[agent.agentId] ?? 1),
          priority: issue?.priority ?? null,
          priorityRank: priorityRankFor(issue?.priority),
        };
      }),
    )
    .sort(
      (a, b) =>
        a.tier - b.tier ||
        (b.blockingImpact as number) - (a.blockingImpact as number) ||
        (a.priorityRank as number) - (b.priorityRank as number) ||
        new Date(a.createdAt as string).getTime() - new Date(b.createdAt as string).getTime(),
    );
  return {
    pluginId: PLUGIN_ID,
    enabled: config.enabled,
    snapshot: {
      companyId: snapshot.companyId,
      refreshedAt: new Date(snapshot.refreshedAtMs).toISOString(),
      ageMs: Date.now() - snapshot.refreshedAtMs,
      agents: Object.values(snapshot.agents).map((agent) => ({
        agentId: agent.agentId,
        name: agent.name,
        status: agent.status,
        maxConcurrentRuns: agent.maxConcurrentRuns,
        running: agent.running,
        headroom: Math.max(0, agent.maxConcurrentRuns - agent.running),
        queued: agent.queued.map((run) => ({
          runId: run.runId,
          issueId: run.issueId,
          routine: run.routine,
          invocationSource: run.invocationSource,
          triggerDetail: run.triggerDetail,
          createdAt: new Date(run.createdAtMs).toISOString(),
        })),
      })),
      ranking: ranked,
    },
    deferrals: Object.entries(rt.state.deferrals).map(([runId, record]) => ({
      runId,
      firstDeferredAt: new Date(record.firstDeferredAtMs).toISOString(),
      ageMs: Date.now() - record.firstDeferredAtMs,
      ageOutAtMinutes: config.ageOutMinutes,
      lastReason: record.lastReason,
    })),
    config,
  };
}

const plugin = definePlugin({
  multiCompanyConfig: true,

  async setup(ctx) {
    activeCtx = ctx;
    ctx.logger.info(
      "global-wip-gate: worker started",
      { capabilities: manifest.capabilities },
    );
    for (const name of REFRESH_EVENTS) {
      ctx.events.on(name, handleRefreshEvent);
    }
    ctx.jobs.register(RECONCILE_JOB_KEY, async (job: PluginJobContext) => {
      // Safety net only: enforcement is host-side. Refresh every company we
      // have ever seen so a quiet company's snapshot cannot go stale.
      const companies = [...runtimes.keys()];
      await Promise.all(companies.map((companyId) => refreshCompany(companyId)));
      ctx.logger.debug(
        "global-wip-gate: reconcile sweep complete",
        { jobKey: job.jobKey, companies: companies.length },
      );
    });
  },

  async onConfigChanged(_newConfig, context) {
    // Policy lives in company-scoped plugin_state (edited via the config
    // API route), not in the host plugin config store. Acknowledge without
    // restarting so an operator config-save cannot bounce the worker mid-load.
    activeCtx?.logger.debug(
      "global-wip-gate: host config changed (no-op; policy is in plugin_state)",
      { companyId: context?.companyId ?? null },
    );
  },

  async onHealth() {
    const companies = [...runtimes.values()];
    return {
      status: "ok",
      message: `tracking ${companies.length} compan${companies.length === 1 ? "y" : "ies"}`,
      details: {
        companies: companies.map((rt) => ({
          companyId: rt.companyId,
          hydrated: rt.hydrated,
          refreshing: rt.refreshing,
          snapshotAgeMs: rt.state.snapshot ? Date.now() - rt.state.snapshot.refreshedAtMs : null,
          deferredRuns: Object.keys(rt.state.deferrals).length,
          enabled: rt.state.config.enabled,
        })),
      },
    };
  },

  async onSchedulingGate(params) {
    const ctx = requireCtx();
    const rt = ensureRuntime(params.companyId);
    if (!rt.hydrated) {
      // Bounded synchronous hydration: when a durable state exists, its
      // persisted snapshot (if fresh) makes even the first post-restart
      // consult decision-capable. If the hydration does not land within the
      // budget slice, the consult fails open (decideGate answers {} for a
      // null/stale snapshot) and the background pass finishes for the next
      // consult.
      const load = hydrate(params.companyId);
      void load.then(() => refreshCompany(params.companyId));
      try {
        await withTimeout(load, HYDRATE_TIMEOUT_MS);
      } catch {
        // Timeout or error: fall through to the fail-open decision.
      }
    }
    const nowMs = Date.now();
    const snapshot = rt.state.snapshot;
    const snapshotAgeMs = snapshot ? nowMs - snapshot.refreshedAtMs : null;
    const cold = !snapshot || (snapshotAgeMs ?? Infinity) > MAX_SNAPSHOT_AGE_MS;
    if (cold) {
      void refreshCompany(params.companyId);
    }
    const decision = decideGate({
      snapshot,
      config: rt.state.config,
      consult: {
        companyId: params.companyId,
        agentId: params.agentId,
        runId: params.run.runId,
        hostSlotsRemaining: params.hostSlotsRemaining,
      },
      deferrals: rt.state.deferrals,
      nowMs,
      maxSnapshotAgeMs: MAX_SNAPSHOT_AGE_MS,
    });
    let stateChanged = false;
    if (decision.deferred) {
      const existing = rt.state.deferrals[params.run.runId];
      rt.state.deferrals = {
        ...rt.state.deferrals,
        [params.run.runId]: {
          firstDeferredAtMs: existing?.firstDeferredAtMs ?? nowMs,
          lastReason: decision.result.reason ?? "",
        },
      };
      stateChanged = true;
    } else if (decision.clearDeferral || decision.runAgedOut) {
      if (rt.state.deferrals[params.run.runId]) {
        const next = { ...rt.state.deferrals };
        delete next[params.run.runId];
        rt.state.deferrals = next;
        stateChanged = true;
      }
    }
    if (stateChanged) {
      // Best-effort: the in-memory bookkeeping is authoritative for the
      // decision; a scope-denied write must not reject the consultation.
      void persistCompanyState(ctx, params.companyId, rt.state).catch((err) => {
        ctx.logger.debug(
          "global-wip-gate: durable deferral persist skipped",
          { err: err instanceof Error ? err.message : String(err), companyId: params.companyId },
        );
      });
    }
    if (decision.deferred || decision.runAgedOut || cold) {
      ctx.logger.info(
        decision.deferred
          ? "global-wip-gate: deferring claim"
          : decision.runAgedOut
            ? "global-wip-gate: deferral aged out; FIFO stands"
            : "global-wip-gate: snapshot cold; failing open",
        {
          companyId: params.companyId,
          agentId: params.agentId,
          runId: params.run.runId,
          hostSlotsRemaining: Number.isFinite(params.hostSlotsRemaining)
            ? params.hostSlotsRemaining
            : null,
          result: decision.result,
          snapshotCold: decision.snapshotCold,
        },
      );
    }
    return decision.result;
  },

  async onApiRequest(input: PluginApiRequestInput) {
    const ctx = requireCtx();
    const companyId = input.companyId;
    const body = (input.body ?? {}) as Record<string, unknown>;

    if (input.routeKey === "status") {
      const rt = ensureRuntime(companyId);
      if (!rt.hydrated) await hydrate(companyId);
      // `?refresh=1` forces a bounded synchronous refresh before the read so
      // a dashboard (or a caller using the status as a freshness barrier)
      // never observes a coalesced-away stale snapshot.
      if (input.query.refresh === "1" || !rt.state.snapshot) {
        await refreshCompany(companyId);
      }
      return { status: 200, body: statusBodyFor(rt) };
    }

    if (input.routeKey === "config" && input.method === "GET") {
      const rt = ensureRuntime(companyId);
      if (!rt.hydrated) await hydrate(companyId);
      return { status: 200, body: { pluginId: PLUGIN_ID, companyId, config: rt.state.config } };
    }

    if (input.routeKey === "config" && input.method === "PATCH") {
      const rt = ensureRuntime(companyId);
      if (!rt.hydrated) await hydrate(companyId);
      const next = { ...rt.state.config };
      if (body.enabled !== undefined) {
        if (typeof body.enabled !== "boolean") {
          return { status: 400, body: { error: "enabled must be a boolean" } };
        }
        next.enabled = body.enabled;
      }
      if (body.ageOutMinutes !== undefined) {
        if (
          typeof body.ageOutMinutes !== "number" ||
          !Number.isFinite(body.ageOutMinutes) ||
          body.ageOutMinutes < 1 ||
          body.ageOutMinutes > 1440
        ) {
          return { status: 400, body: { error: "ageOutMinutes must be a number between 1 and 1440" } };
        }
        next.ageOutMinutes = Math.floor(body.ageOutMinutes);
      }
      if (body.weights !== undefined) {
        if (!body.weights || typeof body.weights !== "object" || Array.isArray(body.weights)) {
          return { status: 400, body: { error: "weights must be an object of agentId -> number" } };
        }
        const weights: Record<string, number> = {};
        for (const [agentId, value] of Object.entries(body.weights as Record<string, unknown>)) {
          if (typeof value !== "number" || !Number.isFinite(value) || value <= 0 || value > 100) {
            return { status: 400, body: { error: `weights.${agentId} must be a number in (0, 100]` } };
          }
          weights[agentId] = value;
        }
        next.weights = weights;
      }
      if (body.routineOverrides !== undefined) {
        if (!body.routineOverrides || typeof body.routineOverrides !== "object" || Array.isArray(body.routineOverrides)) {
          return { status: 400, body: { error: "routineOverrides must be an object of routineOrIssueId -> 'first'|'last'" } };
        }
        const overrides: Record<string, "first" | "last"> = {};
        for (const [key, value] of Object.entries(body.routineOverrides as Record<string, unknown>)) {
          if (value !== "first" && value !== "last") {
            return { status: 400, body: { error: `routineOverrides.${key} must be 'first' or 'last'` } };
          }
          overrides[key] = value;
        }
        next.routineOverrides = overrides;
      }
      if (body.stickyPausedAgents !== undefined) {
        if (!Array.isArray(body.stickyPausedAgents) || body.stickyPausedAgents.some((v) => typeof v !== "string")) {
          return { status: 400, body: { error: "stickyPausedAgents must be an array of agent ids" } };
        }
        next.stickyPausedAgents = body.stickyPausedAgents as string[];
      }
      rt.state = { ...rt.state, config: next };
      await persistCompanyState(ctx, companyId, rt.state);
      scheduleRefresh(companyId);
      ctx.logger.info(
        "global-wip-gate: config updated",
        { companyId, config: next },
      );
      return { status: 200, body: { pluginId: PLUGIN_ID, companyId, config: next } };
    }

    if (input.routeKey === "agent-override" && input.method === "POST") {
      const rt = ensureRuntime(companyId);
      if (!rt.hydrated) await hydrate(companyId);
      const agentId = typeof body.agentId === "string" ? body.agentId : "";
      const action = typeof body.action === "string" ? body.action : "";
      if (!agentId) return { status: 400, body: { error: "agentId is required" } };
      if (action === "sticky-pause") {
        // The fallback lever: a manual pause is sticky across gate deferrals,
        // covering the comment/monitor-wake edge where wakes start outside
        // the queue. Recorded in config so the dashboard can show intent.
        await ctx.agents.pause(agentId, companyId);
        const sticky = new Set(rt.state.config.stickyPausedAgents);
        sticky.add(agentId);
        rt.state = { ...rt.state, config: { ...rt.state.config, stickyPausedAgents: [...sticky] } };
        await persistCompanyState(ctx, companyId, rt.state);
        scheduleRefresh(companyId);
        return {
          status: 200,
          body: { pluginId: PLUGIN_ID, companyId, agentId, action, sticky: true },
        };
      }
      if (action === "resume") {
        await ctx.agents.resume(agentId, companyId);
        const sticky = new Set(rt.state.config.stickyPausedAgents);
        sticky.delete(agentId);
        rt.state = { ...rt.state, config: { ...rt.state.config, stickyPausedAgents: [...sticky] } };
        await persistCompanyState(ctx, companyId, rt.state);
        scheduleRefresh(companyId);
        return {
          status: 200,
          body: { pluginId: PLUGIN_ID, companyId, agentId, action, sticky: false },
        };
      }
      return {
        status: 400,
        body: { error: "action must be 'sticky-pause' or 'resume'" },
      };
    }

    return { status: 404, body: { error: `Unknown global-wip-gate route: ${input.routeKey}` } };
  },
});

export default plugin;
runWorker(plugin, import.meta.url);

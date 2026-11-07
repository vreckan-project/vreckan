/**
 * Shared types for the global-wip-gate plugin.
 *
 * The plugin keeps a per-company snapshot of everything the ranking brain
 * needs (queued runs, agents, issue blocking impact) in memory, refreshes it
 * on domain events plus a one-minute safety-net job, and persists it to
 * company-scoped plugin_state so ranking survives worker restarts.
 */

/** Operator-tunable policy, persisted to company-scoped plugin_state. */
export interface GateConfig {
  /** Master switch. When false the plugin returns no opinion (host count stands). */
  enabled: boolean;
  /**
   * A run deferred longer than this stops being deferred: ordering reverts to
   * FIFO for it so the gate can never strand work. The cap is untouched.
   */
  ageOutMinutes: number;
  /**
   * Per-agent weight multipliers for blocking impact (e.g. { "<agentId>": 2 }
   * ranks that agent's blocking work above equally-blocking work elsewhere).
   * Agents without an entry weigh 1.
   */
  weights: Record<string, number>;
  /**
   * Routine-priority overrides keyed by routine id or routine-execution issue
   * id. "first" moves that routine's runs ahead of the normal tier; "last"
   * pins them to the routine tier even if they were otherwise first-class.
   */
  routineOverrides: Record<string, "first" | "last">;
  /**
   * Agents the operator manually paused as a sticky override (the
   * comment/monitor-wake edge: wakes on a deferred agent still start outside
   * the queue, so pausing between ticks is the fallback lever). Resume via
   * the dashboard route, not by editing this list by hand.
   */
  stickyPausedAgents: string[];
}

export const DEFAULT_GATE_CONFIG: GateConfig = {
  enabled: true,
  ageOutMinutes: 5,
  weights: {},
  routineOverrides: {},
  stickyPausedAgents: [],
};

/** Normalize an untrusted stored config onto safe defaults. */
export function normalizeGateConfig(value: unknown): GateConfig {
  const record =
    value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : {};
  // Fresh containers each call: the result must never alias the shared
  // DEFAULT_GATE_CONFIG records (mutations would leak across normalizations).
  const config: GateConfig = {
    enabled: DEFAULT_GATE_CONFIG.enabled,
    ageOutMinutes: DEFAULT_GATE_CONFIG.ageOutMinutes,
    weights: {},
    routineOverrides: {},
    stickyPausedAgents: [],
  };
  if (typeof record.enabled === "boolean") config.enabled = record.enabled;
  if (typeof record.ageOutMinutes === "number" && Number.isFinite(record.ageOutMinutes)) {
    config.ageOutMinutes = Math.min(1440, Math.max(1, Math.floor(record.ageOutMinutes)));
  }
  if (record.weights && typeof record.weights === "object" && !Array.isArray(record.weights)) {
    for (const [agentId, weight] of Object.entries(record.weights as Record<string, unknown>)) {
      if (typeof weight === "number" && Number.isFinite(weight) && weight > 0) {
        config.weights[agentId] = Math.min(100, weight);
      }
    }
  }
  if (record.routineOverrides && typeof record.routineOverrides === "object" && !Array.isArray(record.routineOverrides)) {
    for (const [key, mode] of Object.entries(record.routineOverrides as Record<string, unknown>)) {
      if (mode === "first" || mode === "last") config.routineOverrides[key] = mode;
    }
  }
  if (Array.isArray(record.stickyPausedAgents)) {
    config.stickyPausedAgents = record.stickyPausedAgents.filter(
      (agentId): agentId is string => typeof agentId === "string" && agentId.length > 0,
    );
  }
  return config;
}

/** One queued run the gate may rank. */
export interface QueuedRunInfo {
  runId: string;
  agentId: string;
  /** Issue the run was woken for (contextSnapshot.issueId or heartbeat_runs.issue_id). */
  issueId: string | null;
  /**
   * Routine origin id when the run's issue is a routine execution
   * (issues.origin_kind = 'routine_execution' / plugin operation variants),
   * else null. Used for routine-tier classification and overrides.
   */
  routineOriginId: string | null;
  /** True when the run belongs to the routine tier (unless overridden first). */
  routine: boolean;
  createdAtMs: number;
  invocationSource: string | null;
  triggerDetail: string | null;
}

/** An agent as the gate sees it. */
export interface AgentInfo {
  agentId: string;
  name: string;
  status: string;
  reportsTo: string | null;
  /** Host per-agent concurrent-run cap (mirrors the heartbeat policy default). */
  maxConcurrentRuns: number;
  /** Runs currently in flight for this agent. */
  running: number;
  /** This agent's queued runs, oldest first (FIFO). */
  queued: QueuedRunInfo[];
}

/** One issue as the gate sees it. */
export interface IssueInfo {
  priority: string;
  originKind: string;
  originId: string | null;
  /**
   * Transitive blocking impact: how many company issues this issue
   * transitively blocks (issue_relations type='blocks', A.issue_id blocks
   * A.related_issue_id). Cached — recomputed on relation changes.
   */
  blockedCount: number;
}

/**
 * The per-company snapshot the ranking brain consumes. Pure data: every
 * field is JSON-serializable so it can round-trip through plugin_state.
 */
export interface CompanySnapshot {
  companyId: string;
  /** Epoch ms of the last successful refresh. */
  refreshedAtMs: number;
  agents: Record<string, AgentInfo>;
  issues: Record<string, IssueInfo>;
}

/** Deferral bookkeeping for one run (age-out fail-open). */
export interface DeferralRecord {
  firstDeferredAtMs: number;
  lastReason: string;
}

/** Ranked queued run with the brain's comparable fields materialized. */
export interface RankedRun {
  run: QueuedRunInfo;
  agent: AgentInfo;
  /** 0 = normal tier (or a routine overridden first), 1 = routine tier (last). */
  tier: 0 | 1;
  /** Transitive blocked count × the agent's weight. */
  blockingImpact: number;
  /** 0=critical, 1=high, 2=medium, 3=low, 4=no issue (ranks below low). */
  priorityRank: number;
  createdAtMs: number;
}

/** What the gate is told about one consultation. */
export interface GateConsultInfo {
  companyId: string;
  agentId: string;
  runId: string;
  /**
   * Remaining company-wide host slots, or null when the host gate is
   * disabled (Infinity does not survive the JSON wire). null = no host cap.
   */
  hostSlotsRemaining: number | null;
}

export interface GateDecision {
  /** The response to send back to the host (empty object = no opinion). */
  result: { slots?: number; defer?: boolean; reason?: string };
  /** True when the snapshot was cold/stale: the caller refreshes in the background. */
  snapshotCold: boolean;
  /** True when the consulted run just aged out: the caller clears its deferral record. */
  runAgedOut: boolean;
  /** Set when this consultation deferred the run (the caller records firstDeferredAt). */
  deferred: boolean;
  /** True when the run stopped being deferred this consultation: clear its deferral record so the age-out clock restarts on the next deferral burst. */
  clearDeferral: boolean;
}

/** Priority ranks: critical first. Unknown priorities rank after low. */
const PRIORITY_RANKS: Record<string, number> = {
  critical: 0,
  high: 1,
  medium: 2,
  low: 3,
};

export function priorityRankFor(priority: string | null | undefined): number {
  if (!priority) return 4;
  return PRIORITY_RANKS[priority] ?? 4;
}

/**
 * Classify the routine tier for a run. Routine execution issues carry
 * origin_kind 'routine_execution' (or a plugin operation variant for
 * plugin-managed routines). An explicit "first" override lifts the run into
 * the normal tier; "last" pins it to the routine tier.
 */
export function routineTierFor(
  run: Pick<QueuedRunInfo, "issueId" | "routineOriginId">,
  issue: IssueInfo | null,
  config: GateConfig,
): boolean {
  const routine =
    run.routineOriginId !== null || issue?.originKind === "routine_execution" ||
    (issue?.originKind?.startsWith("plugin:") ?? false);
  if (!routine) return false;
  const overrideKey = run.routineOriginId ?? run.issueId ?? "";
  const override = overrideKey ? config.routineOverrides[overrideKey] : undefined;
  if (override === "first") return false;
  return true;
}

/**
 * Compare two ranked runs: routines last (unless overridden), then
 * blocking impact (descending), then issue priority (ascending), then FIFO.
 */
export function compareRankedRuns(left: RankedRun, right: RankedRun): number {
  if (left.tier !== right.tier) return left.tier - right.tier;
  if (left.blockingImpact !== right.blockingImpact) {
    return right.blockingImpact - left.blockingImpact;
  }
  if (left.priorityRank !== right.priorityRank) return left.priorityRank - right.priorityRank;
  if (left.createdAtMs !== right.createdAtMs) return left.createdAtMs - right.createdAtMs;
  return left.run.runId.localeCompare(right.run.runId);
}

/**
 * The pure gate decision. Given the cached ranking and the consultation,
 * answer whether this agent's claim should be deferred so a scarce slot
 * lands with a higher-ranked claimable agent.
 *
 * Fail-open rules baked in:
 * - disabled plugin → no opinion
 * - cold/stale snapshot → no opinion (caller refreshes in the background)
 * - consulted agent gone or not claimable → no opinion (the host won't claim anyway)
 * - host gate disabled (infinite slots) → no opinion (nothing scarce to route)
 * - the consulted run already aged out of deferral → no opinion for it (FIFO stands)
 */
export function decideGate(input: {
  snapshot: CompanySnapshot | null;
  config: GateConfig;
  consult: GateConsultInfo;
  deferrals: Record<string, DeferralRecord>;
  nowMs: number;
  /** Max snapshot age in ms before a snapshot counts as cold (fail-open). */
  maxSnapshotAgeMs: number;
}): GateDecision {
  const { snapshot, config, consult, deferrals, nowMs, maxSnapshotAgeMs } = input;
  const noOpinion = (
    snapshotCold: boolean,
    runAgedOut = false,
    clearDeferral = false,
  ): GateDecision => ({
    result: {},
    snapshotCold,
    runAgedOut,
    deferred: false,
    clearDeferral,
  });

  if (!config.enabled) return noOpinion(false);
  if (!snapshot) return noOpinion(true);
  const ageMs = nowMs - snapshot.refreshedAtMs;
  if (ageMs > maxSnapshotAgeMs) return noOpinion(true);

  const self = snapshot.agents[consult.agentId];
  if (!self) return noOpinion(false);

  const ranked: RankedRun[] = [];
  for (const agent of Object.values(snapshot.agents)) {
    for (const run of agent.queued) {
      const issue = run.issueId ? snapshot.issues[run.issueId] ?? null : null;
      ranked.push({
        run,
        agent,
        tier: routineTierFor(run, issue, config) ? 1 : 0,
        blockingImpact:
          Math.max(0, issue?.blockedCount ?? 0) *
          Math.max(0.01, config.weights[agent.agentId] ?? 1),
        priorityRank: priorityRankFor(issue?.priority),
        createdAtMs: run.createdAtMs,
      });
    }
  }
  ranked.sort(compareRankedRuns);

  const selfTop = ranked.find((entry) => entry.run.runId === consult.runId);
  if (!selfTop) {
    // The host told us about a run the snapshot does not know: the snapshot
    // is behind reality. Fail open and let the background refresh catch up.
    return noOpinion(true);
  }
  if (!isAgentClaimable(self, snapshot.agents)) return noOpinion(false);
  // null = disabled host gate (Infinity does not survive the JSON wire),
  // non-finite = malformed. Either way there is nothing scarce to route.
  if (
    typeof consult.hostSlotsRemaining !== "number" ||
    !Number.isFinite(consult.hostSlotsRemaining)
  ) {
    return noOpinion(false);
  }
  const hostSlotsRemaining = consult.hostSlotsRemaining;

  const prior = deferrals[consult.runId];
  let agedOut = false;
  if (prior && nowMs - prior.firstDeferredAtMs >= config.ageOutMinutes * 60_000) {
    agedOut = true;
  }
  if (agedOut) return noOpinion(false, true, true);

  // Superior runs: everything ranked before the consulted top run that sits
  // with a different, claimable agent that still has per-agent headroom.
  // Superior runs of the consulted agent's own queue are irrelevant — the
  // same claim pass already claims them right after the top run.
  let superiorCount = 0;
  for (let i = 0; i < ranked.length; i += 1) {
    const entry = ranked[i]!;
    if (entry.run.runId === consult.runId) break;
    if (entry.agent.agentId === self.agentId) continue;
    if (!isAgentClaimable(entry.agent, snapshot.agents)) continue;
    if (entry.agent.running >= entry.agent.maxConcurrentRuns) continue;
    superiorCount += 1;
  }

  // Scarcity test: only route a slot away when the superior runs would take
  // it. No superiors, or more slots than superiors — everyone fits, no
  // deferral. A run that stops being deferred resets its age-out clock so a
  // later deferral burst is judged fresh.
  if (superiorCount === 0 || hostSlotsRemaining > superiorCount) {
    return noOpinion(false, false, prior !== undefined);
  }

  return {
    result: {
      defer: true,
      reason: `global-wip-gate: ${superiorCount} higher-ranked queued run(s) on claimable agent(s) would take the remaining ${hostSlotsRemaining} slot(s); letting the slot land with them`,
    },
    snapshotCold: false,
    runAgedOut: false,
    deferred: true,
    clearDeferral: false,
  };
}

/**
 * Claimability mirror of the host's `evaluateAgentInvokability`: an agent is
 * claimable when its status is invokable and its reporting chain is healthy.
 * Kept deliberately dependency-free (no server imports): the same rules as
 * @paperclipai/shared's getAgentWorkEligibility, inlined for the bundle.
 */
export function isAgentClaimable(
  agent: AgentInfo,
  allAgents: Record<string, AgentInfo>,
): boolean {
  if (!isInvokableStatus(agent.status)) return false;
  const byId = new Map<string, AgentInfo>(
    Object.values(allAgents).map((entry) => [entry.agentId, entry]),
  );
  const seen = new Set<string>();
  let cursor: AgentInfo | undefined = agent;
  let depth = 0;
  while (cursor) {
    if (seen.has(cursor.agentId)) return false;
    seen.add(cursor.agentId);
    if (depth >= 32) return false;
    depth += 1;
    if (!cursor.reportsTo) return true;
    const manager = byId.get(cursor.reportsTo);
    if (!manager) return false;
    if (manager.status === "terminated") return false;
    cursor = manager;
  }
  return true;
}

const INVOKABLE_STATUSES = new Set(["active", "idle", "running", "error"]);

export function isInvokableStatus(status: string): boolean {
  return INVOKABLE_STATUSES.has(status);
}

/** Mirror of the host heartbeat policy's per-agent cap normalization. */
export function maxConcurrentRunsFor(
  runtimeConfig: Record<string, unknown> | null | undefined,
  defaultMax: number,
): number {
  const heartbeat =
    runtimeConfig && typeof runtimeConfig === "object"
      ? (runtimeConfig as Record<string, unknown>).heartbeat
      : undefined;
  const value =
    heartbeat && typeof heartbeat === "object"
      ? (heartbeat as Record<string, unknown>).maxConcurrentRuns
      : undefined;
  const parsed =
    typeof value === "number" && Number.isFinite(value) ? Math.floor(value) : defaultMax;
  return Math.min(50, Math.max(1, parsed));
}

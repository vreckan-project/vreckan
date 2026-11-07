import { describe, expect, it } from "vitest";
import {
  compareRankedRuns,
  decideGate,
  isAgentClaimable,
  maxConcurrentRunsFor,
  normalizeGateConfig,
  priorityRankFor,
  routineTierFor,
  type AgentInfo,
  type CompanySnapshot,
  type GateConfig,
  type IssueInfo,
  type QueuedRunInfo,
  type RankedRun,
  type DeferralRecord,
  type GateConsultInfo,
} from "../src/types.js";
import { buildSnapshotFromRows, transitiveBlockedCounts } from "../src/queries.js";

const COMPANY = "11111111-1111-4111-8111-111111111111";
const NOW = Date.parse("2026-11-07T09:00:00.000Z");

function agent(
  agentId: string,
  overrides: Partial<AgentInfo> = {},
): AgentInfo {
  return {
    agentId,
    name: `agent-${agentId.slice(0, 8)}`,
    status: "idle",
    reportsTo: null,
    maxConcurrentRuns: 2,
    running: 0,
    queued: [],
    ...overrides,
  };
}

function run(
  agentId: string,
  runId: string,
  overrides: Partial<QueuedRunInfo> = {},
): QueuedRunInfo {
  return {
    runId,
    agentId,
    issueId: null,
    routineOriginId: null,
    routine: false,
    createdAtMs: NOW,
    invocationSource: "on_demand",
    triggerDetail: "manual",
    ...overrides,
  };
}

function issue(overrides: Partial<IssueInfo> = {}): IssueInfo {
  return {
    priority: "medium",
    originKind: "manual",
    originId: null,
    blockedCount: 0,
    ...overrides,
  };
}

function snapshotOf(agents: AgentInfo[], issues: Record<string, IssueInfo> = {}): CompanySnapshot {
  return { companyId: COMPANY, refreshedAtMs: NOW, agents: Object.fromEntries(agents.map((a) => [a.agentId, a])), issues };
}

const CONFIG: GateConfig = {
  enabled: true,
  ageOutMinutes: 5,
  weights: {},
  routineOverrides: {},
  stickyPausedAgents: [],
};

function ranked(agent: AgentInfo, run: QueuedRunInfo, issues: Record<string, IssueInfo>, config: GateConfig): RankedRun {
  const issueInfo = run.issueId ? issues[run.issueId] ?? null : null;
  return {
    run,
    agent,
    tier: routineTierFor(run, issueInfo, config) ? 1 : 0,
    blockingImpact:
      Math.max(0, issueInfo?.blockedCount ?? 0) *
      Math.max(0.01, config.weights[agent.agentId] ?? 1),
    priorityRank: priorityRankFor(issueInfo?.priority),
    createdAtMs: run.createdAtMs,
  };
}

function consult(
  runId: string,
  agentId: string,
  hostSlotsRemaining: number | null,
): GateConsultInfo {
  return { companyId: COMPANY, agentId, runId, hostSlotsRemaining };
}

describe("priorityRankFor", () => {
  it("ranks critical < high < medium < low < no-issue", () => {
    expect(priorityRankFor("critical")).toBe(0);
    expect(priorityRankFor("high")).toBe(1);
    expect(priorityRankFor("medium")).toBe(2);
    expect(priorityRankFor("low")).toBe(3);
    expect(priorityRankFor(null)).toBe(4);
    expect(priorityRankFor("bogus")).toBe(4);
  });
});

describe("routineTierFor", () => {
  it("puts routine_execution issues in the routine tier", () => {
    const r = run("a", "r1", { issueId: "i1", routineOriginId: "rt1" });
    expect(routineTierFor(r, issue({ originKind: "routine_execution", originId: "rt1" }), CONFIG)).toBe(true);
  });

  it("puts plugin-operation routines in the routine tier", () => {
    const r = run("a", "r1", { issueId: "i1", routineOriginId: "op1" });
    expect(routineTierFor(r, issue({ originKind: "plugin:acme:operation:x" }), CONFIG)).toBe(true);
  });

  it("leaves manual issues out of the routine tier", () => {
    const r = run("a", "r1", { issueId: "i1" });
    expect(routineTierFor(r, issue(), CONFIG)).toBe(false);
    expect(routineTierFor(r, null, CONFIG)).toBe(false);
  });

  it("honors a 'first' override keyed by routine id", () => {
    const r = run("a", "r1", { issueId: "i1", routineOriginId: "rt1" });
    const config: GateConfig = { ...CONFIG, routineOverrides: { rt1: "first" } };
    expect(routineTierFor(r, issue({ originKind: "routine_execution", originId: "rt1" }), config)).toBe(false);
  });

  it("honors a 'first' override keyed by issue id when no routine id is known", () => {
    const r = run("a", "r1", { issueId: "i1" });
    const config: GateConfig = { ...CONFIG, routineOverrides: { i1: "first" } };
    expect(routineTierFor(r, issue({ originKind: "routine_execution", originId: "rt-missing" }), config)).toBe(false);
  });

  it("pins a 'last' override to the routine tier", () => {
    const r = run("a", "r1", { issueId: "i1", routineOriginId: "rt1" });
    const config: GateConfig = { ...CONFIG, routineOverrides: { rt1: "last" } };
    expect(routineTierFor(r, issue({ originKind: "routine_execution", originId: "rt1" }), config)).toBe(true);
  });
});

describe("compareRankedRuns", () => {
  it("orders routines last by default", () => {
    const a = agent("a");
    const routineRun = ranked(a, run("a", "r-routine", { createdAtMs: NOW - 100 }), {}, CONFIG);
    const normalRun = ranked(a, run("a", "r-normal"), {}, CONFIG);
    // The routine run is created earlier, but must still sort last.
    routineRun.tier = 1;
    normalRun.tier = 0;
    expect(compareRankedRuns(normalRun, routineRun)).toBeLessThan(0);
  });

  it("orders blocking impact above priority", () => {
    const a = agent("a");
    const bigBlocker = ranked(a, run("a", "r1", { issueId: "i1" }), { i1: issue({ priority: "low", blockedCount: 4 }) }, CONFIG);
    const critical = ranked(a, run("a", "r2", { issueId: "i2" }), { i2: issue({ priority: "critical", blockedCount: 0 }) }, CONFIG);
    expect(compareRankedRuns(bigBlocker, critical)).toBeLessThan(0);
  });

  it("multiplies blocking impact by the agent weight", () => {
    const a = agent("a");
    const b = agent("b");
    const config = { ...CONFIG, weights: { b: 3 } };
    const aRun = ranked(a, run("a", "r1", { issueId: "i1" }), { i1: issue({ blockedCount: 4 }) }, config);
    const bRun = ranked(b, run("b", "r2", { issueId: "i2" }), { i2: issue({ blockedCount: 2 }) }, config);
    // b: 2 * 3 = 6 > a: 4 * 1
    expect(compareRankedRuns(bRun, aRun)).toBeLessThan(0);
  });

  it("falls back to priority, then FIFO", () => {
    const a = agent("a");
    const high = ranked(a, run("a", "r-high", { issueId: "i1", createdAtMs: NOW + 10 }), { i1: issue({ priority: "high" }) }, CONFIG);
    const medium = ranked(a, run("a", "r-medium", { issueId: "i2", createdAtMs: NOW }), { i2: issue({ priority: "medium" }) }, CONFIG);
    expect(compareRankedRuns(high, medium)).toBeLessThan(0);

    const mediumEarly = ranked(a, run("a", "r-m1", { issueId: "i3", createdAtMs: NOW }), { i3: issue({ priority: "medium" }) }, CONFIG);
    const mediumLate = ranked(a, run("a", "r-m2", { issueId: "i4", createdAtMs: NOW + 5 }), { i4: issue({ priority: "medium" }) }, CONFIG);
    expect(compareRankedRuns(mediumEarly, mediumLate)).toBeLessThan(0);
  });
});

describe("isAgentClaimable", () => {
  it("accepts healthy active/idle/running/error agents", () => {
    for (const status of ["active", "idle", "running", "error"]) {
      const a = agent("a", { status });
      expect(isAgentClaimable(a, { a })).toBe(true);
    }
  });

  it("rejects paused, terminated, pending_approval, and unknown statuses", () => {
    for (const status of ["paused", "terminated", "pending_approval", "weird"]) {
      const a = agent("a", { status });
      expect(isAgentClaimable(a, { a })).toBe(false);
    }
  });

  it("rejects agents whose manager is terminated", () => {
    const boss = agent("boss", { status: "terminated" });
    const worker = agent("a", { reportsTo: "boss" });
    expect(isAgentClaimable(worker, { boss, worker })).toBe(false);
  });

  it("rejects agents with a missing manager", () => {
    const a = agent("a", { reportsTo: "ghost" });
    expect(isAgentClaimable(a, { a })).toBe(false);
  });

  it("rejects reporting cycles", () => {
    const a = agent("a", { reportsTo: "b" });
    const b = agent("b", { reportsTo: "a" });
    expect(isAgentClaimable(a, { a, b })).toBe(false);
    expect(isAgentClaimable(b, { a, b })).toBe(false);
  });

  it("keeps an agent invokable when the manager is only paused", () => {
    const boss = agent("boss", { status: "paused" });
    const worker = agent("a", { reportsTo: "boss" });
    expect(isAgentClaimable(worker, { boss, worker })).toBe(true);
  });
});

describe("maxConcurrentRunsFor", () => {
  it("mirrors the host heartbeat policy normalization", () => {
    expect(maxConcurrentRunsFor(null, 2)).toBe(2);
    expect(maxConcurrentRunsFor({ heartbeat: {} }, 2)).toBe(2);
    expect(maxConcurrentRunsFor({ heartbeat: { maxConcurrentRuns: 3 } }, 2)).toBe(3);
    expect(maxConcurrentRunsFor({ heartbeat: { maxConcurrentRuns: 0.5 } }, 2)).toBe(1);
    expect(maxConcurrentRunsFor({ heartbeat: { maxConcurrentRuns: 51 } }, 2)).toBe(50);
    expect(maxConcurrentRunsFor({ heartbeat: { maxConcurrentRuns: "3" } }, 2)).toBe(2);
  });
});

describe("normalizeGateConfig", () => {
  it("keeps sane values", () => {
    const config = normalizeGateConfig({
      enabled: false,
      ageOutMinutes: 12,
      weights: { a: 2.5 },
      routineOverrides: { r1: "first" },
      stickyPausedAgents: ["a", "b"],
    });
    expect(config).toEqual({
      enabled: false,
      ageOutMinutes: 12,
      weights: { a: 2.5 },
      routineOverrides: { r1: "first" },
      stickyPausedAgents: ["a", "b"],
    });
  });

  it("clamps and drops garbage", () => {
    const config = normalizeGateConfig({
      enabled: "yes",
      ageOutMinutes: 9999,
      weights: { a: -1, b: 3, c: "x" },
      routineOverrides: { r1: "middle" },
      stickyPausedAgents: ["a", 42],
    });
    expect(config.enabled).toBe(true);
    expect(config.ageOutMinutes).toBe(1440);
    expect(config.weights).toEqual({ b: 3 });
    expect(config.routineOverrides).toEqual({});
    expect(config.stickyPausedAgents).toEqual(["a"]);
  });

  it("returns defaults for non-objects", () => {
    expect(normalizeGateConfig(null)).toEqual(CONFIG);
    expect(normalizeGateConfig("x")).toEqual(CONFIG);
    expect(normalizeGateConfig([1])).toEqual(CONFIG);
  });
});

describe("transitiveBlockedCounts", () => {
  it("counts transitive blocks once per reachable issue", () => {
    // A blocks B and C; B blocks D and C (diamond) → A blocks {B, C, D} = 3
    const counts = transitiveBlockedCounts(
      ["A", "B"],
      [
        { blocker_id: "A", blocked_id: "B" },
        { blocker_id: "A", blocked_id: "C" },
        { blocker_id: "B", blocked_id: "C" },
        { blocker_id: "B", blocked_id: "D" },
      ],
    );
    expect(counts.get("A")).toBe(3);
    expect(counts.get("B")).toBe(2);
  });

  it("survives cycles", () => {
    const counts = transitiveBlockedCounts(
      ["A", "B"],
      [
        { blocker_id: "A", blocked_id: "B" },
        { blocker_id: "B", blocked_id: "A" },
      ],
    );
    expect(counts.get("A")).toBe(1);
    expect(counts.get("B")).toBe(1);
  });

  it("is zero for issues that block nothing", () => {
    const counts = transitiveBlockedCounts(["A"], []);
    expect(counts.get("A")).toBe(0);
  });
});

describe("decideGate", () => {
  function scenario(opts: {
    selfRuns?: QueuedRunInfo[];
    otherAgents?: AgentInfo[];
    issues?: Record<string, IssueInfo>;
    config?: GateConfig;
    hostSlotsRemaining?: number | null;
    deferrals?: Record<string, DeferralRecord>;
    nowMs?: number;
    topRunId?: string;
  }) {
    const self = agent("self", { queued: opts.selfRuns ?? [] });
    const others = opts.otherAgents ?? [];
    const snapshot = snapshotOf([self, ...others], opts.issues ?? {});
    const topRunId = opts.topRunId ?? self.queued[0]?.runId ?? "missing";
    const hostSlotsRemaining: number | null =
      opts.hostSlotsRemaining === undefined ? 1 : opts.hostSlotsRemaining;
    return decideGate({
      snapshot,
      config: opts.config ?? CONFIG,
      consult: consult(topRunId, "self", hostSlotsRemaining),
      deferrals: opts.deferrals ?? {},
      nowMs: opts.nowMs ?? NOW,
      maxSnapshotAgeMs: 5 * 60_000,
    });
  }

  it("defers when a higher-ranked run on a claimable agent would take the scarce slot", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self", { issueId: "is", createdAtMs: NOW })],
      otherAgents: [
        agent("other", {
          queued: [run("other", "r-other", { issueId: "io", createdAtMs: NOW - 10 })],
        }),
      ],
      issues: {
        // other's run blocks 3 issues; self's none → other ranks first
        is: issue({ priority: "high", blockedCount: 0 }),
        io: issue({ priority: "low", blockedCount: 3 }),
      },
      hostSlotsRemaining: 1,
    });
    expect(decision.deferred).toBe(true);
    expect(decision.result.defer).toBe(true);
    expect(decision.result.reason).toMatch(/higher-ranked/);
  });

  it("does not defer when slots are not scarce", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self", { issueId: "is", createdAtMs: NOW })],
      otherAgents: [
        agent("other", {
          queued: [run("other", "r-other", { issueId: "io", createdAtMs: NOW - 10 })],
        }),
      ],
      issues: {
        is: issue({ priority: "high", blockedCount: 0 }),
        io: issue({ priority: "low", blockedCount: 3 }),
      },
      hostSlotsRemaining: 5,
    });
    expect(decision.result).toEqual({});
    expect(decision.deferred).toBe(false);
  });

  it("does not count superior runs on paused agents", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self")],
      otherAgents: [
        agent("other", {
          status: "paused",
          queued: [run("other", "r-other", { issueId: "io", createdAtMs: NOW - 10 })],
        }),
      ],
      issues: { io: issue({ blockedCount: 3 }) },
      hostSlotsRemaining: 1,
    });
    expect(decision.result).toEqual({});
  });

  it("does not count superior runs on agents with no per-agent headroom", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self")],
      otherAgents: [
        agent("other", {
          maxConcurrentRuns: 1,
          running: 1,
          queued: [run("other", "r-other", { issueId: "io", createdAtMs: NOW - 10 })],
        }),
      ],
      issues: { io: issue({ blockedCount: 3 }) },
      hostSlotsRemaining: 1,
    });
    expect(decision.result).toEqual({});
  });

  it("does not count superior runs in its own queue (the same pass claims them)", () => {
    const decision = scenario({
      selfRuns: [
        run("self", "r-self-top", { issueId: "i1", createdAtMs: NOW }),
        run("self", "r-self-lower", { issueId: "i2", createdAtMs: NOW + 10 }),
      ],
      issues: {
        i1: issue({ priority: "low", blockedCount: 0 }),
        i2: issue({ priority: "critical", blockedCount: 5 }),
      },
      hostSlotsRemaining: 1,
    });
    expect(decision.result).toEqual({});
  });

  it("defers routines ahead of normal work when slots are scarce", () => {
    const decision = scenario({
      selfRuns: [
        run("self", "r-normal", { issueId: "in", createdAtMs: NOW }),
      ],
      otherAgents: [
        agent("other", {
          queued: [
            run("other", "r-routine", {
              issueId: "ir",
              routineOriginId: "rt1",
              routine: true,
              createdAtMs: NOW - 100,
            }),
          ],
        }),
      ],
      issues: {
        in: issue({ priority: "high", blockedCount: 0 }),
        ir: issue({ originKind: "routine_execution", originId: "rt1", priority: "medium", blockedCount: 0 }),
      },
      hostSlotsRemaining: 1,
    });
    // Normal work ranks ahead of routine work → self is not deferred.
    expect(decision.result).toEqual({});
  });

  it("defers normal work in favor of a routine overridden to 'first'", () => {
    const common = {
      selfRuns: [run("self", "r-normal", { issueId: "in", createdAtMs: NOW + 10 })],
      otherAgents: [
        agent("other", {
          queued: [
            run("other", "r-routine", {
              issueId: "ir",
              routineOriginId: "rt1",
              routine: true,
              createdAtMs: NOW,
            }),
          ],
        }),
      ],
      issues: {
        // Equal priority and impact: only the tier (and then FIFO) differ.
        in: issue({ priority: "medium", blockedCount: 0 }),
        ir: issue({ originKind: "routine_execution", originId: "rt1", priority: "medium", blockedCount: 0 }),
      },
      hostSlotsRemaining: 1,
    };
    // Without the override the routine stays in the last tier: no deferral.
    expect(scenario(common).result).toEqual({});
    // With the override the routine enters the normal tier and, being older,
    // outranks the normal run: the claim is deferred.
    const overrideConfig: GateConfig = { ...CONFIG, routineOverrides: { rt1: "first" } };
    const decision = scenario({ ...common, config: overrideConfig });
    expect(decision.deferred).toBe(true);
    expect(decision.result.defer).toBe(true);
  });

  it("returns no opinion when the plugin is disabled", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self")],
      otherAgents: [
        agent("other", { queued: [run("other", "r-other", { issueId: "io", createdAtMs: NOW - 10 })] }),
      ],
      issues: { io: issue({ blockedCount: 3 }) },
      config: { ...CONFIG, enabled: false },
      hostSlotsRemaining: 1,
    });
    expect(decision.result).toEqual({});
  });

  it("fails open on a null snapshot (cold)", () => {
    const decision = decideGate({
      snapshot: null,
      config: CONFIG,
      consult: consult("r-self", "self", 1),
      deferrals: {},
      nowMs: NOW,
      maxSnapshotAgeMs: 5 * 60_000,
    });
    expect(decision.result).toEqual({});
    expect(decision.snapshotCold).toBe(true);
  });

  it("fails open on a stale snapshot", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self")],
      hostSlotsRemaining: 1,
      nowMs: NOW + 6 * 60_000, // snapshot is 6 minutes old
    });
    expect(decision.result).toEqual({});
    expect(decision.snapshotCold).toBe(true);
  });

  it("fails open when the consulted agent is gone", () => {
    const goneSelf = agent("self", { queued: [run("self", "r-gone")] });
    const other = agent("other", { queued: [run("other", "r-other", { issueId: "io", createdAtMs: NOW - 10 })] });
    const decision = decideGate({
      // The snapshot no longer contains the consulted agent at all.
      snapshot: snapshotOf([other], { io: issue({ blockedCount: 3 }) }),
      config: CONFIG,
      consult: consult("r-gone", "self", 1),
      deferrals: {},
      nowMs: NOW,
      maxSnapshotAgeMs: 5 * 60_000,
    });
    expect(decision.result).toEqual({});
    expect(goneSelf.agentId).toBe("self");
  });

  it("fails open when the consulted run vanished from the queue", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self")],
      topRunId: "r-gone",
      hostSlotsRemaining: 1,
    });
    expect(decision.result).toEqual({});
    expect(decision.snapshotCold).toBe(true);
  });

  it("fails open when the consulted run is not in the snapshot", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self")],
      topRunId: "r-unknown",
      hostSlotsRemaining: 1,
    });
    expect(decision.result).toEqual({});
    expect(decision.snapshotCold).toBe(true);
  });

  it("returns no opinion when the host gate is disabled (infinite slots)", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self")],
      otherAgents: [
        agent("other", { queued: [run("other", "r-other", { issueId: "io", createdAtMs: NOW - 10 })] }),
      ],
      issues: { io: issue({ blockedCount: 3 }) },
      hostSlotsRemaining: Number.POSITIVE_INFINITY,
    });
    expect(decision.result).toEqual({});
  });

  it("returns no opinion when the host gate is disabled (null over the wire)", () => {
    // ANT-906: a disabled host gate arrives as null, not Infinity, because
    // JSON.stringify(Infinity) === null.
    const decision = scenario({
      selfRuns: [run("self", "r-self")],
      otherAgents: [
        agent("other", { queued: [run("other", "r-other", { issueId: "io", createdAtMs: NOW - 10 })] }),
      ],
      issues: { io: issue({ blockedCount: 3 }) },
      hostSlotsRemaining: null,
    });
    expect(decision.result).toEqual({});
    expect(decision.deferred).toBe(false);
  });

  it("returns no opinion for a non-claimable consulted agent", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self")],
      hostSlotsRemaining: 1,
    });
    // Override the consulted agent to paused via the snapshot.
    const pausedSelf = agent("self", { status: "paused", queued: [run("self", "r-self")] });
    const decision2 = decideGate({
      snapshot: snapshotOf([pausedSelf]),
      config: CONFIG,
      consult: consult("r-self", "self", 1),
      deferrals: {},
      nowMs: NOW,
      maxSnapshotAgeMs: 5 * 60_000,
    });
    expect(decision2.result).toEqual({});
    expect(decision.result).toEqual({});
  });

  it("ages out: a run deferred longer than ageOutMinutes stops being deferred", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self", { issueId: "is", createdAtMs: NOW })],
      otherAgents: [
        agent("other", { queued: [run("other", "r-other", { issueId: "io", createdAtMs: NOW - 10 })] }),
      ],
      issues: {
        is: issue({ priority: "high", blockedCount: 0 }),
        io: issue({ priority: "low", blockedCount: 3 }),
      },
      config: { ...CONFIG, ageOutMinutes: 5 },
      deferrals: { "r-self": { firstDeferredAtMs: NOW - 6 * 60_000, lastReason: "x" } },
      hostSlotsRemaining: 1,
    });
    expect(decision.result).toEqual({});
    expect(decision.runAgedOut).toBe(true);
    expect(decision.clearDeferral).toBe(true);
  });

  it("keeps deferring while the deferral is younger than ageOutMinutes", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self", { issueId: "is", createdAtMs: NOW })],
      otherAgents: [
        agent("other", { queued: [run("other", "r-other", { issueId: "io", createdAtMs: NOW - 10 })] }),
      ],
      issues: {
        is: issue({ priority: "high", blockedCount: 0 }),
        io: issue({ priority: "low", blockedCount: 3 }),
      },
      config: { ...CONFIG, ageOutMinutes: 5 },
      deferrals: { "r-self": { firstDeferredAtMs: NOW - 4 * 60_000, lastReason: "x" } },
      hostSlotsRemaining: 1,
    });
    expect(decision.deferred).toBe(true);
    expect(decision.runAgedOut).toBe(false);
  });

  it("clears the deferral record when the run stops being deferred", () => {
    const decision = scenario({
      selfRuns: [run("self", "r-self")],
      hostSlotsRemaining: 5, // not scarce
      deferrals: { "r-self": { firstDeferredAtMs: NOW - 60_000, lastReason: "x" } },
    });
    expect(decision.result).toEqual({});
    expect(decision.clearDeferral).toBe(true);
  });
});

describe("buildSnapshotFromRows", () => {
  it("assembles agents, issues, and queued runs with transitive impact", () => {
    const snapshot = buildSnapshotFromRows({
      companyId: COMPANY,
      nowMs: NOW,
      queuedRuns: [
        {
          run_id: "r1",
          agent_id: "a1",
          issue_id: "i1",
          invocation_source: "automation",
          trigger_detail: "routine",
          created_at: new Date(NOW).toISOString(),
          origin_kind: "routine_execution",
          origin_id: "rt1",
        },
        {
          run_id: "r2",
          agent_id: "a2",
          issue_id: "i2",
          invocation_source: "on_demand",
          trigger_detail: "manual",
          created_at: new Date(NOW - 1000).toISOString(),
          origin_kind: "manual",
          origin_id: null,
        },
      ],
      agents: [
        {
          agent_id: "a1",
          name: "A1",
          status: "idle",
          reports_to: null,
          runtime_config: { heartbeat: { maxConcurrentRuns: 3 } },
          running: 1,
        },
        {
          agent_id: "a2",
          name: "A2",
          status: "idle",
          reports_to: null,
          runtime_config: null,
          running: 0,
        },
      ],
      issues: [
        { issue_id: "i1", priority: "medium", origin_kind: "routine_execution", origin_id: "rt1", direct_block_count: 1 },
        { issue_id: "i2", priority: "high", origin_kind: "manual", origin_id: null, direct_block_count: 2 },
      ],
      relations: [
        { blocker_id: "i2", blocked_id: "i3" },
        { blocker_id: "i2", blocked_id: "i4" },
        { blocker_id: "i3", blocked_id: "i5" },
        { blocker_id: "i1", blocked_id: "i2" },
      ],
    });
    expect(snapshot.agents["a1"].maxConcurrentRuns).toBe(3);
    expect(snapshot.agents["a1"].running).toBe(1);
    expect(snapshot.agents["a1"].queued[0]?.routine).toBe(true);
    expect(snapshot.agents["a1"].queued[0]?.routineOriginId).toBe("rt1");
    // i2 transitively blocks i3, i4, i5 → 3
    expect(snapshot.issues["i2"].blockedCount).toBe(3);
    expect(snapshot.issues["i1"].blockedCount).toBe(4); // i2, i3, i4, i5
    // FIFO within the agent's queue
    expect(snapshot.agents["a2"].queued.map((r) => r.runId)).toEqual(["r2"]);
  });

  it("falls back to context_snapshot.issueId when issue_id is null", () => {
    const snapshot = buildSnapshotFromRows({
      companyId: COMPANY,
      nowMs: NOW,
      queuedRuns: [
        {
          run_id: "r1",
          agent_id: "a1",
          issue_id: "ctx-issue",
          invocation_source: "on_demand",
          trigger_detail: null,
          created_at: new Date(NOW).toISOString(),
          origin_kind: null,
          origin_id: null,
        },
      ],
      agents: [
        { agent_id: "a1", name: "A1", status: "idle", reports_to: null, runtime_config: null, running: 0 },
      ],
      issues: [
        { issue_id: "ctx-issue", priority: "low", origin_kind: "manual", origin_id: null, direct_block_count: 0 },
      ],
      relations: [],
    });
    expect(snapshot.agents["a1"].queued[0]?.issueId).toBe("ctx-issue");
  });
});

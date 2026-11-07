import type { PaperclipPluginManifestV1 } from "@paperclipai/plugin-sdk";

export const PLUGIN_ID = "vreckan.global-wip-gate";

export const RECONCILE_JOB_KEY = "reconcile";

const manifest: PaperclipPluginManifestV1 = {
  id: PLUGIN_ID,
  apiVersion: 1,
  version: "1.0.0",
  displayName: "Global WIP Gate",
  description:
    "At-gate run ranking for the company-wide in-flight cap: routines last (unless overridden), blocking-impact first (weighted per agent), issue priority, FIFO within a tier. Defers scarce slots to higher-ranked claimable agents with an aging fail-open, and exposes a status/config dashboard. Only ever decreases slots or defers — it can never break the host cap, and if it dies ordering degrades to first-come.",
  author: "Vreckan <bob@vreckan.project>",
  categories: ["automation"],
  capabilities: [
    "scheduling.gate",
    "plugin.state.read",
    "plugin.state.write",
    "events.subscribe",
    "jobs.schedule",
    "api.routes.register",
    "database.namespace.read",
    "companies.read",
    "agents.read",
    "agents.pause",
    "agents.resume",
    "issues.read",
    "issue.relations.read",
  ],
  schedulingGate: true,
  entrypoints: {
    worker: "dist/worker.js",
  },
  database: {
    migrationsDir: "migrations",
    coreReadTables: [
      "companies",
      "agents",
      "issues",
      "issue_relations",
      "heartbeat_runs",
    ],
  },
  jobs: [
    {
      jobKey: RECONCILE_JOB_KEY,
      displayName: "Ranking reconcile",
      description:
        "Safety-net refresh of the cached per-company ranking snapshots (every minute). Enforcement is host-side; this job only keeps the cache warm.",
      schedule: "* * * * *",
    },
  ],
  apiRoutes: [
    {
      routeKey: "status",
      method: "GET",
      path: "/status",
      auth: "board-or-agent",
      capability: "api.routes.register",
      companyResolution: { from: "query", key: "companyId" },
    },
    {
      routeKey: "config",
      method: "GET",
      path: "/config",
      auth: "board-or-agent",
      capability: "api.routes.register",
      companyResolution: { from: "query", key: "companyId" },
    },
    {
      routeKey: "config",
      method: "PATCH",
      path: "/config",
      auth: "board",
      capability: "api.routes.register",
      companyResolution: { from: "query", key: "companyId" },
    },
    {
      routeKey: "agent-override",
      method: "POST",
      path: "/agent-override",
      auth: "board",
      capability: "api.routes.register",
      companyResolution: { from: "query", key: "companyId" },
    },
  ],
};

export default manifest;

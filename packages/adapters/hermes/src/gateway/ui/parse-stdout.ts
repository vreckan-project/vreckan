import type { TranscriptEntry } from "@paperclipai/adapter-utils";

/**
 * Strip ANSI escape sequences (CSI, OSC) from terminal text.
 * Same pattern used in claude-local adapter quota.ts.
 */
function stripAnsi(text: string): string {
  return text
    .replace(/\u001B\][^\u0007]*(?:\u0007|\u001B\\)/g, "")
    .replace(/\u001B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])/g, "");
}

function safeJsonParse(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function asRecord(value: unknown): Record<string, unknown> | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  return value as Record<string, unknown>;
}

function asString(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/**
 * Pull the reasoning text out of a `reasoning.available` event payload.
 * The exact field name is defined by the external Hermes gateway, so this
 * checks the plausible field names (mirroring the fallback chain used by
 * extractOutput() in gateway/server/execute.ts) and recurses one level into
 * nested `data` / `payload` records.
 */
function extractDirectReasoningText(data: Record<string, unknown>): string {
  return (
    asString(data.reasoning).trim() ||
    asString(data.reasoning_text).trim() ||
    asString(data.thinking).trim() ||
    asString(data.text).trim() ||
    asString(data.summary).trim() ||
    asString(data.content).trim()
  );
}

function extractReasoningText(data: Record<string, unknown> | null): string {
  if (!data) return "";
  const direct = extractDirectReasoningText(data);
  if (direct) return stripAnsi(direct);
  const nested = asRecord(data.data) ?? asRecord(data.payload);
  const nestedDirect = nested ? extractDirectReasoningText(nested) : "";
  return nestedDirect ? stripAnsi(nestedDirect) : "";
}

// ── Tool event pairing ─────────────────────────────────────────────────────

// The gateway streams tool.started and tool.completed as separate SSE events.
// When the runtime supplies a tool_call_id both events carry it and the UI
// pairs them directly; otherwise the parser pairs the oldest unpaired started
// event with the next id-less completed event (FIFO).
let syntheticToolCounter = 0;
const pendingToolIds: string[] = [];

function nextSyntheticToolId(): string {
  syntheticToolCounter += 1;
  return `hermes-gateway-tool-${syntheticToolCounter}`;
}

function extractToolCallId(data: Record<string, unknown> | null): string {
  if (!data) return "";
  const direct = asString(data.tool_call_id) || asString(data.toolCallId);
  if (direct) return direct;
  const nested = asRecord(data.data) ?? asRecord(data.payload);
  return nested ? asString(nested.tool_call_id) || asString(nested.toolCallId) : "";
}

function extractToolInfo(
  data: Record<string, unknown> | null,
): { tool: string; preview: string; duration: string; isError: boolean } {
  const nested = data ? asRecord(data.data) ?? asRecord(data.payload) : null;
  const source = nested ?? data;
  const tool = (source ? asString(source.tool) : "") || "tool";
  const preview = source ? asString(source.preview) : "";
  const rawDuration =
    source && typeof source.duration === "number"
      ? source.duration
      : Number.parseFloat(source ? asString(source.duration) : "");
  const duration = Number.isFinite(rawDuration) ? `${rawDuration}s` : "";
  const isError = source ? source.error === true : false;
  return { tool, preview, duration, isError };
}

export function parseHermesGatewayStdoutLine(line: string, ts: string): TranscriptEntry[] {
  const cleaned = stripAnsi(line);
  const trimmed = cleaned.trim();
  if (!trimmed) return [];

  const eventMatch = trimmed.match(/^\[hermes-gateway:event\]\s+run=([^\s]+)\s+event=([^\s]+)\s+data=(.*)$/s);
  if (eventMatch) {
    const eventName = eventMatch[2];
    const data = asRecord(safeJsonParse(eventMatch[3]));
    if (eventName === "message.delta") {
      const delta = asString(data?.delta) || asString(data?.text_delta);
      return delta ? [{ kind: "assistant", ts, text: stripAnsi(delta), delta: true }] : [];
    }
    if (eventName === "run.failed" || eventName === "run.error") {
      const message = asString(data?.error) || asString(data?.message) || "Hermes run failed";
      return [{ kind: "stderr", ts, text: message }];
    }
    if (eventName === "reasoning.available") {
      const reasoning = extractReasoningText(data);
      return [{ kind: "thinking", ts, text: reasoning || "Hermes reasoning available" }];
    }
    if (eventName === "tool.started") {
      const info = extractToolInfo(data);
      const callId = extractToolCallId(data);
      const id = callId || nextSyntheticToolId();
      if (!callId) pendingToolIds.push(id);
      return [
        {
          kind: "tool_call",
          ts,
          name: info.tool,
          input: info.preview ? { preview: info.preview } : {},
          toolUseId: id,
        },
      ];
    }
    if (eventName === "tool.completed") {
      const info = extractToolInfo(data);
      const callId = extractToolCallId(data);
      const id = callId || pendingToolIds.shift() || nextSyntheticToolId();
      const content = info.preview
        ? info.duration
          ? `${info.preview}  ${info.duration}`
          : info.preview
        : info.duration
          ? `[${info.tool}] ${info.duration}`
          : `[${info.tool}]`;
      return [
        {
          kind: "tool_result",
          ts,
          toolUseId: id,
          toolName: info.tool,
          content,
          isError: info.isError,
        },
      ];
    }
    if (eventName === "message.interim") {
      const text = ((data ? asString(data.text) : "") || (data ? asString(data.delta) : "")).trim();
      return text ? [{ kind: "assistant", ts, text: stripAnsi(text) }] : [];
    }
    return [{ kind: "system", ts, text: `Hermes event: ${eventName}` }];
  }

  if (trimmed.startsWith("[hermes-gateway]")) {
    return [{ kind: "system", ts, text: trimmed.replace(/^\[hermes-gateway\]\s*/, "") }];
  }

  return [{ kind: "stdout", ts, text: cleaned }];
}

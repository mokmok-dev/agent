import type { AgentCapability } from "@agent/protocol";
import type { Session, Turn, TurnUpdate } from "@agent/store";

/**
 * The two decisions the gateway path makes, kept free of sockets and storage so
 * both are testable: which queued turn a gateway connection receives, and what
 * a gateway event does to a session and turn.
 */

/** What the control plane sends a gateway when a session's turn is ready. */
export type PromptFrame = {
  type: "prompt";
  sessionId: string;
  /** The ACP session id, or `""` before the gateway has opened one. */
  acpSessionId: string;
  turnId: string;
  text: string;
  /**
   * The model the route chose. A `stdio` agent picks its own by default, so a
   * gateway that can select one is told which the route meant.
   */
  model?: { provider: string; modelId: string };
};

/**
 * The turn a gateway connection should receive for a session.
 *
 * Only a queued turn is dispatched: a turn already `dispatched` is either being
 * run by this gateway or was claimed before a disconnect, and re-sending it
 * would run the prompt twice. The ACP session id comes from the session record,
 * so a reconnect continues the conversation the agent already has.
 */
export function promptFor(session: Session, turn: Turn): PromptFrame {
  return {
    type: "prompt",
    sessionId: session.id,
    acpSessionId: session.acpSessionId ?? "",
    turnId: turn.id,
    text: turn.prompt,
    // A stored session always has a selection once it is routed; a session that
    // has not been routed has none, and then there is no model to send.
    ...(session.selection == null ? {} : { model: session.selection.model }),
  };
}

/** The queued turn a connection should be sent, if there is one. */
export function nextPrompt(session: Session, turns: readonly Turn[]): PromptFrame | undefined {
  const queued = turns
    .filter((turn) => turn.status === "queued")
    .sort((a, b) => a.createdAt - b.createdAt);
  const turn = queued[0];
  return turn === undefined ? undefined : promptFor(session, turn);
}

/** A gateway event, as the control plane reads it. Unknown types are ignored. */
export type GatewayReport =
  | { type: "session.bound"; sessionId: string; acpSessionId: string }
  | { type: "session.capability"; sessionId: string; capability: AgentCapability }
  | { type: "turn.started"; sessionId: string; turnId: string }
  | { type: "turn.completed"; sessionId: string; turnId: string; stopReason: string; text?: string }
  | { type: "turn.failed"; sessionId: string; turnId: string; error: string };

/** What applying an event does: a turn move, an ACP binding, a capability, or nothing. */
export type GatewayEffect =
  | { kind: "bind"; acpSessionId: string }
  | { kind: "capability"; capability: AgentCapability }
  | { kind: "settle"; turnId: string; update: TurnUpdate; text?: string }
  | { kind: "ignore"; reason: string };

/**
 * What one gateway event does to a session and its turns.
 *
 * `session.bound` records the ACP session id, which is what a later turn needs
 * to continue the same agent conversation. `turn.started` moves the turn to
 * `running`, so a turn that was dispatched is visibly underway. A completed or
 * failed event settles it, and the text a completed event carries is what the
 * reply path reads.
 */
export function effectOf(
  report: GatewayReport,
  session: Session,
  turns: readonly Turn[],
): GatewayEffect {
  if (report.sessionId !== session.id) {
    return { kind: "ignore", reason: "event is for another session" };
  }
  switch (report.type) {
    case "session.bound":
      return report.acpSessionId === ""
        ? { kind: "ignore", reason: "an empty ACP session id would lose the binding" }
        : { kind: "bind", acpSessionId: report.acpSessionId };
    case "session.capability":
      return { kind: "capability", capability: report.capability };
    case "turn.started": {
      const turn = turns.find((candidate) => candidate.id === report.turnId);
      if (turn === undefined) return { kind: "ignore", reason: "no such turn" };
      if (turn.status !== "dispatched") {
        return { kind: "ignore", reason: `turn is ${turn.status}, not dispatched` };
      }
      return { kind: "settle", turnId: turn.id, update: { status: "running" } };
    }
    case "turn.completed": {
      const turn = turns.find((candidate) => candidate.id === report.turnId);
      if (turn === undefined) return { kind: "ignore", reason: "no such turn" };
      // `running` is what `turn.started` moves the turn to, and the state table
      // only completes from there. A gateway that skipped the start event is a
      // defect on its side, so the turn is left for the failure path.
      if (turn.status !== "running") {
        return { kind: "ignore", reason: `turn is ${turn.status}, not running` };
      }
      return {
        kind: "settle",
        turnId: turn.id,
        update: { status: "completed", stopReason: report.stopReason },
        ...(report.text === undefined ? {} : { text: report.text }),
      };
    }
    case "turn.failed": {
      const turn = turns.find((candidate) => candidate.id === report.turnId);
      if (turn === undefined) return { kind: "ignore", reason: "no such turn" };
      if (turn.status === "completed" || turn.status === "failed") {
        return { kind: "ignore", reason: `turn is ${turn.status}, already settled` };
      }
      return { kind: "settle", turnId: turn.id, update: { status: "failed", error: report.error } };
    }
  }
}

/** Read a gateway's JSON as a report, or `undefined` when it is not one. */
export function reportOf(value: unknown): GatewayReport | undefined {
  const payload = bodyOf(value);
  if (payload === undefined) return undefined;
  const record = payload;
  const str = (key: string): string | undefined =>
    typeof record[key] === "string" ? (record[key] as string) : undefined;
  const sessionId = str("sessionId");
  if (sessionId === undefined) return undefined;
  switch (record["type"]) {
    case "session.bound": {
      const acpSessionId = str("acpSessionId");
      return acpSessionId === undefined
        ? undefined
        : { type: "session.bound", sessionId, acpSessionId };
    }
    case "session.capability": {
      const capability = capabilityOf(record["capability"]);
      return capability === undefined
        ? undefined
        : { type: "session.capability", sessionId, capability };
    }
    case "turn.started": {
      const turnId = str("turnId");
      return turnId === undefined ? undefined : { type: "turn.started", sessionId, turnId };
    }
    case "turn.completed": {
      const turnId = str("turnId");
      if (turnId === undefined) return undefined;
      const text = str("text");
      return {
        type: "turn.completed",
        sessionId,
        turnId,
        stopReason: str("stopReason") ?? "end_turn",
        ...(text === undefined ? {} : { text }),
      };
    }
    case "turn.failed": {
      const turnId = str("turnId");
      if (turnId === undefined) return undefined;
      return {
        type: "turn.failed",
        sessionId,
        turnId,
        error: str("error") ?? "the gateway reported a failure with no reason",
      };
    }
    default:
      return undefined;
  }
}

/**
 * The payload of a gateway message.
 *
 * The gateway speaks JSON-RPC, so an event arrives as a `gateway/event`
 * notification whose `params` is the event itself. A bare object is accepted
 * too, which keeps a hand-written frame valid and makes the reducer usable
 * without the codec.
 */
function bodyOf(value: unknown): Record<string, unknown> | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const record = value as Record<string, unknown>;
  if (record["method"] === "gateway/event") {
    const params = record["params"];
    return typeof params === "object" && params !== null
      ? (params as Record<string, unknown>)
      : undefined;
  }
  return record;
}

/** The session a gateway's `hello` names, or `undefined` for another frame. */
export function helloOf(value: unknown): string | undefined {
  const record = bodyOf(value);
  if (record === undefined || record["type"] !== "hello") return undefined;
  return typeof record["sessionId"] === "string" ? record["sessionId"] : undefined;
}

/**
 * The capability an agent stated, read from the frame the gateway sends.
 *
 * It arrives from a process outside this deployment, so every field is checked
 * rather than trusted: a malformed capability is dropped, which leaves the
 * session as it was instead of recording a model list that is not real.
 */
export function capabilityOf(value: unknown): AgentCapability | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const record = value as Record<string, unknown>;
  const stated = record["availableModels"];
  if (!Array.isArray(stated)) return undefined;
  const availableModels = stated.filter((model): model is string => typeof model === "string");
  const agentInfo = record["agent"];
  const agent =
    typeof agentInfo === "object" &&
    agentInfo !== null &&
    typeof (agentInfo as Record<string, unknown>)["name"] === "string" &&
    typeof (agentInfo as Record<string, unknown>)["version"] === "string"
      ? {
          name: (agentInfo as { name: string }).name,
          version: (agentInfo as { version: string }).version,
        }
      : undefined;
  const currentModelId = record["currentModelId"];
  return {
    ...(agent === undefined ? {} : { agent }),
    ...(typeof currentModelId === "string" ? { currentModelId } : {}),
    availableModels,
    statesModels: record["statesModels"] === true,
  };
}

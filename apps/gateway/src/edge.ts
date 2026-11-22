import {
  decodeMessage,
  encodeNotification,
  lenient,
  object,
  oneOf,
  optional,
  str,
  tagged,
} from "@agent/protocol";
import type { GatewayEvent } from "./bridge.ts";

/**
 * The local event log. Every event the gateway sends is appended here first,
 * so a reconnect can replay what the control plane may have missed.
 */
export class EventLog {
  readonly #events: GatewayEvent[] = [];
  #acknowledged = 0;

  constructor(readonly limit: number) {}

  append(event: GatewayEvent): void {
    this.#events.push(event);
    if (this.#events.length > this.limit) {
      this.#events.shift();
      this.#acknowledged = Math.max(0, this.#acknowledged - 1);
    }
  }

  unacknowledged(): GatewayEvent[] {
    return this.#events.slice(this.#acknowledged);
  }

  acknowledge(): void {
    this.#acknowledged = this.#events.length;
  }

  retained(): GatewayEvent[] {
    return [...this.#events];
  }

  get pending(): number {
    return this.#events.length - this.#acknowledged;
  }
}

export type EdgeFrame =
  | {
      type: "prompt";
      sessionId: string;
      acpSessionId: string;
      turnId: string;
      text: string;
      /** The model the route chose, for an agent that lets the client pick one. */
      model?: { provider: string; modelId: string };
    }
  | { type: "cancel"; acpSessionId: string }
  | { type: "permission"; toolCallId: string; optionId: string | null }
  | { type: "ping" };

export const decodeEdgeFrame = tagged<EdgeFrame>("type", {
  ping: object({ type: oneOf("ping") }),
  cancel: object({ type: oneOf("cancel"), acpSessionId: str }),
  prompt: object({
    type: oneOf("prompt"),
    sessionId: str,
    acpSessionId: str,
    turnId: str,
    text: str,
    // A model the route chose is a hint, so a malformed one drops the field and
    // keeps the prompt: the agent then runs its own model.
    model: optional(lenient(object({ provider: str, modelId: str }))),
  }),
  permission: (value) => {
    const frame = object({
      type: oneOf("permission"),
      toolCallId: str,
      optionId: optional(lenient(str)),
    })(value);
    if (frame.kind === "rejected") return frame;
    return { kind: "decoded", value: { ...frame.value, optionId: frame.value.optionId ?? null } };
  },
});

/** A control-plane frame as a command. A malformed frame is `undefined`. */
export function parseEdgeFrame(text: string): EdgeFrame | undefined {
  const message = decodeMessage(text);
  if (message.kind === "rejected") return undefined;
  const decoded = lenient(decodeEdgeFrame)((message.message as { params?: unknown }).params);
  return decoded.kind === "decoded" ? decoded.value : undefined;
}

export function encodeEvent(event: GatewayEvent): string {
  return encodeNotification("gateway/event", event);
}

/**
 * The failure owed to a turn when the agent never started.
 *
 * A gateway that could not spawn its agent has no process to read the prompt,
 * so the prompt response never comes and the turn would wait forever. The
 * reason is the agent's own start error, which names the missing binary rather
 * than a timeout this side invented.
 */
export function agentStartFailureFor(
  turn: { sessionId: string; turnId: string },
  reason: string,
): GatewayEvent {
  return {
    type: "turn.failed",
    sessionId: turn.sessionId,
    turnId: turn.turnId,
    error: `the agent could not start: ${reason}`,
  };
}

/**
 * The failure owed to a turn when its agent process exits mid-turn.
 *
 * A prompt response arrives only if the agent lives to send one. An agent that
 * exits first leaves the turn `running` with nothing to settle it, so the exit
 * becomes the turn's failure and the thread learns what happened. The code is
 * the agent's own, which is the difference between "the agent crashed" and a
 * timeout this side invented.
 */
export function agentExitFailure(
  turn: { sessionId: string; turnId: string },
  code: number | null,
): GatewayEvent {
  return {
    type: "turn.failed",
    sessionId: turn.sessionId,
    turnId: turn.turnId,
    // A signal leaves no exit code. Reporting `null` as a number would invent
    // one; the signal is the fact, so it is named when there is no code.
    error:
      code === null
        ? "the agent exited from a signal before answering"
        : `the agent exited with code ${code} before answering`,
  };
}

/**
 * The `hello` a gateway sends as soon as its socket opens.
 *
 * Every gateway announces itself, whatever it was configured with. A gateway
 * started with `AGENT_SESSION_ID` names that session; one started with
 * nothing — which is how a gateway serves a delegation that had not happened
 * yet — asks the control plane to name the session it reached. The control
 * plane sends a turn only in answer to a hello, so skipping it leaves the
 * socket open and silent.
 */
export function helloFrame(configured: string | undefined): string {
  return JSON.stringify({ type: "hello", sessionId: configured ?? "auto" });
}

/**
 * The session id a control plane's answer to a hello names, if it is one.
 *
 * The answer arrives from the deployment, so every field is checked: a frame
 * that is not JSON, names another type, or carries a non-string or empty id
 * names no session. Treating one of those as a name would make the gateway
 * serve a session that does not exist.
 */
export function identityOf(text: string): string | undefined {
  const parsed = parseJson(text);
  // `null` and a bare scalar are valid JSON, so the body must be an object
  // before its fields are read.
  if (typeof parsed !== "object" || parsed === null) return undefined;
  const params = (parsed as { params?: unknown }).params;
  if (typeof params !== "object" || params === null) return undefined;
  const frame = params as { type?: unknown; sessionId?: unknown };
  if (frame.type !== "hello") return undefined;
  const sessionId = frame.sessionId;
  // An empty id names no session, so it is the same as no answer at all.
  return typeof sessionId === "string" && sessionId !== "" ? sessionId : undefined;
}

function parseJson(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

export { decodeMessage };

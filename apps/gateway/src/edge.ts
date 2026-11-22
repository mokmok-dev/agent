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

export { decodeMessage };

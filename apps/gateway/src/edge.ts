import {
  decodeMessage,
  encodeNotification,
  lenient,
  object,
  oneOf,
  optional,
  str,
  tagged,
  type RpcMessage,
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

  /** Everything not yet acknowledged, oldest first. */
  unacknowledged(): GatewayEvent[] {
    return this.#events.slice(this.#acknowledged);
  }

  /** Mark everything appended so far as delivered. */
  acknowledge(): void {
    this.#acknowledged = this.#events.length;
  }

  /** The events still held by the log. */
  retained(): GatewayEvent[] {
    return [...this.#events];
  }

  get pending(): number {
    return this.#events.length - this.#acknowledged;
  }
}

/** A command from the control plane. */
export type EdgeFrame =
  | { type: "prompt"; sessionId: string; acpSessionId: string; turnId: string; text: string }
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
  }),
  permission: (value) => {
    const frame = object({
      type: oneOf("permission"),
      toolCallId: str,
      optionId: optional(lenient(str)),
    })(value);
    return { ...frame, optionId: frame.optionId ?? null };
  },
});

/** A control-plane frame as a command. A malformed frame is `undefined`. */
export function parseEdgeFrame(text: string): EdgeFrame | undefined {
  let message: RpcMessage;
  try {
    message = decodeMessage(text);
  } catch {
    return undefined;
  }
  // Only requests and notifications carry params. A result or an error has none,
  // and the decoder rejects the missing params like any other unknown frame.
  return lenient(decodeEdgeFrame)((message as { params?: unknown }).params);
}

/** A gateway event as the notification the control plane reads. */
export function encodeEvent(event: GatewayEvent): string {
  return encodeNotification("gateway/event", event);
}

export { decodeMessage };

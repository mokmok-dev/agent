import { decodeMessage, type RpcMessage } from "@agent/protocol";

/** The frames this test-side agent pushes into the bridge. */
export type Inbound = (text: string) => void;

/**
 * A stand-in for the agent process. `send` is what the bridge writes, and a
 * test drives the bridge with `deliver`, which is wired to `bridge.receive`.
 * Replies never re-enter through `send`, so the recorded frames stay the
 * bridge's own output.
 */
export class AgentRecorder {
  readonly sent: RpcMessage[] = [];
  inbound: Inbound | undefined;

  send(text: string): void {
    this.sent.push(decodeMessage(text));
  }

  /** The frames the bridge sent as requests or notifications, oldest first. */
  outbound(): Extract<RpcMessage, { kind: "request" | "notification" }>[] {
    return this.sent.filter(
      (message): message is Extract<RpcMessage, { kind: "request" | "notification" }> =>
        message.kind === "request" || message.kind === "notification",
    );
  }

  last(): Extract<RpcMessage, { kind: "request" | "notification" }> {
    const message = this.outbound().at(-1);
    if (message === undefined) throw new Error("the bridge sent nothing");
    return message;
  }

  /** Answer a request the bridge sent. */
  answer(id: string | number, result: unknown): void {
    this.deliver(JSON.stringify({ jsonrpc: "2.0", id, result }));
  }

  /** Reject a request the bridge sent. */
  fail(id: string | number, code: number, message: string): void {
    this.deliver(JSON.stringify({ jsonrpc: "2.0", id, error: { code, message } }));
  }

  /** Send a session update from the agent. */
  update(sessionId: string, update: unknown): void {
    this.deliver(
      JSON.stringify({ jsonrpc: "2.0", method: "session/update", params: { sessionId, update } }),
    );
  }

  /** Ask the bridge for permission. */
  askPermission(id: number, params: unknown): void {
    this.deliver(
      JSON.stringify({ jsonrpc: "2.0", id, method: "session/request_permission", params }),
    );
  }

  deliver(text: string): void {
    if (this.inbound === undefined) throw new Error("the recorder is not attached to a bridge");
    this.inbound(text);
  }
}

/** Wait until `probe` returns a truthy value, yielding to the event loop between attempts. */
export async function until<T>(probe: () => T | undefined | false, attempts = 20): Promise<T> {
  for (let attempt = 0; attempt < attempts; attempt++) {
    const value = probe();
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
  throw new Error("the bridge never reached the expected frame");
}

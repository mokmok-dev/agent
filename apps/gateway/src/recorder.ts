import { decodeMessage, type RpcMessage } from "@agent/protocol";

export type Inbound = (text: string) => void;

export class AgentRecorder {
  readonly sent: RpcMessage[] = [];
  inbound: Inbound | undefined;

  send(text: string): void {
    this.sent.push(decodeMessage(text));
  }

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

  answer(id: string | number, result: unknown): void {
    this.deliver(JSON.stringify({ jsonrpc: "2.0", id, result }));
  }

  fail(id: string | number, code: number, message: string): void {
    this.deliver(JSON.stringify({ jsonrpc: "2.0", id, error: { code, message } }));
  }

  update(sessionId: string, update: unknown): void {
    this.deliver(
      JSON.stringify({ jsonrpc: "2.0", method: "session/update", params: { sessionId, update } }),
    );
  }

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

export async function until<T>(probe: () => T | undefined | false, attempts = 20): Promise<T> {
  for (let attempt = 0; attempt < attempts; attempt++) {
    const value = probe();
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
  throw new Error("the bridge never reached the expected frame");
}

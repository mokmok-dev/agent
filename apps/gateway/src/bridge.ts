import {
  AcpClient,
  ProtocolError,
  type AgentRequestHandlers,
  type SessionUpdate,
} from "@agent/protocol";
import { PendingPermission } from "./permission.ts";

export interface AgentChannel {
  send(text: string): void;
}

export type GatewayEvent =
  | { type: "turn.started"; turnId: string; sessionId: string }
  | { type: "turn.update"; turnId: string; sessionId: string; update: SessionUpdate }
  | { type: "turn.completed"; turnId: string; sessionId: string; stopReason: string; text: string }
  | { type: "turn.failed"; turnId: string; sessionId: string; error: string }
  | { type: "permission.asked"; turnId: string; toolCallId: string; options: string[] }
  | { type: "session.bound"; sessionId: string; acpSessionId: string };

/** The text a turn's message chunks have produced so far. */
type ActiveTurn = { turnId: string; sessionId: string; text: string };

export class AcpBridge {
  readonly #client: AcpClient;
  readonly #emit: (event: GatewayEvent) => void;
  readonly #permissions = new Map<string, PendingPermission>();
  #turn: ActiveTurn | undefined;

  constructor(options: { channel: AgentChannel; emit: (event: GatewayEvent) => void }) {
    this.#emit = options.emit;
    const handlers: AgentRequestHandlers = {
      onSessionUpdate: (update) => this.#update(update),
      requestPermission: async (params) => {
        const optionId = await this.#ask(
          params.toolCall.toolCallId,
          params.options.map((o) => o.optionId),
        );
        return optionId === undefined
          ? { outcome: "cancelled" }
          : { outcome: "selected", optionId };
      },
    };
    this.#client = new AcpClient({
      channel: options.channel,
      handlers,
      clientInfo: { name: "agent-gateway", version: "0" },
    });
  }

  receive(text: string): void {
    this.#client.receive(text);
  }

  get pending(): number {
    return this.#client.pendingCount;
  }

  async open(sessionId: string, cwd: string): Promise<string> {
    await this.#client.initialize(this.#client.defaultInitializeParams());
    // An agent requires `mcpServers` in some form and ignores an empty one; a
    // record is what opencode2 accepts, and ACP allows it in place of the array.
    const session = await this.#client.newSession({ cwd, mcpServers: {} });
    this.#emit({ type: "session.bound", sessionId, acpSessionId: session.sessionId });
    return session.sessionId;
  }

  async run(sessionId: string, acpSessionId: string, turnId: string, text: string): Promise<void> {
    this.#turn = { turnId, sessionId, text: "" };
    this.#emit({ type: "turn.started", turnId, sessionId });
    try {
      const result = await this.#client.prompt(acpSessionId, [{ type: "text", text }]);
      this.#emit({
        type: "turn.completed",
        turnId,
        sessionId,
        stopReason: result.stopReason,
        text: this.#turn.text,
      });
    } catch (error) {
      this.#emit({ type: "turn.failed", turnId, sessionId, error: errorMessage(error) });
    } finally {
      this.#turn = undefined;
    }
  }

  cancel(acpSessionId: string): void {
    this.#client.cancel(acpSessionId);
  }

  answer(toolCallId: string, optionId: string | undefined): boolean {
    const permission = this.#permissions.get(toolCallId);
    if (permission === undefined) return false;
    this.#permissions.delete(toolCallId);
    permission.decide(optionId);
    return true;
  }

  #ask(toolCallId: string, options: string[]): Promise<string | undefined> {
    const permission = new PendingPermission();
    this.#permissions.set(toolCallId, permission);
    this.#emit({
      type: "permission.asked",
      turnId: this.#turn?.turnId ?? "",
      toolCallId,
      options,
    });
    return permission.chosen();
  }

  #update(update: SessionUpdate): void {
    const turn = this.#turn;
    if (turn === undefined) return;
    // The agent's words are what a Linear reply carries, so the chunks are
    // collected as they arrive and the text travels with the completed event.
    // The two checks are separate statements because each is its own question:
    // is this a message at all, and does it carry text.
    if (update.sessionUpdate === "agent_message_chunk") {
      if (update.content.type === "text") turn.text += update.content.text;
    }
    this.#emit({ type: "turn.update", turnId: turn.turnId, sessionId: turn.sessionId, update });
  }
}

export function errorMessage(error: unknown): string {
  if (error instanceof ProtocolError) return `${error.code}: ${error.message}`;
  if (error instanceof Error) return error.message;
  return String(error);
}

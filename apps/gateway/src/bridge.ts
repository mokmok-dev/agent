import {
  AcpClient,
  ProtocolError,
  type AgentRequestHandlers,
  type SessionUpdate,
} from "@agent/protocol";

/** The transport the gateway speaks to the ACP server. */
export interface AgentChannel {
  send(text: string): void;
}

/** Everything the gateway reports to the control plane. */
export type GatewayEvent =
  | { type: "turn.started"; turnId: string; sessionId: string }
  | { type: "turn.update"; turnId: string; sessionId: string; update: SessionUpdate }
  | { type: "turn.completed"; turnId: string; sessionId: string; stopReason: string }
  | { type: "turn.failed"; turnId: string; sessionId: string; error: string }
  | { type: "permission.asked"; turnId: string; toolCallId: string; options: string[] }
  | { type: "session.bound"; sessionId: string; acpSessionId: string };

/** A permission decision, resolved when the person answers. */
type Decision = (optionId: string | undefined) => void;

/** The turn this bridge is running. Both ids are set together, so one field holds both. */
type ActiveTurn = { turnId: string; sessionId: string };

/**
 * One ACP conversation on the VM. It owns the `AcpClient`, turns agent updates
 * into gateway events, and keeps the turn it is running. A permission request
 * pauses the turn until the control plane relays the person's answer.
 */
export class AcpBridge {
  readonly #client: AcpClient;
  readonly #emit: (event: GatewayEvent) => void;
  readonly #decisions = new Map<string, Decision>();
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

  /** ACP frames from the agent process, one per line. */
  receive(text: string): void {
    this.#client.receive(text);
  }

  get pending(): number {
    return this.#client.pendingCount;
  }

  /** Open the ACP session for a control-plane session, and report the bound id. */
  async open(sessionId: string, cwd: string): Promise<string> {
    await this.#client.initialize(this.#client.defaultInitializeParams());
    const session = await this.#client.newSession({ cwd });
    this.#emit({ type: "session.bound", sessionId, acpSessionId: session.sessionId });
    return session.sessionId;
  }

  /** Run one prompt turn and report how it ended. */
  async run(sessionId: string, acpSessionId: string, turnId: string, text: string): Promise<void> {
    this.#turn = { turnId, sessionId };
    this.#emit({ type: "turn.started", turnId, sessionId });
    try {
      const result = await this.#client.prompt(acpSessionId, [{ type: "text", text }]);
      this.#emit({ type: "turn.completed", turnId, sessionId, stopReason: result.stopReason });
    } catch (error) {
      this.#emit({ type: "turn.failed", turnId, sessionId, error: errorMessage(error) });
    } finally {
      this.#turn = undefined;
    }
  }

  /** Ask the agent to stop. The prompt response then ends the turn as cancelled. */
  cancel(acpSessionId: string): void {
    this.#client.cancel(acpSessionId);
  }

  /** Resolve the permission request a person answered. An unknown id is ignored. */
  answer(toolCallId: string, optionId: string | undefined): boolean {
    const decision = this.#decisions.get(toolCallId);
    if (decision === undefined) return false;
    this.#decisions.delete(toolCallId);
    decision(optionId);
    return true;
  }

  /** A permission request held until `answer` is called. */
  #ask(toolCallId: string, options: string[]): Promise<string | undefined> {
    this.#emit({
      type: "permission.asked",
      turnId: this.#turn?.turnId ?? "",
      toolCallId,
      options,
    });
    return new Promise<string | undefined>((resolve) => {
      this.#decisions.set(toolCallId, resolve);
    });
  }

  #update(update: SessionUpdate): void {
    const turn = this.#turn;
    if (turn === undefined) return;
    this.#emit({ type: "turn.update", turnId: turn.turnId, sessionId: turn.sessionId, update });
  }
}

/** The message a failed turn reports. A JSON-RPC error keeps its code. */
export function errorMessage(error: unknown): string {
  if (error instanceof ProtocolError) return `${error.code}: ${error.message}`;
  if (error instanceof Error) return error.message;
  return String(error);
}

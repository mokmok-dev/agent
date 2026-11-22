import {
  AcpClient,
  ProtocolError,
  agentCapabilityOf,
  emptyMcpServers,
  modelValueFor,
  type AgentCapability,
  type AgentRequestHandlers,
  type McpServersShape,
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
  | { type: "session.bound"; sessionId: string; acpSessionId: string }
  | { type: "session.capability"; sessionId: string; capability: AgentCapability };

/** The text a turn's message chunks have produced so far. */
type ActiveTurn = { turnId: string; sessionId: string; text: string };

export class AcpBridge {
  readonly #client: AcpClient;
  readonly #emit: (event: GatewayEvent) => void;
  readonly #permissions = new Map<string, PendingPermission>();
  /** The empty `mcpServers` shape this agent accepts. */
  readonly #mcpServers: McpServersShape;
  /** What the agent said it can run, which is what a model choice is checked against. */
  #capability: AgentCapability | undefined;
  #turn: ActiveTurn | undefined;

  constructor(options: {
    channel: AgentChannel;
    emit: (event: GatewayEvent) => void;
    /**
     * How the empty `mcpServers` is sent. `opencode2 acp` requires a record and
     * `grok agent stdio` requires an array, so the host states which agent it
     * started rather than the bridge guessing.
     */
    mcpServers?: McpServersShape;
  }) {
    this.#emit = options.emit;
    this.#mcpServers = options.mcpServers ?? "array";
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
    const initialized = await this.#client.initialize(this.#client.defaultInitializeParams());
    // The field is required, and its empty form differs by agent: a record for
    // opencode2, an array for grok. The host states which, so this module does
    // not carry a table of agents.
    const session = await this.#client.newSession({
      cwd,
      mcpServers: emptyMcpServers(this.#mcpServers),
    });
    this.#emit({ type: "session.bound", sessionId, acpSessionId: session.sessionId });
    this.#capability = agentCapabilityOf(initialized, session);
    // A `stdio` agent's models depend on its own credentials, so the control
    // plane learns what this one offers instead of assuming the routed model.
    this.#emit({ type: "session.capability", sessionId, capability: this.#capability });
    return session.sessionId;
  }

  /**
   * Tell the agent which model the route chose, when the agent lets the client
   * pick one. An agent that names its models takes the id it lists; an agent
   * that names none has nothing to select, so the call is skipped rather than
   * assumed to work.
   */
  async selectModel(
    acpSessionId: string,
    model: { provider: string; modelId: string },
  ): Promise<boolean> {
    const capability = this.#capability;
    // No report means no list to match against, which is not the same as a list
    // the model happens to be missing from.
    if (capability === undefined) return false;
    const value = modelValueFor(capability.availableModels, model);
    if (value === undefined) return false;
    try {
      await this.#client.setConfigOption({
        sessionId: acpSessionId,
        configId: "model",
        value,
      });
      return true;
    } catch {
      // An agent that reports models but cannot change them is not a defect on
      // this side; the turn still runs on the model the agent already chose.
      return false;
    }
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
  if (error instanceof ProtocolError) {
    // An agent often puts the real cause in `data`, as grok does when the
    // account cannot pay for a model: `-32603: Internal error` alone would hide
    // why the turn failed. The detail is included when it is a string or names
    // one, and left out when it adds nothing.
    const detail = detailOf(error.data);
    return detail === undefined
      ? `${error.code}: ${error.message}`
      : `${error.code}: ${error.message} (${detail})`;
  }
  if (error instanceof Error) return error.message;
  return String(error);
}

/** The most specific text an error's `data` carries, when it carries any. */
function detailOf(data: unknown): string | undefined {
  if (typeof data === "string") return data === "" ? undefined : data;
  if (typeof data !== "object" || data === null) return undefined;
  const record = data as Record<string, unknown>;
  for (const key of ["message", "detail", "reason"]) {
    const value = record[key];
    if (typeof value === "string" && value !== "") return value;
  }
  return undefined;
}

import {
  decodeMessage,
  encodeErrorResponse,
  encodeNotification,
  encodeRequest,
  encodeResult,
  isRecord,
  ProtocolError,
  RpcErrorCode,
  type RequestId,
  type RpcMessage,
} from "../jsonrpc.ts";
import { AgentMethod, ClientMethod, PROTOCOL_VERSION } from "./types.ts";
import {
  decodeCreateTerminalParams,
  decodeInitializeResult,
  decodeNewSessionResult,
  decodePromptResult,
  decodeReadTextFileParams,
  decodeRequestPermissionParams,
  decodeSessionUpdate,
  decodeTerminalOutputResult,
  decodeTerminalRefParams,
  decodeWriteTextFileParams,
  type ContentBlock,
  type CreateTerminalParams,
  type CreateTerminalResult,
  type InitializeParams,
  type InitializeResult,
  type NewSessionParams,
  type NewSessionResult,
  type PromptResult,
  type ReadTextFileParams,
  type ReadTextFileResult,
  type RequestPermissionParams,
  type RequestPermissionResult,
  type SessionUpdate,
  type TerminalOutputResult,
  type WaitForTerminalExitResult,
  type WriteTextFileParams,
} from "./types.ts";

/** The transport an `AcpClient` writes JSON-RPC frames to. */
export interface Channel {
  send(text: string): void;
}

/** How the client answers the agent's own requests. A method with no handler is refused. */
export interface AgentRequestHandlers {
  onSessionUpdate?: (update: SessionUpdate) => void;
  requestPermission?: (params: RequestPermissionParams) => Promise<RequestPermissionResult>;
  readTextFile?: (params: ReadTextFileParams) => Promise<ReadTextFileResult>;
  writeTextFile?: (params: WriteTextFileParams) => Promise<void>;
  createTerminal?: (params: CreateTerminalParams) => Promise<CreateTerminalResult>;
  terminalOutput?: (params: TerminalRef) => Promise<TerminalOutputResult>;
  waitForTerminalExit?: (params: TerminalRef) => Promise<WaitForTerminalExitResult>;
  killTerminal?: (params: TerminalRef) => Promise<void>;
  releaseTerminal?: (params: TerminalRef) => Promise<void>;
}

export type TerminalRef = { sessionId: string; terminalId: string };

export type AcpClientOptions = {
  channel: Channel;
  handlers?: AgentRequestHandlers;
  /** Protocol version to ask for. Defaults to ACP v1. */
  protocolVersion?: number;
  clientInfo?: { name: string; version: string };
  clientCapabilities?: InitializeParams["clientCapabilities"];
};

type Pending = { resolve: (value: unknown) => void; reject: (error: ProtocolError) => void };

/** One agent-to-client method: take raw params, return the JSON-RPC result. */
type Route = (params: unknown) => Promise<unknown>;

const noHandler = (method: string): ProtocolError =>
  new ProtocolError(RpcErrorCode.MethodNotFound, `this client does not handle ${method}`);

/**
 * One entry per client method, so a missing handler is refused by the same
 * code path for every method and each entry reads as one rule.
 */
function routes(handlers: AgentRequestHandlers): Record<string, Route> {
  const route =
    <T>(decode: (value: unknown) => T, then: (value: T) => Promise<unknown>): Route =>
    (params) =>
      then(decode(params));

  return {
    [ClientMethod.RequestPermission]: route(decodeRequestPermissionParams, (params) =>
      handlers.requestPermission
        ? handlers.requestPermission(params)
        : Promise.reject(noHandler(ClientMethod.RequestPermission)),
    ),
    [ClientMethod.ReadTextFile]: route(decodeReadTextFileParams, (params) =>
      handlers.readTextFile
        ? handlers.readTextFile(params)
        : Promise.reject(noHandler(ClientMethod.ReadTextFile)),
    ),
    [ClientMethod.WriteTextFile]: route(decodeWriteTextFileParams, async (params) => {
      if (!handlers.writeTextFile) throw noHandler(ClientMethod.WriteTextFile);
      await handlers.writeTextFile(params);
      return null;
    }),
    [ClientMethod.CreateTerminal]: route(decodeCreateTerminalParams, (params) =>
      handlers.createTerminal
        ? handlers.createTerminal(params)
        : Promise.reject(noHandler(ClientMethod.CreateTerminal)),
    ),
    [ClientMethod.TerminalOutput]: route(decodeTerminalRefParams, async (params) => {
      if (!handlers.terminalOutput) throw noHandler(ClientMethod.TerminalOutput);
      return decodeTerminalOutputResult(await handlers.terminalOutput(params));
    }),
    [ClientMethod.WaitForTerminalExit]: route(decodeTerminalRefParams, (params) =>
      handlers.waitForTerminalExit
        ? handlers.waitForTerminalExit(params)
        : Promise.reject(noHandler(ClientMethod.WaitForTerminalExit)),
    ),
    [ClientMethod.KillTerminal]: route(decodeTerminalRefParams, async (params) => {
      if (!handlers.killTerminal) throw noHandler(ClientMethod.KillTerminal);
      await handlers.killTerminal(params);
      return null;
    }),
    [ClientMethod.ReleaseTerminal]: route(decodeTerminalRefParams, async (params) => {
      if (!handlers.releaseTerminal) throw noHandler(ClientMethod.ReleaseTerminal);
      await handlers.releaseTerminal(params);
      return null;
    }),
  };
}

/**
 * The client half of ACP. It correlates responses by id, answers the agent's
 * requests through `handlers`, and forwards `session/update` notifications.
 */
export class AcpClient {
  readonly #channel: Channel;
  readonly #handlers: AgentRequestHandlers;
  readonly #version: number;
  readonly #clientInfo: { name: string; version: string } | undefined;
  readonly #capabilities: InitializeParams["clientCapabilities"] | undefined;
  readonly #pending = new Map<RequestId, Pending>();
  readonly #routes: Record<string, Route>;
  #nextId = 1;

  constructor(options: AcpClientOptions) {
    this.#channel = options.channel;
    this.#handlers = options.handlers ?? {};
    this.#version = options.protocolVersion ?? PROTOCOL_VERSION;
    this.#clientInfo = options.clientInfo;
    this.#capabilities = options.clientCapabilities;
    this.#routes = routes(this.#handlers);
  }

  /** Requests still waiting for a response. */
  get pendingCount(): number {
    return this.#pending.size;
  }
  /** Send a request and resolve with its result. A JSON-RPC error reply rejects. */
  request(method: string, params: unknown): Promise<unknown> {
    const id = this.#nextId++;
    const promise = new Promise<unknown>((resolve, rejectPromise) => {
      this.#pending.set(id, { resolve, reject: rejectPromise });
    });
    this.#channel.send(encodeRequest(id, method, params));
    return promise;
  }

  notify(method: string, params: unknown): void {
    this.#channel.send(encodeNotification(method, params));
  }

  /** The parameters this client sends by default: ACP v1, text prompts, no filesystem access. */
  defaultInitializeParams(): InitializeParams {
    const params: InitializeParams = {
      protocolVersion: this.#version,
      clientCapabilities: this.#capabilities ?? {
        fs: { readTextFile: false, writeTextFile: false },
      },
    };
    if (this.#clientInfo) params.clientInfo = this.#clientInfo;
    return params;
  }

  initialize(params: InitializeParams): Promise<InitializeResult> {
    return this.request(AgentMethod.Initialize, params).then(decodeInitializeResult);
  }

  newSession(params: NewSessionParams): Promise<NewSessionResult> {
    return this.request(AgentMethod.NewSession, params).then(decodeNewSessionResult);
  }

  prompt(sessionId: string, prompt: ContentBlock[]): Promise<PromptResult> {
    return this.request(AgentMethod.Prompt, { sessionId, prompt }).then(decodePromptResult);
  }

  /** `session/cancel` is a notification: the agent answers through the prompt response. */
  cancel(sessionId: string): void {
    this.notify(AgentMethod.Cancel, { sessionId });
  }

  setMode(sessionId: string, modeId: string): Promise<unknown> {
    return this.request(AgentMethod.SetMode, { sessionId, modeId });
  }

  /**
   * Feed one inbound frame. Malformed JSON-RPC is dropped: there is no peer to
   * report it to, and one bad frame must not end the session.
   */
  receive(text: string): void {
    let message: RpcMessage;
    try {
      message = decodeMessage(text);
    } catch {
      return;
    }
    switch (message.kind) {
      case "result":
        this.#settle(message.id, (pending) => pending.resolve(message.result));
        return;
      case "error":
        this.#settle(message.id, (pending) =>
          pending.reject(
            new ProtocolError(message.error.code, message.error.message, message.error.data),
          ),
        );
        return;
      case "request":
        void this.#answer(message.id, message.method, message.params);
        return;
      case "notification":
        this.#notification(message.method, message.params);
        return;
    }
  }

  #settle(id: RequestId, apply: (pending: Pending) => void): void {
    const pending = this.#pending.get(id);
    if (!pending) return;
    this.#pending.delete(id);
    apply(pending);
  }

  #notification(method: string, params: unknown): void {
    const handler = this.#handlers.onSessionUpdate;
    if (method !== ClientMethod.SessionUpdate) return;
    if (!handler) return;
    handler(decodeSessionUpdate(isRecord(params) ? params.update : undefined));
  }

  async #answer(id: RequestId, method: string, params: unknown): Promise<void> {
    try {
      this.#channel.send(encodeResult(id, await this.#dispatch(method, params)));
    } catch (error) {
      this.#channel.send(encodeErrorResponse(id, asProtocolError(error)));
    }
  }

  #dispatch(method: string, params: unknown): Promise<unknown> {
    if (!Object.hasOwn(this.#routes, method)) return Promise.reject(noHandler(method));
    return this.#routes[method]!(params);
  }
}

function asProtocolError(error: unknown): ProtocolError {
  if (error instanceof ProtocolError) return error;
  return new ProtocolError(
    RpcErrorCode.InternalError,
    error instanceof Error ? error.message : String(error),
  );
}

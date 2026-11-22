import {
  array,
  arrayLenient,
  lenient,
  nullable,
  bool,
  num,
  object,
  oneOf,
  optional,
  str,
  tagged,
  type Decoder,
} from "../decode.ts";

/** The ACP v1 method names this client calls. */
export const AgentMethod = {
  Initialize: "initialize",
  Authenticate: "authenticate",
  NewSession: "session/new",
  LoadSession: "session/load",
  Prompt: "session/prompt",
  SetMode: "session/set_mode",
  SetConfigOption: "session/set_config_option",
  Cancel: "session/cancel",
} as const;

/** The ACP v1 methods an agent calls on this client. */
export const ClientMethod = {
  SessionUpdate: "session/update",
  RequestPermission: "session/request_permission",
  ReadTextFile: "fs/read_text_file",
  WriteTextFile: "fs/write_text_file",
  CreateTerminal: "terminal/create",
  TerminalOutput: "terminal/output",
  WaitForTerminalExit: "terminal/wait_for_exit",
  KillTerminal: "terminal/kill",
  ReleaseTerminal: "terminal/release",
} as const;

export const PROTOCOL_VERSION = 1;

export const STOP_REASONS = [
  "end_turn",
  "max_tokens",
  "max_turn_requests",
  "refusal",
  "cancelled",
] as const;

export type StopReason = (typeof STOP_REASONS)[number];

export const TOOL_KINDS = [
  "read",
  "edit",
  "delete",
  "move",
  "search",
  "execute",
  "think",
  "fetch",
  "other",
] as const;

export type ToolKind = (typeof TOOL_KINDS)[number];

export const TOOL_STATUSES = ["pending", "in_progress", "completed", "failed"] as const;

export type ToolStatus = (typeof TOOL_STATUSES)[number];

export const PERMISSION_KINDS = [
  "allow_once",
  "allow_always",
  "reject_once",
  "reject_always",
] as const;

export type PermissionKind = (typeof PERMISSION_KINDS)[number];

export const PLAN_STATUSES = ["pending", "in_progress", "completed"] as const;

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "resource_link"; uri: string; name: string }
  | { type: "resource"; resource: ResourceContents };

export type ResourceContents = { uri: string; text?: string; blob?: string; mimeType?: string };

export type ToolCallContent =
  | { type: "content"; content: ContentBlock }
  | { type: "diff"; path: string; oldText?: string | null; newText: string }
  | { type: "terminal"; terminalId: string };

export type ToolCallLocation = { path: string; line?: number | null };

export type ToolCall = {
  toolCallId: string;
  title: string;
  kind?: ToolKind;
  status?: ToolStatus;
};

export type ToolCallUpdate = {
  toolCallId: string;
  title?: string | null;
  kind?: ToolKind | null;
  status?: ToolStatus | null;
};

export type PlanEntry = { content: string; status: (typeof PLAN_STATUSES)[number] };

export type AvailableCommand = { name: string; description: string };

export type SessionUpdate =
  | { sessionUpdate: "user_message_chunk"; content: ContentBlock }
  | { sessionUpdate: "agent_message_chunk"; content: ContentBlock }
  | { sessionUpdate: "agent_thought_chunk"; content: ContentBlock }
  | { sessionUpdate: "tool_call"; toolCall: ToolCall }
  | { sessionUpdate: "tool_call_update"; toolCall: ToolCallUpdate }
  | { sessionUpdate: "plan"; entries: PlanEntry[] }
  | { sessionUpdate: "current_mode_update"; currentModeId: string }
  | { sessionUpdate: "available_commands_update"; availableCommands: AvailableCommand[] }
  | { sessionUpdate: "session_info_update"; title?: string | null; updatedAt?: string | null }
  | { sessionUpdate: "usage_update"; used: number; size: number };

export type Implementation = { name: string; version: string };

export type ClientCapabilities = {
  fs?: { readTextFile?: boolean; writeTextFile?: boolean };
  terminal?: boolean;
};

export type InitializeParams = {
  protocolVersion: number;
  clientCapabilities?: ClientCapabilities;
  clientInfo?: Implementation;
};

export type AgentCapabilities = {
  loadSession?: boolean;
  promptCapabilities?: { image?: boolean; audio?: boolean; embeddedContext?: boolean };
};

export type AuthMethod = { id: string; name: string; description?: string | null };

/** The agent's own name and version, which a capable agent states at `initialize`. */
export type AgentInfo = { name: string; version: string };

/**
 * One selectable session setting. Agents state their model choice this way in
 * `session/new`, and the list differs per agent, which is why the selected
 * model cannot be assumed.
 */
export type SessionConfigOptionValue = {
  value: string;
  name: string;
  description?: string | null;
};

export type SessionConfigOption = {
  id: string;
  name: string;
  description?: string | null;
  category?: string;
  type?: string;
  currentValue?: string;
  options?: SessionConfigOptionValue[];
};

/** The models a session may run, as an agent that names them states them. */
export type SessionModel = { modelId: string; name: string; description?: string | null };

export type SessionModels = {
  currentModelId: string;
  availableModels: SessionModel[];
};

export type InitializeResult = {
  protocolVersion: number;
  agentCapabilities?: AgentCapabilities;
  authMethods?: AuthMethod[];
  agentInfo?: AgentInfo;
};

export type SessionMode = { id: string; name: string; description?: string | null };

export type SessionModeState = { currentModeId: string; availableModes: SessionMode[] };

export type McpServer =
  | {
      type: "stdio";
      name: string;
      command: string;
      args?: string[];
      env?: Array<{ name: string; value: string }>;
    }
  | { type: "http"; name: string; url: string; headers?: Array<{ name: string; value: string }> }
  | { type: "sse"; name: string; url: string; headers?: Array<{ name: string; value: string }> };

/**
 * `mcpServers` is a list, but the shipped agents disagree on the empty form:
 * `opencode2 acp` rejects an array and requires a record, while
 * `grok agent stdio` rejects a record and requires an array. The empty value is
 * therefore configurable, so one gateway can serve either.
 */
export type NewSessionParams = {
  cwd: string;
  mcpServers?: McpServer[] | Readonly<Record<string, unknown>>;
};

/** How the empty `mcpServers` is sent, which differs by agent. */
export type McpServersShape = "array" | "record";

/** The empty `mcpServers` in the shape an agent accepts. */
export function emptyMcpServers(
  shape: McpServersShape,
): McpServer[] | Readonly<Record<string, unknown>> {
  return shape === "array" ? [] : {};
}

export type NewSessionResult = {
  sessionId: string;
  modes?: SessionModeState;
  /** Some agents (grok) state their models here rather than in `configOptions`. */
  models?: SessionModels;
  /** The selectable settings, including the model, as most agents state them. */
  configOptions?: SessionConfigOption[];
};

/**
 * The models an agent offers, from whichever field it stated them in.
 *
 * Agents disagree: grok returns `models` and also a `configOptions` entry whose
 * category is `model`, opencode2 returns only `configOptions`. Reading both
 * means a deployment learns the real choice either way, and the `model` option
 * wins because it is the field an agent that supports changing its model reads.
 */
export function offeredModels(result: NewSessionResult): SessionModels | undefined {
  const fromConfig = modelOption(result.configOptions);
  if (fromConfig !== undefined) return fromConfig;
  return result.models;
}

/** The model option of a session's settings, in the shape the session API uses. */
function modelOption(options: SessionConfigOption[] | undefined): SessionModels | undefined {
  if (options === undefined) return undefined;
  const option = options.find((candidate) => candidate.category === "model");
  if (option === undefined) return undefined;
  const availableModels = (option.options ?? []).map((value) => ({
    modelId: value.value,
    name: value.name,
    ...(value.description === undefined ? {} : { description: value.description }),
  }));
  if (availableModels.length === 0) return undefined;
  return { currentModelId: option.currentValue ?? availableModels[0]!.modelId, availableModels };
}

/** What a session can run: the model it would use and every model it offers. */
export type AgentCapability = {
  /** The agent's own name and version when it stated them. */
  agent?: AgentInfo;
  /** The model the agent would use with no further instruction. */
  currentModelId?: string;
  /** The models the agent offers, in the order it listed them. */
  availableModels: string[];
  /** Whether the agent states a model list at all, so a caller can tell "none" from "unknown". */
  statesModels: boolean;
};

/**
 * What one agent can do, as `initialize` and `session/new` reported it.
 *
 * A `stdio` agent is a process someone installed, and the models it can reach
 * depend on its own credentials: grok offers its own, opencode2 offers whatever
 * its config lists. The routing policy therefore cannot assume the model it
 * picked is available, so the gateway reports this and the control plane keeps
 * it with the session.
 */
export function agentCapabilityOf(
  initialized: InitializeResult,
  session: NewSessionResult | undefined,
): AgentCapability {
  const models = session === undefined ? undefined : offeredModels(session);
  return {
    ...(initialized.agentInfo === undefined ? {} : { agent: initialized.agentInfo }),
    ...(models === undefined ? {} : { currentModelId: models.currentModelId }),
    availableModels: models?.availableModels.map((model) => model.modelId) ?? [],
    statesModels: models !== undefined,
  };
}

/** Whether the capability states a model list that includes `modelId`. */
export function offersModel(capability: AgentCapability, modelId: string): boolean {
  if (!capability.statesModels) return false;
  return capability.availableModels.includes(modelId);
}

/**
 * The models a config option names, as the option value an agent matches.
 *
 * Agents qualify a model differently: grok lists bare ids (`grok-4.6`), while
 * opencode2 prefixes the provider (`ollama-cloud/deepseek-v4.1-flash`). A
 * routed model is a provider and an id, so both forms are compared, which lets
 * one selection reach either agent.
 */
export function modelValueFor(
  availableModels: readonly string[],
  model: { provider: string; modelId: string },
): string | undefined {
  const qualified = `${model.provider}/${model.modelId}`;
  if (availableModels.includes(qualified)) return qualified;
  if (availableModels.includes(model.modelId)) return model.modelId;
  return undefined;
}

/** A request that changes one session setting, such as its model. */
export type SetConfigOptionParams = {
  sessionId: string;
  configId: string;
  value: string;
};

export type SetConfigOptionResult = { configOptions?: SessionConfigOption[] };

export type PromptParams = { sessionId: string; prompt: ContentBlock[] };

export type PromptResult = { stopReason: StopReason };

export type PermissionOption = { optionId: string; name: string; kind: PermissionKind };

export type RequestPermissionParams = {
  sessionId: string;
  toolCall: ToolCallUpdate;
  options: PermissionOption[];
};

export type RequestPermissionResult =
  { outcome: "selected"; optionId: string } | { outcome: "cancelled" };

export type ReadTextFileParams = { sessionId: string; path: string; line?: number; limit?: number };

export type ReadTextFileResult = { content: string };

export type WriteTextFileParams = { sessionId: string; path: string; content: string };

export type CreateTerminalParams = {
  sessionId: string;
  command: string;
  args?: string[];
  cwd?: string;
  env?: Array<{ name: string; value: string }>;
  outputByteLimit?: number;
};

export type CreateTerminalResult = { terminalId: string };

export type TerminalRefParams = { sessionId: string; terminalId: string };

export type TerminalOutputResult = {
  output: string;
  truncated: boolean;
  exitStatus?: { exitCode: number | null; signal: string | null };
};

export type WaitForTerminalExitResult = { exitCode: number | null; signal: string | null };

const resourceContents: Decoder<ResourceContents> = (value) =>
  object({
    uri: str,
    text: optional(str),
    blob: optional(str),
    mimeType: optional(str),
  })(value);

export const decodeContentBlock: Decoder<ContentBlock> = tagged("type", {
  text: object({ type: oneOf("text"), text: str }),
  resource_link: object({ type: oneOf("resource_link"), uri: str, name: str }),
  resource: object({ type: oneOf("resource"), resource: resourceContents }),
});

export const decodeContentBlocks = array(decodeContentBlock);

export const decodeToolCall: Decoder<ToolCall> = object({
  toolCallId: str,
  title: str,
  kind: optional(lenient(oneOf(...TOOL_KINDS))),
  status: optional(lenient(oneOf(...TOOL_STATUSES))),
});

export const decodeToolCallUpdate: Decoder<ToolCallUpdate> = object({
  toolCallId: str,
  title: optional(nullable(str)),
  kind: optional(nullable(lenient(oneOf(...TOOL_KINDS)))),
  status: optional(nullable(lenient(oneOf(...TOOL_STATUSES)))),
});

const decodePlanEntry: Decoder<PlanEntry> = object({
  content: str,
  status: oneOf(...PLAN_STATUSES),
});

const decodeAvailableCommand: Decoder<AvailableCommand> = object({ name: str, description: str });

export const decodeSessionUpdate: Decoder<SessionUpdate> = tagged("sessionUpdate", {
  user_message_chunk: object({
    sessionUpdate: oneOf("user_message_chunk"),
    content: decodeContentBlock,
  }),
  agent_message_chunk: object({
    sessionUpdate: oneOf("agent_message_chunk"),
    content: decodeContentBlock,
  }),
  agent_thought_chunk: object({
    sessionUpdate: oneOf("agent_thought_chunk"),
    content: decodeContentBlock,
  }),
  tool_call: object({ sessionUpdate: oneOf("tool_call"), toolCall: decodeToolCall }),
  tool_call_update: object({
    sessionUpdate: oneOf("tool_call_update"),
    toolCall: decodeToolCallUpdate,
  }),
  plan: object({ sessionUpdate: oneOf("plan"), entries: array(decodePlanEntry) }),
  current_mode_update: object({ sessionUpdate: oneOf("current_mode_update"), currentModeId: str }),
  available_commands_update: object({
    sessionUpdate: oneOf("available_commands_update"),
    availableCommands: array(decodeAvailableCommand),
  }),
  session_info_update: object({
    sessionUpdate: oneOf("session_info_update"),
    title: optional(nullable(str)),
    updatedAt: optional(nullable(str)),
  }),
  usage_update: object({ sessionUpdate: oneOf("usage_update"), used: num, size: num }),
});

const decodeAuthMethod: Decoder<AuthMethod> = object({
  id: str,
  name: str,
  description: optional(nullable(str)),
});

const decodeAgentCapabilities: Decoder<AgentCapabilities> = object({
  loadSession: optional(bool),
  promptCapabilities: optional(
    object({
      image: optional(bool),
      audio: optional(bool),
      embeddedContext: optional(bool),
    }),
  ),
});

const decodeAgentInfo: Decoder<AgentInfo> = object({ name: str, version: str });

export const decodeInitializeResult: Decoder<InitializeResult> = object({
  protocolVersion: num,
  agentCapabilities: optional(decodeAgentCapabilities),
  authMethods: optional(array(decodeAuthMethod)),
  agentInfo: optional(lenient(decodeAgentInfo)),
});

const decodeSessionMode: Decoder<SessionMode> = object({
  id: str,
  name: str,
  description: optional(nullable(str)),
});

const decodeSessionModeState: Decoder<SessionModeState> = object({
  currentModeId: str,
  availableModes: arrayLenient(decodeSessionMode),
});

export const decodeConfigValue: Decoder<SessionConfigOptionValue> = object({
  value: str,
  name: str,
  description: optional(nullable(str)),
});

const decodeConfigOption: Decoder<SessionConfigOption> = object({
  id: str,
  name: str,
  description: optional(nullable(str)),
  category: optional(str),
  type: optional(str),
  currentValue: optional(str),
  options: optional(arrayLenient(decodeConfigValue)),
});

const decodeSessionModel: Decoder<SessionModel> = object({
  modelId: str,
  name: str,
  description: optional(nullable(str)),
});

const decodeSessionModels: Decoder<SessionModels> = object({
  currentModelId: str,
  availableModels: arrayLenient(decodeSessionModel),
});

export const decodeNewSessionResult: Decoder<NewSessionResult> = object({
  sessionId: str,
  modes: optional(lenient(decodeSessionModeState)),
  models: optional(lenient(decodeSessionModels)),
  configOptions: optional(arrayLenient(decodeConfigOption)),
});

export const decodeSetConfigOptionResult: Decoder<SetConfigOptionResult> = object({
  configOptions: optional(arrayLenient(decodeConfigOption)),
});

export const decodePromptResult: Decoder<PromptResult> = object({
  stopReason: oneOf(...STOP_REASONS),
});

const decodePermissionOption: Decoder<PermissionOption> = object({
  optionId: str,
  name: str,
  kind: oneOf(...PERMISSION_KINDS),
});

export const decodeRequestPermissionParams: Decoder<RequestPermissionParams> = object({
  sessionId: str,
  toolCall: decodeToolCallUpdate,
  options: array(decodePermissionOption),
});

export const decodeReadTextFileParams: Decoder<ReadTextFileParams> = object({
  sessionId: str,
  path: str,
  line: optional(num),
  limit: optional(num),
});

export const decodeWriteTextFileParams: Decoder<WriteTextFileParams> = object({
  sessionId: str,
  path: str,
  content: str,
});

const decodeEnv = array(object({ name: str, value: str }));

export const decodeCreateTerminalParams: Decoder<CreateTerminalParams> = object({
  sessionId: str,
  command: str,
  args: optional(array(str)),
  cwd: optional(str),
  env: optional(decodeEnv),
  outputByteLimit: optional(num),
});

export const decodeTerminalRefParams: Decoder<TerminalRefParams> = object({
  sessionId: str,
  terminalId: str,
});

const decodeExitStatus: Decoder<{ exitCode: number | null; signal: string | null }> = object({
  exitCode: nullable(num),
  signal: nullable(str),
});

export const decodeTerminalOutputResult: Decoder<TerminalOutputResult> = object({
  output: str,
  truncated: bool,
  exitStatus: optional(decodeExitStatus),
});

export function isStopReason(value: unknown): value is StopReason {
  return (STOP_REASONS as readonly string[]).includes(String(value));
}

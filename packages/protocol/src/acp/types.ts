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

export type InitializeResult = {
  protocolVersion: number;
  agentCapabilities?: AgentCapabilities;
  authMethods?: AuthMethod[];
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
 * `mcpServers` is a list, but ACP clients in the wild send a record keyed by
 * server name and at least one agent requires the field with either shape.
 */
export type NewSessionParams = {
  cwd: string;
  mcpServers?: McpServer[] | Readonly<Record<string, unknown>>;
};

export type NewSessionResult = { sessionId: string; modes?: SessionModeState };

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

export const decodeInitializeResult: Decoder<InitializeResult> = object({
  protocolVersion: num,
  agentCapabilities: optional(decodeAgentCapabilities),
  authMethods: optional(array(decodeAuthMethod)),
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

export const decodeNewSessionResult: Decoder<NewSessionResult> = object({
  sessionId: str,
  modes: optional(lenient(decodeSessionModeState)),
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

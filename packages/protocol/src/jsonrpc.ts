export type RequestId = string | number;

/** The standard codes JSON-RPC defines, plus ACP's reserved range. */
export const RpcErrorCode = {
  ParseError: -32700,
  InvalidRequest: -32600,
  MethodNotFound: -32601,
  InvalidParams: -32602,
  InternalError: -32603,
  /** ACP: the agent cannot serve the request because an auth step is missing. */
  AuthRequired: -32000,
  /** ACP: the named resource does not exist. */
  ResourceNotFound: -32002,
} as const;

export type RpcErrorShape = { code: number; message: string; data?: unknown };

/** Why a frame or an envelope was rejected. The text is the diagnostic a caller reads. */
export const Rejection = {
  InvalidJson: "invalid JSON",
  NotAnObject: "message is not an object",
  WrongVersion: 'jsonrpc must be "2.0"',
  BadId: "id must be a non-empty string or a number",
  MethodAndResult: "a message has a method or a result, not both",
  ResultAndError: "a response has a result or an error, not both",
  ResultWithoutId: "a result needs an id",
  ErrorWithoutId: "an error needs an id",
  ErrorShape: "error needs a numeric code and a message",
  NeitherMethodNorResult: "message has neither a method nor a result",
} as const;

export type RejectionReason = keyof typeof Rejection;

export type RpcRequest = { kind: "request"; id: RequestId; method: string; params: unknown };

export type RpcNotification = { kind: "notification"; method: string; params: unknown };

export type RpcSuccess = { kind: "result"; id: RequestId; result: unknown };

export type RpcFailure = { kind: "error"; id: RequestId; error: RpcErrorShape };

export type RpcMessage = RpcRequest | RpcNotification | RpcSuccess | RpcFailure;

export class ProtocolError extends Error {
  readonly code: number;
  readonly data: unknown;

  constructor(code: number, message: string, data?: unknown) {
    super(message);
    this.name = "ProtocolError";
    this.code = code;
    this.data = data;
  }
}

export function reject(reason: RejectionReason, data?: unknown): ProtocolError {
  const code = reason === "InvalidJson" ? RpcErrorCode.ParseError : RpcErrorCode.InvalidRequest;
  return new ProtocolError(code, Rejection[reason], data);
}

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

function isRequestId(value: unknown): value is RequestId {
  switch (typeof value) {
    case "number":
      return Number.isFinite(value);
    case "string":
      return value.length > 0;
    default:
      return false;
  }
}

function withData(shape: RpcErrorShape, data: unknown): RpcErrorShape {
  return data === undefined ? shape : { ...shape, data };
}

function encodeError(error: unknown): RpcErrorShape {
  if (isRecord(error) && typeof error.code === "number" && typeof error.message === "string") {
    return withData({ code: error.code, message: error.message }, error.data);
  }
  return {
    code: RpcErrorCode.InternalError,
    message: error instanceof Error ? error.message : String(error),
  };
}

export function encodeRequest(id: RequestId, method: string, params: unknown): string {
  return JSON.stringify({ jsonrpc: "2.0", id, method, params: params ?? null });
}

export function encodeNotification(method: string, params: unknown): string {
  return JSON.stringify({ jsonrpc: "2.0", method, params: params ?? null });
}

export function encodeResult(id: RequestId, result: unknown): string {
  return JSON.stringify({ jsonrpc: "2.0", id, result: result ?? null });
}

export function encodeErrorResponse(id: RequestId, error: unknown): string {
  return JSON.stringify({ jsonrpc: "2.0", id, error: encodeError(error) });
}

export function decodeMessage(text: string): RpcMessage {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw reject("InvalidJson");
  }
  return parseMessage(value);
}

function parseErrorShape(value: unknown): RpcErrorShape {
  if (!isRecord(value) || typeof value.code !== "number" || typeof value.message !== "string") {
    throw reject("ErrorShape");
  }
  return withData({ code: value.code, message: value.message }, value.data);
}

export function parseMessage(value: unknown): RpcMessage {
  if (!isRecord(value)) throw reject("NotAnObject");
  if (value.jsonrpc !== "2.0") throw reject("WrongVersion");
  const hasId = "id" in value;
  if (hasId && !isRequestId(value.id)) throw reject("BadId");

  const method = value.method;
  const hasResult = "result" in value;
  const hasError = "error" in value;
  const isCall = typeof method === "string" && method.length > 0;

  if (isCall && (hasResult || hasError)) throw reject("MethodAndResult");
  if (hasResult && hasError) throw reject("ResultAndError");
  if (hasResult && !hasId) throw reject("ResultWithoutId");
  if (hasError && !hasId) throw reject("ErrorWithoutId");

  const params = value.params ?? null;
  if (isCall) {
    return hasId
      ? { kind: "request", id: value.id as RequestId, method: method as string, params }
      : { kind: "notification", method: method as string, params };
  }
  if (hasResult) return { kind: "result", id: value.id as RequestId, result: value.result };
  if (hasError) {
    return { kind: "error", id: value.id as RequestId, error: parseErrorShape(value.error) };
  }
  throw reject("NeitherMethodNorResult");
}

export { isRecord, isRequestId };

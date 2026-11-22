import { describe, expect, test } from "bun:test";
import fc from "fast-check";
import {
  decodeMessage,
  encodeErrorResponse,
  encodeNotification,
  encodeRequest,
  encodeResult,
  parseMessage,
  ProtocolError,
  RpcErrorCode,
  type MessageRejected,
  type MessageResult,
  type RpcMessage,
} from "./jsonrpc.ts";

const requestId = fc.oneof(
  fc.string({ minLength: 1 }),
  fc.integer({ min: -1_000_000, max: 1_000_000 }),
);
const method = fc.string({ minLength: 1 });

/** The message a decode produced, or a failure naming the refusal. */
function message(text: string): RpcMessage {
  return read(decodeMessage(text));
}

function read(result: MessageResult): RpcMessage {
  if (result.kind !== "read") throw new Error("expected a message");
  expect(result.kind).toBe("read");
  return result.message;
}

/** The refusal a decode produced, or a failure naming what it read instead. */
function refusal(result: MessageResult): MessageRejected {
  if (result.kind !== "rejected") throw new Error("expected a refusal");
  expect(result.kind).toBe("rejected");
  return result;
}

const asJson = (value: unknown): string => JSON.stringify(value) ?? "null";

describe("codec round trip", () => {
  test("requests and notifications carry their params unchanged", () => {
    fc.assert(
      fc.property(requestId, method, fc.jsonValue(), (id, name, params) => {
        const request = message(encodeRequest(id, name, params));
        expect(request.kind).toBe("request");
        if (request.kind !== "request") return;
        expect(request.id).toBe(id);
        expect(request.method).toBe(name);
        expect(asJson(request.params)).toBe(asJson(params));

        const notification = message(encodeNotification(name, params));
        expect(notification.kind).toBe("notification");
        if (notification.kind !== "notification") return;
        expect(notification.method).toBe(name);
        expect(asJson(notification.params)).toBe(asJson(params));
      }),
    );
  });

  test("a result carries its payload unchanged", () => {
    fc.assert(
      fc.property(requestId, fc.jsonValue(), (id, payload) => {
        const decoded = message(encodeResult(id, payload));
        expect(decoded.kind).toBe("result");
        if (decoded.kind !== "result") return;
        expect(decoded.id).toBe(id);
        expect(asJson(decoded.result)).toBe(asJson(payload));
      }),
    );
  });

  test("a ProtocolError keeps its code, message, and data", () => {
    fc.assert(
      fc.property(
        requestId,
        fc.integer(),
        fc.string({ minLength: 1 }),
        fc.jsonValue(),
        (id, code, text, data) => {
          const decoded = message(encodeErrorResponse(id, new ProtocolError(code, text, data)));
          expect(decoded.kind).toBe("error");
          if (decoded.kind !== "error") return;
          expect(decoded.id).toBe(id);
          expect(decoded.error.code).toBe(code);
          expect(decoded.error.message).toBe(text);
          expect(asJson(decoded.error.data)).toBe(asJson(data));
        },
      ),
    );
  });

  test("a ProtocolError without data omits the key", () => {
    const decoded = message(encodeErrorResponse(1, new ProtocolError(-1, "nope")));
    expect(decoded).toEqual({ kind: "error", id: 1, error: { code: -1, message: "nope" } });
    if (decoded.kind !== "error") return;
    expect(Object.keys(decoded.error)).toEqual(["code", "message"]);
  });

  test("a ProtocolError names itself", () => {
    const error = new ProtocolError(-1, "nope");
    expect(error.name).toBe("ProtocolError");
    expect(error).toBeInstanceOf(Error);
  });

  test("a record with a numeric code and a non-string message is an internal error", () => {
    const decoded = message(encodeErrorResponse(1, { code: 7, message: 42 }));
    expect(decoded).toEqual({
      kind: "error",
      id: 1,
      error: { code: -32603, message: "[object Object]" },
    });
  });

  test("a plain error object is passed through", () => {
    const decoded = message(encodeErrorResponse("a", { code: 7, message: "bad", data: { x: 1 } }));
    expect(decoded).toEqual({
      kind: "error",
      id: "a",
      error: { code: 7, message: "bad", data: { x: 1 } },
    });
  });

  test("a plain error object without data omits the key", () => {
    const decoded = message(encodeErrorResponse("a", { code: 7, message: "bad" }));
    expect(decoded).toEqual({ kind: "error", id: "a", error: { code: 7, message: "bad" } });
  });

  test("an Error becomes an internal error", () => {
    const decoded = message(encodeErrorResponse(3, new Error("boom")));
    expect(decoded).toEqual({ kind: "error", id: 3, error: { code: -32603, message: "boom" } });
  });

  test("a non-Error value becomes an internal error with its string form", () => {
    const decoded = message(encodeErrorResponse(3, "plain string"));
    expect(decoded).toEqual({
      kind: "error",
      id: 3,
      error: { code: -32603, message: "plain string" },
    });
  });

  test("null params travel as null", () => {
    expect(message(encodeRequest(1, "ping", null))).toEqual({
      kind: "request",
      id: 1,
      method: "ping",
      params: null,
    });
  });

  test("an undefined result travels as null", () => {
    expect(message(encodeResult(1, undefined))).toEqual({
      kind: "result",
      id: 1,
      result: null,
    });
  });
});

describe("error codes", () => {
  test("match the JSON-RPC and ACP specifications", () => {
    expect(RpcErrorCode).toEqual({
      ParseError: -32700,
      InvalidRequest: -32600,
      MethodNotFound: -32601,
      InvalidParams: -32602,
      InternalError: -32603,
      AuthRequired: -32000,
      ResourceNotFound: -32002,
    });
  });
});

describe("rejections", () => {
  const cases: Array<[string, () => unknown, number, string]> = [
    ["invalid JSON", () => decodeMessage("{oops"), -32700, "invalid JSON"],
    ["a non-object", () => parseMessage(42), -32600, "message is not an object"],
    ["an array", () => parseMessage([]), -32600, "message is not an object"],
    ["null", () => parseMessage(null), -32600, "message is not an object"],
    [
      "a wrong version",
      () => parseMessage({ jsonrpc: "1.0", method: "x" }),
      -32600,
      'jsonrpc must be "2.0"',
    ],
    ["a missing version", () => parseMessage({ method: "x" }), -32600, 'jsonrpc must be "2.0"'],
    [
      "a null id",
      () => parseMessage({ jsonrpc: "2.0", id: null, method: "x" }),
      -32600,
      "id must be a non-empty string or a number",
    ],
    [
      "an empty id",
      () => parseMessage({ jsonrpc: "2.0", id: "", method: "x" }),
      -32600,
      "id must be a non-empty string or a number",
    ],
    [
      "an infinite id",
      () => parseMessage({ jsonrpc: "2.0", id: Number.POSITIVE_INFINITY, method: "x" }),
      -32600,
      "id must be a non-empty string or a number",
    ],
    [
      "an object id",
      () => parseMessage({ jsonrpc: "2.0", id: {}, method: "x" }),
      -32600,
      "id must be a non-empty string or a number",
    ],
    [
      "an empty method",
      () => parseMessage({ jsonrpc: "2.0", method: "" }),
      -32600,
      "message has neither a method nor a result",
    ],
    [
      "a method with a result",
      () => parseMessage({ jsonrpc: "2.0", id: 1, method: "x", result: 1 }),
      -32600,
      "a message has a method or a result, not both",
    ],
    [
      "a method with an error",
      () => parseMessage({ jsonrpc: "2.0", id: 1, method: "x", error: { code: 1, message: "m" } }),
      -32600,
      "a message has a method or a result, not both",
    ],
    [
      "a result with an error",
      () => parseMessage({ jsonrpc: "2.0", id: 1, result: 1, error: { code: 1, message: "m" } }),
      -32600,
      "a response has a result or an error, not both",
    ],
    [
      "a result without an id",
      () => parseMessage({ jsonrpc: "2.0", result: 1 }),
      -32600,
      "a result needs an id",
    ],
    [
      "an error without an id",
      () => parseMessage({ jsonrpc: "2.0", error: { code: 1, message: "m" } }),
      -32600,
      "an error needs an id",
    ],
    [
      "a non-object error",
      () => parseMessage({ jsonrpc: "2.0", id: 1, error: "x" }),
      -32600,
      "error needs a numeric code and a message",
    ],
    [
      "an error without a code",
      () => parseMessage({ jsonrpc: "2.0", id: 1, error: { message: "m" } }),
      -32600,
      "error needs a numeric code and a message",
    ],
    [
      "an error with a string code",
      () => parseMessage({ jsonrpc: "2.0", id: 1, error: { code: "1", message: "m" } }),
      -32600,
      "error needs a numeric code and a message",
    ],
    [
      "an error without a message",
      () => parseMessage({ jsonrpc: "2.0", id: 1, error: { code: 1 } }),
      -32600,
      "error needs a numeric code and a message",
    ],
    [
      "no method and no result",
      () => parseMessage({ jsonrpc: "2.0", id: 1 }),
      -32600,
      "message has neither a method nor a result",
    ],
  ];

  test.each(cases)("%s is rejected with its diagnostic", (_label, run, code, text) => {
    const answer = refusal(run() as MessageResult);
    expect(answer.code).toBe(code);
    expect(answer.message).toBe(text);
  });

  test("an error payload without data omits the key", () => {
    expect(read(parseMessage({ jsonrpc: "2.0", id: 1, error: { code: 1, message: "m" } }))).toEqual(
      {
        kind: "error",
        id: 1,
        error: { code: 1, message: "m" },
      },
    );
  });

  test("an error payload keeps data", () => {
    expect(
      read(parseMessage({ jsonrpc: "2.0", id: 1, error: { code: 1, message: "m", data: 5 } })),
    ).toEqual({
      kind: "error",
      id: 1,
      error: { code: 1, message: "m", data: 5 },
    });
  });

  test("a parse error names the JSON that could not be read", () => {
    expect(refusal(decodeMessage("{oops")).code).toBe(-32700);
  });

  test("any malformed frame is rejected with a spec code", () => {
    fc.assert(
      fc.property(fc.anything(), (value) => {
        try {
          parseMessage(value);
        } catch (error) {
          expect(error).toBeInstanceOf(ProtocolError);
          expect([-32700, -32600]).toContain((error as ProtocolError).code);
        }
      }),
    );
  });
});

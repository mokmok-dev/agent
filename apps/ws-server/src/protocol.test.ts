import { describe, expect, it } from "vitest";
import fc from "fast-check";
import { Effect } from "effect";

import { decodeClientMessage, encodeServerMessage, rejected, replyTo } from "./protocol.js";
import type { ClientMessage } from "./protocol.js";

const echo = (text: string): ClientMessage => ({ _tag: "Echo", text });
const ping = (id: number): ClientMessage => ({ _tag: "Ping", id });

const clientMessage = fc.oneof(
  fc.record({ _tag: fc.constant("Echo" as const), text: fc.string() }),
  fc.record({
    _tag: fc.constant("Ping" as const),
    id: fc.integer({ min: Number.MIN_SAFE_INTEGER, max: Number.MAX_SAFE_INTEGER }),
  }),
);

const parsesAsClientMessage = (raw: string): boolean => {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return false;
  }
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const candidate = value as Record<string, unknown>;
  if (candidate._tag === "Echo") {
    return typeof candidate.text === "string";
  }
  if (candidate._tag === "Ping") {
    return Number.isSafeInteger(candidate.id);
  }
  return false;
};

describe("protocol", () => {
  it("encodes each server message as one JSON document", () => {
    expect(encodeServerMessage({ _tag: "Echo", text: "hi" })).toBe('{"_tag":"Echo","text":"hi"}');
    expect(encodeServerMessage({ _tag: "Pong", id: 7 })).toBe('{"_tag":"Pong","id":7}');
    expect(encodeServerMessage(rejected)).toBe('{"_tag":"Rejected","reason":"invalid message"}');
  });

  it("replies to an Echo by echoing its text", () => {
    expect(replyTo(echo("hi"))).toEqual({ _tag: "Echo", text: "hi" });
  });

  it("replies to a Ping with a Pong carrying the same id", () => {
    expect(replyTo(ping(7))).toEqual({ _tag: "Pong", id: 7 });
  });

  it("decodes what JSON.stringify writes for every client message", () => {
    fc.assert(
      fc.property(clientMessage, (message) => {
        const result = Effect.runSync(Effect.result(decodeClientMessage(JSON.stringify(message))));
        expect(result._tag).toBe("Success");
        if (result._tag === "Success") {
          expect(result.success).toEqual(message);
        }
      }),
    );
  });

  it("rejects every string that is not a client message with ProtocolError", () => {
    fc.assert(
      fc.property(fc.string(), (raw) => {
        fc.pre(!parsesAsClientMessage(raw));
        const result = Effect.runSync(Effect.result(decodeClientMessage(raw)));
        expect(result._tag).toBe("Failure");
        if (result._tag === "Failure") {
          expect(result.failure._tag).toBe("ProtocolError");
          expect(result.failure.raw).toBe(raw);
          expect(result.failure.cause).toBeDefined();
        }
      }),
    );
  });
});

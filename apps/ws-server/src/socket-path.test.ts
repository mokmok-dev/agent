import { describe, expect, it } from "vitest";
import fc from "fast-check";
import { Effect } from "effect";

import { configKey, decodeSocketPath, SocketPath, socketPathUrl } from "./socket-path.js";

const decodes = (input: string): boolean =>
  Effect.runSync(Effect.result(decodeSocketPath(input)))._tag === "Success";

const grammar = (input: string): boolean =>
  /^\/[A-Za-z0-9._/-]+$/.test(input) && input.length <= 103;

const grammarAlphabet = fc.constantFrom("/", "a", "Z", "0", ".", "_", "-", ":", "?", "#", "%", " ");
const nearMiss = fc
  .array(grammarAlphabet, { minLength: 0, maxLength: 8 })
  .map((characters) => characters.join(""));

describe("socket path", () => {
  it("names the configuration variable WS_SOCKET_PATH", () => {
    expect(configKey).toBe("WS_SOCKET_PATH");
  });

  it("brands the socket path schema with the identifier SocketPath", () => {
    expect(SocketPath.identifier).toBe("SocketPath");
  });

  it("accepts exactly the paths the socket grammar and byte bound allow", () => {
    fc.assert(
      fc.property(fc.oneof(fc.string(), nearMiss), (input) => {
        expect(decodes(input)).toBe(grammar(input));
      }),
    );
  });

  it("decodes every generated path that follows the grammar", () => {
    fc.assert(
      fc.property(fc.stringMatching(/^\/[A-Za-z0-9._/-]{1,102}$/), (input) => {
        const result = Effect.runSync(Effect.result(decodeSocketPath(input)));
        expect(result._tag).toBe("Success");
        if (result._tag === "Success") {
          expect(result.success).toBe(input);
        }
      }),
    );
  });

  it("reports the rejected input and a cause on failure", () => {
    const result = Effect.runSync(Effect.result(decodeSocketPath("relative")));
    expect(result._tag).toBe("Failure");
    if (result._tag === "Failure") {
      expect(result.failure._tag).toBe("SocketPathError");
      expect(result.failure.input).toBe("relative");
      expect(result.failure.cause).toBeDefined();
    }
  });

  it("accepts a path of 103 bytes and rejects one of 104", () => {
    expect(decodes("/" + "a".repeat(102))).toBe(true);
    expect(decodes("/" + "a".repeat(103))).toBe(false);
  });

  it("rejects an empty path, a relative path, and a bare slash", () => {
    expect(decodes("")).toBe(false);
    expect(decodes("relative")).toBe(false);
    expect(decodes("/")).toBe(false);
  });

  it("rejects a path that does not start at the root", () => {
    expect(decodes("a/b")).toBe(false);
    expect(decodes("relative/absolute")).toBe(false);
  });

  it("rejects a path with a character outside the grammar", () => {
    expect(decodes("/tmp/a!")).toBe(false);
  });

  it("accepts dots, underscores, and hyphens in a path", () => {
    expect(decodes("/tmp/a.b_c-d")).toBe(true);
  });

  it("rejects a path containing a colon, question mark, hash, percent, or space", () => {
    expect(decodes("/tmp/a:b")).toBe(false);
    expect(decodes("/tmp/a?b")).toBe(false);
    expect(decodes("/tmp/a#b")).toBe(false);
    expect(decodes("/tmp/a%b")).toBe(false);
    expect(decodes("/tmp/a b")).toBe(false);
  });

  it("formats the socket path and request path into a ws+unix URL", () => {
    expect(socketPathUrl("/tmp/a.sock", "/ws")).toBe("ws+unix:///tmp/a.sock:/ws");
  });

  it("keeps the socket path and request path as the two colon-separated parts", () => {
    const noColon = fc.string({ minLength: 1 }).filter((part) => !part.includes(":"));
    fc.assert(
      fc.property(noColon, noColon, (path, requestPath) => {
        const tail = socketPathUrl(path, requestPath).replace("ws+unix://", "");
        expect(tail.split(":")).toHaveLength(2);
      }),
    );
  });
});

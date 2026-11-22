import { describe, expect, test } from "bun:test";

/**
 * The pi adapter is the piece that lets the gateway keep one wire protocol: the
 * gateway speaks ACP to every agent, and pi speaks its own RPC. These tests pin
 * the translation, driving the shipped adapter over real stdio.
 *
 * The turn test needs a real pi binary with a configured model, so it is
 * skipped when one is not present; the protocol test runs everywhere.
 */

const ADAPTER = new URL("./pi-acp.ts", import.meta.url).pathname;

/** Drive the adapter over stdio and collect its frames. */
async function drive(
  send: Array<Record<string, unknown>>,
  waitMs: number,
): Promise<Array<Record<string, unknown>>> {
  const child = Bun.spawn(["bun", ADAPTER], { stdin: "pipe", stdout: "pipe", stderr: "ignore" });
  const frames: Array<Record<string, unknown>> = [];
  const decoder = new TextDecoder();
  let buffer = "";
  void (async () => {
    for await (const chunk of child.stdout) {
      buffer += decoder.decode(chunk, { stream: true });
      let newline = buffer.indexOf("\n");
      while (newline >= 0) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        try {
          frames.push(JSON.parse(line) as Record<string, unknown>);
        } catch {
          // A partial frame is not a protocol record.
        }
        newline = buffer.indexOf("\n");
      }
    }
  })();
  for (const frame of send) {
    child.stdin.write(`${JSON.stringify(frame)}\n`);
    await child.stdin.flush();
  }
  await new Promise((resolve) => setTimeout(resolve, waitMs));
  child.kill();
  return frames;
}

describe("the pi adapter's protocol", () => {
  test("answers initialize with the ACP version and capabilities", async () => {
    const frames = await drive(
      [{ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: 1 } }],
      600,
    );
    expect(frames[0]).toEqual({
      jsonrpc: "2.0",
      id: 1,
      result: {
        protocolVersion: 1,
        agentCapabilities: { promptCapabilities: { embeddedContext: true, image: false } },
        // pi itself has no ACP identity; the adapter is what answers.
        agentInfo: { name: "pi-adapter", version: "0" },
      },
    });
  });

  test("a method it does not implement is a JSON-RPC method-not-found", async () => {
    const frames = await drive(
      [{ jsonrpc: "2.0", id: 9, method: "session/load", params: {} }],
      600,
    );
    expect(frames[0]).toMatchObject({ id: 9, error: { code: -32601 } });
  });

  test("a frame that is not JSON is a parse error without an id", async () => {
    const child = Bun.spawn(["bun", ADAPTER], { stdin: "pipe", stdout: "pipe", stderr: "ignore" });
    child.stdin.write("not json\n");
    await child.stdin.flush();
    const text = await new Promise<string>((resolve) => {
      const timer = setTimeout(() => resolve(""), 1500);
      void (async () => {
        const decoder = new TextDecoder();
        for await (const chunk of child.stdout) {
          clearTimeout(timer);
          resolve(decoder.decode(chunk));
          return;
        }
      })();
    });
    child.kill();
    expect(JSON.parse(text.trim())).toMatchObject({ error: { code: -32700 } });
  });

  test("a prompt for an unknown session is refused, not silently accepted", async () => {
    const frames = await drive(
      [
        {
          jsonrpc: "2.0",
          id: 2,
          method: "session/prompt",
          params: { sessionId: "nope", prompt: [{ type: "text", text: "hi" }] },
        },
      ],
      600,
    );
    expect(frames[0]).toMatchObject({ id: 2, error: { code: -32602 } });
  });
});

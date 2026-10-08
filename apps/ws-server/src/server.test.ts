import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import fc from "fast-check";
import { Effect, Logger, References } from "effect";
import * as NodeSocket from "@effect/platform-node/NodeSocket";
import * as Socket from "effect/socket/Socket";

import { serve } from "./server.js";
import { socketPathUrl } from "./socket-path.js";

let dir: string;
let socketPath: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "ws-server-"));
  socketPath = join(dir, "app.sock");
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

// A server that stops answering must fail a test, not hang the run. Every wait in
// this file is bounded, so the failure lands well inside Stryker's mutant timeout.
const bound = "1 second";

const httpGet = (path: string, method = "GET") =>
  Effect.suspend(() => {
    const req = request({ socketPath, path, method });
    const response = Effect.callback<{ readonly status: number; readonly body: string }>(
      (resume) => {
        req.on("response", (res) => {
          const chunks: Buffer[] = [];
          res.on("data", (chunk: Buffer) => chunks.push(chunk));
          res.on("end", () =>
            resume(
              Effect.succeed({
                status: res.statusCode ?? 0,
                body: Buffer.concat(chunks).toString(),
              }),
            ),
          );
        });
        req.on("error", (error) => resume(Effect.die(error)));
        req.end();
      },
    );
    // A request that outlives its effect keeps a connection open, and
    // `http.Server.close()` then waits for it on the way out of the test.
    return response.pipe(Effect.ensuring(Effect.sync(() => req.destroy())));
  }).pipe(Effect.timeout(bound));

const waitForReady = Effect.gen(function* () {
  let attempts = 0;
  while (attempts < 50) {
    const ready = yield* httpGet("/health").pipe(
      Effect.map((response) => response.status === 200),
      Effect.catchCause(() => Effect.succeed(false)),
    );
    if (ready) {
      return;
    }
    attempts += 1;
    yield* Effect.sleep("10 millis");
  }
  return yield* Effect.die(`server on ${socketPath} never became ready`);
}).pipe(Effect.timeout(bound));

const withServer = <A, E>(use: () => Effect.Effect<A, E>) =>
  Effect.scoped(
    Effect.gen(function* () {
      yield* Effect.forkScoped(serve({ socketPath }));
      yield* waitForReady;
      return yield* use();
    }),
  );

const exchange = (frames: readonly string[]) =>
  Effect.gen(function* () {
    const socket = yield* Socket.makeWebSocket(socketPathUrl(socketPath, "/ws"));
    return yield* Effect.scoped(
      Effect.gen(function* () {
        const writer = yield* socket.writer;
        const pull = yield* Socket.readerString(socket);
        const received: string[] = [];
        for (const frame of frames) {
          yield* writer.write(frame);
          received.push(...(yield* pull));
        }
        return received;
      }),
    );
  }).pipe(Effect.provide(NodeSocket.layerWebSocketConstructorWS), Effect.timeout(bound));

const killAfterBinding = (path: string) =>
  new Promise<void>((resolve, reject) => {
    const child = spawn(
      process.execPath,
      [
        "-e",
        `const net = require("node:net"); const server = net.createServer(); server.listen(${JSON.stringify(path)}, () => console.log("ready")); setInterval(() => {}, 1000);`,
      ],
      { stdio: ["ignore", "pipe", "inherit"] },
    );
    child.stdout.once("data", () => child.kill("SIGKILL"));
    child.once("exit", () => resolve());
    child.once("error", reject);
  });

const clientMessage = fc.oneof(
  fc.record({ _tag: fc.constant("Echo" as const), text: fc.string() }),
  fc.record({ _tag: fc.constant("Ping" as const), id: fc.integer() }),
);

const wireCase = fc.oneof(
  {
    weight: 3,
    arbitrary: clientMessage.map((message) => ({
      frame: JSON.stringify(message),
      reply:
        message._tag === "Echo"
          ? `{"_tag":"Echo","text":${JSON.stringify(message.text)}}`
          : `{"_tag":"Pong","id":${JSON.stringify(message.id)}}`,
    })),
  },
  {
    weight: 1,
    arbitrary: fc
      .string()
      .filter((text) => {
        try {
          JSON.parse(text);
          return false;
        } catch {
          return true;
        }
      })
      .map((frame) => ({
        frame,
        reply: '{"_tag":"Rejected","reason":"invalid message"}',
      })),
  },
);

describe("ws server", () => {
  it("answers a generated sequence of frames in order", async () => {
    await fc.assert(
      fc.asyncProperty(fc.array(wireCase, { minLength: 1, maxLength: 5 }), async (cases) => {
        const received = await Effect.runPromise(
          withServer(() => exchange(cases.map((one) => one.frame))),
        );
        expect(received).toEqual(cases.map((one) => one.reply));
      }),
      { numRuns: 10 },
    );
  });

  it("round-trips an Echo frame", async () => {
    const received = await Effect.runPromise(
      withServer(() => exchange(['{"_tag":"Echo","text":"hello"}'])),
    );
    expect(received).toEqual(['{"_tag":"Echo","text":"hello"}']);
  });

  it("answers a Ping frame with a Pong carrying the same id", async () => {
    const received = await Effect.runPromise(
      withServer(() => exchange(['{"_tag":"Ping","id":5}'])),
    );
    expect(received).toEqual(['{"_tag":"Pong","id":5}']);
  });

  it("rejects a malformed frame and keeps serving the connection", async () => {
    const received = await Effect.runPromise(
      withServer(() => exchange(["not json", '{"_tag":"Echo","text":"after"}'])),
    );
    expect(received).toEqual([
      '{"_tag":"Rejected","reason":"invalid message"}',
      '{"_tag":"Echo","text":"after"}',
    ]);
  });

  it("responds 200 ok on GET /health", async () => {
    const response = await Effect.runPromise(withServer(() => httpGet("/health")));
    expect(response).toEqual({ status: 200, body: "ok" });
  });

  it("responds 404 on POST /health", async () => {
    const response = await Effect.runPromise(withServer(() => httpGet("/health", "POST")));
    expect(response.status).toBe(404);
  });

  it("responds 426 on GET /ws without an upgrade header", async () => {
    const response = await Effect.runPromise(withServer(() => httpGet("/ws")));
    expect(response.status).toBe(426);
  });

  it("responds 404 on an unknown path", async () => {
    const response = await Effect.runPromise(withServer(() => httpGet("/nope")));
    expect(response.status).toBe(404);
  });

  it("fails with SocketPathError before binding an invalid path", async () => {
    const result = await Effect.runPromise(Effect.result(serve({ socketPath: "not-absolute" })));
    expect(result._tag).toBe("Failure");
    if (result._tag === "Failure") {
      expect(result.failure._tag).toBe("SocketPathError");
      if (result.failure._tag === "SocketPathError") {
        expect(result.failure.input).toBe("not-absolute");
      }
    }
  });

  it("fails to bind when the path is an existing regular file and leaves it untouched", async () => {
    writeFileSync(socketPath, "keep");
    const result = await Effect.runPromise(Effect.result(serve({ socketPath })));
    expect(result._tag).toBe("Failure");
    if (result._tag === "Failure") {
      expect(result.failure._tag).toBe("ServeError");
      if (result.failure._tag === "ServeError") {
        expect((result.failure.cause as NodeJS.ErrnoException).code).toBe("EADDRINUSE");
      }
    }
    expect(existsSync(socketPath)).toBe(true);
  });

  it("replaces a stale socket file left behind by a killed process", async () => {
    await killAfterBinding(socketPath);
    expect(statSync(socketPath, { throwIfNoEntry: false })?.isSocket()).toBe(true);

    const response = await Effect.runPromise(withServer(() => httpGet("/health")));
    expect(response).toEqual({ status: 200, body: "ok" });
  });

  it("removes the socket file when the server shuts down", async () => {
    await Effect.runPromise(withServer(() => httpGet("/health").pipe(Effect.asVoid)));
    expect(existsSync(socketPath)).toBe(false);
  });

  it("logs the unix address it is listening on", async () => {
    const messages: string[] = [];
    const collector = Logger.make(({ message }) => {
      messages.push(String(message));
    });
    await Effect.runPromise(
      withServer(() => Effect.void).pipe(Effect.provide(Logger.layer([collector]))),
    );
    expect(messages).toContain(`listening on unix://${socketPath}`);
  });

  it("reports a client disconnect and closes the connection quietly", async () => {
    const ended: string[] = [];
    const problems: string[] = [];
    const collector = Logger.make(({ logLevel, message }) => {
      const text = String(message);
      if (text.includes("websocket connection ended")) {
        ended.push(text);
      }
      if (logLevel === "Error" || logLevel === "Fatal" || logLevel === "Warn") {
        problems.push(text);
      }
    });
    await Effect.runPromise(
      withServer(() =>
        Effect.gen(function* () {
          yield* exchange(['{"_tag":"Echo","text":"bye"}']);
          yield* Effect.sleep("50 millis");
        }),
      ).pipe(
        Effect.provide(Logger.layer([collector])),
        Effect.provideService(References.MinimumLogLevel, "Debug"),
      ),
    );
    expect(ended).toHaveLength(1);
    expect(problems).toEqual([]);
  });
});

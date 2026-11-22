import { describe, expect, test } from "bun:test";
import { modelsFor, piRunnerFor, piRunnerOver } from "./pi-runtime.ts";
import type { Env } from "./worker.ts";

/**
 * These exercise the parts of the runtime that do not need workerd: the model
 * collection and the lazily opened conversation. `open` itself needs a Durable
 * Object's SQLite, so a turn through it is proven by the deployment, not here.
 */

const env = (values: Record<string, string> = {}): Env =>
  ({
    SLACK_SIGNING_SECRET: "",
    SLACK_BOT_USER_ID: "",
    DISCORD_PUBLIC_KEY: "",
    LINEAR_WEBHOOK_SECRET: "",
    LINEAR_CLIENT_ID: "",
    LINEAR_CLIENT_SECRET: "",
    LINEAR_REDIRECT_URI: "",
    SESSIONS: { getByName: () => ({}) },
    THREAD_INDEX: { getByName: () => ({}) },
    LINEAR_OAUTH: { getByName: () => ({}) },
    ...values,
  }) as Env;

describe("the model collection", () => {
  test("registers the Cloudflare Workers AI provider, so a turn has a model source", () => {
    const models = modelsFor(env());
    expect(models.getProvider("cloudflare-workers-ai")).toBeDefined();
    expect(models.getProvider("cloudflare-workers-ai")?.id).toBe("cloudflare-workers-ai");
  });

  test("reads the key through the Worker environment, which is where a key lives", async () => {
    const models = modelsFor(env({ CLOUDFLARE_API_KEY: "key-1", CLOUDFLARE_ACCOUNT_ID: "acct-1" }));
    const auth = (await models.checkAuth("cloudflare-workers-ai")) as {
      source?: string;
      type?: string;
    };
    expect(auth).toEqual({ source: "CLOUDFLARE_API_KEY", type: "api_key" });
  });

  test("reports no auth when the environment has no key, rather than pretending", async () => {
    expect(await modelsFor(env()).checkAuth("cloudflare-workers-ai")).toBeUndefined();
  });
});

describe("the runner", () => {
  test("is a runner whose conversation is opened lazily, so construction touches no storage", () => {
    const runner = piRunnerFor({ storage: {} } as never, env());
    expect(typeof runner.run).toBe("function");
  });

  test("a turn with no usable platform storage answers unanswered rather than throwing", async () => {
    const runner = piRunnerFor({ storage: {} } as never, env());
    const result = await runner.run({
      sessionId: "s",
      conversationId: "c",
      prompt: "hi",
      operationId: "o",
      model: { provider: "cloudflare-workers-ai", modelId: "m" },
      now: 0,
    });
    expect(result.kind).toBe("unanswered");
  });
});

describe("the conversation is opened once", () => {
  test("a second turn reuses the conversation the first turn opened", async () => {
    let opens = 0;
    const base = {
      setModel: async () => undefined,
      submit: async (_prompt: string, operationId: string) => ({
        kind: "done" as const,
        operationId,
        reason: undefined,
      }),
      lastReply: async () => "ok",
    } as never;
    const runner = piRunnerOver(async () => {
      opens += 1;
      return base;
    });
    const input = {
      sessionId: "s",
      conversationId: "c",
      prompt: "hi",
      operationId: "o",
      model: { provider: "p", modelId: "m" },
      now: 0,
    };
    await runner.run(input);
    await runner.run({ ...input, operationId: "o2" });
    expect(opens).toBe(1);
  });

  test("a factory that fails answers unanswered and is not retried by the cache", async () => {
    let opens = 0;
    const runner = piRunnerOver(async () => {
      opens += 1;
      throw new Error("storage is gone");
    });
    const input = {
      sessionId: "s",
      conversationId: "c",
      prompt: "hi",
      operationId: "o",
      model: { provider: "p", modelId: "m" },
      now: 0,
    };
    expect(await runner.run(input)).toEqual({ kind: "unanswered", reason: "storage is gone" });
    expect(opens).toBe(1);
  });
});

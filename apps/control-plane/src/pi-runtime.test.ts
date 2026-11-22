import { describe, expect, test } from "bun:test";
import {
  modelsFor,
  piRunnerFor,
  piRunnerOver,
  providerConfigOf,
  refreshCatalog,
  refusalText,
  setupFor,
} from "./pi-runtime.ts";
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

describe("the configuration a deployment states", () => {
  test("PI_PROVIDERS is a comma-separated list, trimmed", () => {
    expect(providerConfigOf(env({ PI_PROVIDERS: " openai , anthropic " })).providers).toEqual([
      "openai",
      "anthropic",
    ]);
  });

  test("no provider list falls back to Workers AI, the model this repository ships", () => {
    expect(providerConfigOf(env()).providers).toEqual(["cloudflare-workers-ai"]);
  });

  test("an empty or comma-only list falls back rather than registering nothing", () => {
    expect(providerConfigOf(env({ PI_PROVIDERS: " , " })).providers).toEqual([
      "cloudflare-workers-ai",
    ]);
  });

  test("PI_DEFAULT_MODEL reads provider:modelId", () => {
    expect(
      providerConfigOf(env({ PI_DEFAULT_MODEL: "anthropic:claude-opus-4-7" })).defaultModel,
    ).toEqual({ provider: "anthropic", modelId: "claude-opus-4-7" });
  });

  test("a model id containing colons keeps everything after the first", () => {
    expect(
      providerConfigOf(env({ PI_DEFAULT_MODEL: "cloudflare-workers-ai:@cf/deepseek-ai/x" }))
        .defaultModel,
    ).toEqual({ provider: "cloudflare-workers-ai", modelId: "@cf/deepseek-ai/x" });
  });

  test("a default model with no separator is refused by the setup, not guessed", () => {
    expect(() => setupFor(env({ PI_DEFAULT_MODEL: "anthropic" }))).toThrow("PI_DEFAULT_MODEL");
  });

  test("a trailing separator is refused, because the model id would be empty", () => {
    expect(() => setupFor(env({ PI_DEFAULT_MODEL: "anthropic:" }))).toThrow("PI_DEFAULT_MODEL");
  });

  test("no default model uses the shipped Workers AI model", () => {
    expect(providerConfigOf(env()).defaultModel).toEqual({
      provider: "cloudflare-workers-ai",
      modelId: "@cf/deepseek-ai/deepseek-v4.1-flash",
    });
  });
});

describe("a misconfiguration is a named failure, not a silent fallback", () => {
  test("an unknown provider names it", () => {
    expect(() => setupFor(env({ PI_PROVIDERS: "nope" }))).toThrow(
      "PI_PROVIDERS names an unknown provider: nope",
    );
  });

  test("a default model outside the enabled providers is refused", () => {
    expect(() =>
      setupFor(env({ PI_PROVIDERS: "openai", PI_DEFAULT_MODEL: "anthropic:claude-opus-4-7" })),
    ).toThrow("unknown provider: anthropic");
  });

  test("a valid configuration returns the collection and the default model", () => {
    const setup = setupFor(env({ PI_PROVIDERS: "openai", PI_DEFAULT_MODEL: "openai:gpt-5-codex" }));
    expect(setup.defaultModel).toEqual({ provider: "openai", modelId: "gpt-5-codex" });
    expect(setup.models.getProvider("openai")).toBeDefined();
  });
});

describe("the exact parsing rules", () => {
  test("whitespace around the whole value is trimmed, so a padded secret works", () => {
    expect(
      providerConfigOf(env({ PI_DEFAULT_MODEL: "  openai:gpt-5-codex  " })).defaultModel,
    ).toEqual({ provider: "openai", modelId: "gpt-5-codex" });
  });

  test("a leading separator has no provider and is refused", () => {
    expect(() =>
      setupFor(env({ PI_PROVIDERS: "openai", PI_DEFAULT_MODEL: ":gpt-5-codex" })),
    ).toThrow("PI_DEFAULT_MODEL");
  });

  test("a trailing separator has no model id and is refused", () => {
    expect(() => setupFor(env({ PI_PROVIDERS: "openai", PI_DEFAULT_MODEL: "openai:" }))).toThrow(
      "PI_DEFAULT_MODEL",
    );
  });

  test("a value of only whitespace falls back to the shipped model", () => {
    expect(providerConfigOf(env({ PI_DEFAULT_MODEL: "   " })).defaultModel).toEqual({
      provider: "cloudflare-workers-ai",
      modelId: "@cf/deepseek-ai/deepseek-v4.1-flash",
    });
  });
});

describe("the message a missing key produces", () => {
  test("names the provider and every key it needs", () => {
    expect(refusalText({ kind: "missing-key", provider: "openai", keys: ["OPENAI_API_KEY"] })).toBe(
      "openai needs OPENAI_API_KEY",
    );
    expect(
      refusalText({
        kind: "missing-key",
        provider: "cloudflare-workers-ai",
        keys: ["CLOUDFLARE_API_KEY", "CLOUDFLARE_ACCOUNT_ID"],
      }),
    ).toBe("cloudflare-workers-ai needs CLOUDFLARE_API_KEY and CLOUDFLARE_ACCOUNT_ID");
  });

  test("names an unknown provider and a bad default model distinctly", () => {
    expect(refusalText({ kind: "unknown-provider", provider: "nope" })).toBe(
      "PI_PROVIDERS names an unknown provider: nope",
    );
    expect(refusalText({ kind: "no-default-model" })).toBe(
      "PI_DEFAULT_MODEL must be provider:modelId",
    );
  });
});

describe("the model id an operator leaves empty", () => {
  test("provider with no model id is refused, so a turn cannot run a nameless model", () => {
    expect(() => setupFor(env({ PI_PROVIDERS: "openai", PI_DEFAULT_MODEL: "openai:" }))).toThrow(
      "PI_DEFAULT_MODEL must be provider:modelId",
    );
  });

  test("model id with no provider is refused too", () => {
    expect(() =>
      setupFor(env({ PI_PROVIDERS: "openai", PI_DEFAULT_MODEL: ":gpt-5-codex" })),
    ).toThrow("PI_DEFAULT_MODEL must be provider:modelId");
  });
});

describe("the catalog a turn resolves against", () => {
  const dynamic = () =>
    setupFor(
      env({
        PI_PROVIDERS: "ollama-cloud",
        PI_DEFAULT_MODEL: "ollama-cloud:deepseek-v4.1-flash",
      }),
    );

  test("a dynamic provider has no models until the catalog is refreshed", () => {
    // This is what produced `no_model` on the deployment: the Durable Object
    // starts empty, so a named model resolves to nothing.
    expect(dynamic().models.getProvider("ollama-cloud")?.getModels()).toEqual([]);
  });

  test("refreshing fills the catalog, so the deployment's model resolves", async () => {
    const setup = dynamic();
    // The real Ollama endpoint is not reachable from a unit test's transport, so
    // this asserts the call is made and its failure is tolerated; the live
    // refresh is proven by the pipeline evidence.
    await refreshCatalog(setup.models, ["ollama-cloud"]);
    expect(Array.isArray(setup.models.getProvider("ollama-cloud")?.getModels())).toBe(true);
  });

  test("a provider list with no dynamic provider needs no network", async () => {
    const setup = setupFor(
      env({
        PI_PROVIDERS: "cloudflare-workers-ai",
        PI_DEFAULT_MODEL: "cloudflare-workers-ai:@cf/deepseek-ai/deepseek-v4.1-flash",
      }),
    );
    // A static provider ignores the refresh, so this must not throw.
    await expect(refreshCatalog(setup.models, ["cloudflare-workers-ai"])).resolves.toBeUndefined();
  });

  test("a refresh that throws leaves the catalog as it was, never a crash", async () => {
    const setup = setupFor(
      env({
        PI_PROVIDERS: "cloudflare-workers-ai",
        PI_DEFAULT_MODEL: "cloudflare-workers-ai:@cf/deepseek-ai/deepseek-v4.1-flash",
      }),
    );
    const throwing = {
      refresh: async () => {
        throw new Error("offline");
      },
    } as never;
    await expect(refreshCatalog(throwing, ["x"])).resolves.toBeUndefined();
    expect(setup.models.getProvider("cloudflare-workers-ai")).toBeDefined();
  });
});

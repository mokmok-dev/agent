import { describe, expect, test } from "bun:test";
import {
  availableProviders,
  configureProviders,
  PROVIDER_KEYS,
  type ProviderConfig,
} from "./providers.ts";

const config = (overrides: Partial<ProviderConfig> = {}): ProviderConfig => ({
  providers: ["cloudflare-workers-ai"],
  defaultModel: {
    provider: "cloudflare-workers-ai",
    modelId: "@cf/deepseek-ai/deepseek-v4.1-flash",
  },
  ...overrides,
});

const envOf = (values: Record<string, string>) => (name: string) => values[name];

describe("the providers a deployment may register", () => {
  test("registers each named provider, so its models resolve", () => {
    const setup = configureProviders(
      config({ providers: ["cloudflare-workers-ai", "openai"] }),
      envOf({}),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(setup.models.getProvider("openai")).toBeDefined();
    expect(setup.models.getProvider("cloudflare-workers-ai")).toBeDefined();
  });

  test("a provider the deployment did not name has no reachable key", async () => {
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({ ANTHROPIC_API_KEY: "sk-ant-1" }),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("anthropic")).toBeUndefined();
  });

  test("a default model from a provider the deployment did not name is refused", () => {
    expect(
      configureProviders(
        config({
          providers: ["openai"],
          defaultModel: { provider: "anthropic", modelId: "claude-opus-4-7" },
        }),
        envOf({}),
      ),
    ).toEqual({
      kind: "refused",
      refusal: { kind: "unknown-provider", provider: "anthropic" },
    });
  });

  test("keeps the stated default model", () => {
    const model = { provider: "anthropic", modelId: "claude-opus-4-7" };
    const setup = configureProviders(
      config({ providers: ["anthropic"], defaultModel: model }),
      envOf({}),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(setup.defaultModel).toEqual(model);
  });

  test("refuses a provider pi does not know, naming it", () => {
    expect(configureProviders(config({ providers: ["not-a-provider"] }), envOf({}))).toEqual({
      kind: "refused",
      refusal: { kind: "unknown-provider", provider: "not-a-provider" },
    });
  });

  test("refuses an empty default model, because a turn would have nothing to run", () => {
    expect(
      configureProviders(config({ defaultModel: { provider: "", modelId: "" } }), envOf({})),
    ).toEqual({ kind: "refused", refusal: { kind: "no-default-model" } });
  });
});

describe("the keys a provider may read", () => {
  test("a key for an enabled provider reaches pi", async () => {
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({ OPENAI_API_KEY: "sk-1" }),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("openai")).toEqual({
      source: "OPENAI_API_KEY",
      type: "api_key",
    });
  });

  test("a key for a provider this deployment does not run is unreachable", async () => {
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({ OPENAI_API_KEY: "sk-1", ANTHROPIC_API_KEY: "sk-ant-1" }),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("anthropic")).toBeUndefined();
  });

  test("a missing key reads as unauthenticated rather than absent", async () => {
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      envOf({}),
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    expect(await setup.models.checkAuth("openai")).toBeUndefined();
  });
});

describe("the providers a deployment can actually run", () => {
  test("names only those whose keys are all set", () => {
    expect(
      availableProviders(
        ["openai", "anthropic", "cloudflare-workers-ai"],
        envOf({ OPENAI_API_KEY: "k" }),
      ),
    ).toEqual(["openai"]);
  });

  test("a provider with several keys needs all of them", () => {
    expect(
      availableProviders(["cloudflare-workers-ai"], envOf({ CLOUDFLARE_API_KEY: "k" })),
    ).toEqual([]);
    expect(
      availableProviders(
        ["cloudflare-workers-ai"],
        envOf({ CLOUDFLARE_API_KEY: "k", CLOUDFLARE_ACCOUNT_ID: "a" }),
      ),
    ).toEqual(["cloudflare-workers-ai"]);
  });

  test("an empty value counts as unset, because wrangler stores an empty secret that way", () => {
    expect(availableProviders(["openai"], envOf({ OPENAI_API_KEY: "" }))).toEqual([]);
  });

  test("an unknown provider has no keys and is not advertised as available", () => {
    expect(availableProviders(["nope"], envOf({}))).toEqual(["nope"]);
  });
});

describe("the documented key names", () => {
  test("each entry names at least one key", () => {
    for (const [provider, keys] of Object.entries(PROVIDER_KEYS)) {
      expect(keys.length).toBeGreaterThan(0);
      for (const key of keys) expect(key).toMatch(/^[A-Z][A-Z0-9_]*$/);
      expect(provider).toBe(provider.toLowerCase());
    }
  });

  test("the providers this repository ships a default for are configurable", () => {
    expect(PROVIDER_KEYS["cloudflare-workers-ai"]).toBeDefined();
  });
});

describe("the pieces a caller cannot reach", () => {
  test("the env reader only answers for keys an enabled provider declares", async () => {
    const asked: string[] = [];
    const setup = configureProviders(
      config({
        providers: ["openai"],
        defaultModel: { provider: "openai", modelId: "gpt-5-codex" },
      }),
      (name) => {
        asked.push(name);
        return name === "OPENAI_API_KEY" ? "sk" : undefined;
      },
    );
    if (setup.kind !== "configured") throw new Error("expected a configured collection");
    await setup.models.checkAuth("openai");
    expect(asked).toEqual(["OPENAI_API_KEY"]);
  });
});

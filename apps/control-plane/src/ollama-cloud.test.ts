import { describe, expect, test } from "bun:test";
import {
  OLLAMA_CLOUD_BASE_URL,
  OLLAMA_CLOUD_ID,
  OLLAMA_CLOUD_KEY,
  OLLAMA_CLOUD_MODELS,
  ollamaCloudProvider,
} from "./ollama-cloud.ts";

/**
 * Ollama Cloud is not a pi built-in, so these tests pin the pieces pi cannot
 * derive: the endpoint, the key name, and the models. The protocol itself is
 * pi's OpenAI-completions implementation, which its own tests cover.
 */

describe("the provider", () => {
  test("is named and pointed at Ollama's OpenAI-compatible endpoint", () => {
    const provider = ollamaCloudProvider();
    expect(provider.id).toBe(OLLAMA_CLOUD_ID);
    expect(provider.id).toBe("ollama-cloud");
    expect(provider.baseUrl).toBe(OLLAMA_CLOUD_BASE_URL);
    expect(provider.baseUrl).toBe("https://ollama.com/v1");
  });

  test("authenticates with the key Ollama documents", async () => {
    const provider = ollamaCloudProvider();
    const resolved = await provider.auth.apiKey?.resolve({
      ctx: { env: async (name: string) => (name === "OLLAMA_API_KEY" ? "k-1" : undefined) },
      signal: AbortSignal.timeout(1000),
    } as never);
    expect(OLLAMA_CLOUD_KEY).toBe("OLLAMA_API_KEY");
    expect(resolved).toEqual({ auth: { apiKey: "k-1" }, source: "OLLAMA_API_KEY" });
  });

  test("reports no auth when the key is absent, rather than inventing one", async () => {
    const provider = ollamaCloudProvider();
    const resolved = await provider.auth.apiKey?.resolve({
      ctx: { env: async () => undefined },
      signal: AbortSignal.timeout(1000),
    } as never);
    expect(resolved).toBeUndefined();
  });

  test("a stored credential wins over the environment, so a login takes effect", async () => {
    const provider = ollamaCloudProvider();
    const resolved = await provider.auth.apiKey?.resolve({
      ctx: { env: async () => "from-env" },
      credential: { type: "api_key", key: "from-login" },
      signal: AbortSignal.timeout(1000),
    } as never);
    expect(resolved?.auth.apiKey).toBe("from-login");
    expect(resolved?.source).toBe("stored credential");
  });
});

/** The provider resolves its models through `getModels`, not a property. */
const models = () => ollamaCloudProvider().getModels();

describe("the models it registers", () => {
  test("every model is a chat model on this provider's own endpoint", async () => {
    const registered = await models();
    expect(registered.length).toBe(OLLAMA_CLOUD_MODELS.length);
    for (const model of registered) {
      expect(model.api).toBe("openai-completions");
      expect(model.provider).toBe(OLLAMA_CLOUD_ID);
      expect(model.baseUrl).toBe(OLLAMA_CLOUD_BASE_URL);
      expect(model.input).toEqual(["text"]);
      expect(model.contextWindow).toBeGreaterThan(0);
      expect(model.maxTokens).toBeGreaterThan(0);
    }
  });

  test("the model ids are the names Ollama's own API answers to", () => {
    expect(OLLAMA_CLOUD_MODELS.map((model) => model.id)).toEqual([
      "gemma4:31b",
      "gpt-oss:120b",
      "gpt-oss:20b",
      "qwen3-coder:480b",
      "deepseek-v3.1:671b",
    ]);
  });

  test("a listed model carries the window the list states", async () => {
    const qwen = (await models()).find((model) => model.id === "qwen3-coder:480b");
    expect(qwen?.contextWindow).toBe(256_000);
  });

  test("cost is zero, because Ollama bills the account rather than per token", async () => {
    for (const model of await models()) {
      expect(model.cost).toEqual({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0 });
    }
  });

  test("no model claims reasoning, which this provider does not report", async () => {
    for (const model of await models()) expect(model.reasoning).toBe(false);
  });
});

describe("the names a person sees", () => {
  test("the provider presents itself as Ollama Cloud", () => {
    expect(ollamaCloudProvider().name).toBe("Ollama Cloud");
  });

  test("the key prompt names Ollama Cloud, so the login card is unambiguous", () => {
    expect(ollamaCloudProvider().auth.apiKey?.name).toBe("Ollama Cloud API key");
  });

  test("each model carries the display name a person reads", async () => {
    const registered = await models();
    expect(registered.map((model) => [model.id, model.name])).toEqual([
      ["gemma4:31b", "Gemma 4 31B"],
      ["gpt-oss:120b", "GPT-OSS 120B"],
      ["gpt-oss:20b", "GPT-OSS 20B"],
      ["qwen3-coder:480b", "Qwen3 Coder 480B"],
      ["deepseek-v3.1:671b", "DeepSeek V3.1 671B"],
    ]);
  });
});

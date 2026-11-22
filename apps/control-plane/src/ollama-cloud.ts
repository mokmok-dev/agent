import { createProvider, envApiKeyAuth, type Provider } from "@earendil-works/pi-ai";
import { openAICompletionsApi } from "@earendil-works/pi-ai/api/openai-completions.lazy";

/**
 * Ollama Cloud as a pi-ai provider.
 *
 * Ollama Cloud answers the OpenAI chat-completions subset at
 * `https://ollama.com/v1`, authenticated with a bearer API key, so it plugs in
 * through pi's existing completions API. Pi has no built-in Ollama provider, so
 * this module supplies the one piece it cannot derive: the base URL, the key
 * name, and the model list.
 */

export const OLLAMA_CLOUD_ID = "ollama-cloud";
export const OLLAMA_CLOUD_BASE_URL = "https://ollama.com/v1";

/**
 * The models a deployment may run. Ollama's own `/api/tags` lists what the
 * account can reach; a static list is what pi registers, and `PI_DEFAULT_MODEL`
 * may name any model the account has even if it is not listed here.
 */
export type OllamaModel = {
  /** The id Ollama answers to, such as `gemma4:31b`. */
  id: string;
  name: string;
  contextWindow: number;
  maxTokens: number;
};

export const OLLAMA_CLOUD_MODELS: readonly OllamaModel[] = [
  { id: "gemma4:31b", name: "Gemma 4 31B", contextWindow: 128_000, maxTokens: 8_192 },
  { id: "gpt-oss:120b", name: "GPT-OSS 120B", contextWindow: 128_000, maxTokens: 8_192 },
  { id: "gpt-oss:20b", name: "GPT-OSS 20B", contextWindow: 128_000, maxTokens: 8_192 },
  { id: "qwen3-coder:480b", name: "Qwen3 Coder 480B", contextWindow: 256_000, maxTokens: 8_192 },
  {
    id: "deepseek-v3.1:671b",
    name: "DeepSeek V3.1 671B",
    contextWindow: 128_000,
    maxTokens: 8_192,
  },
];

/** The key Ollama Cloud reads. It is a bearer token, not a query parameter. */
export const OLLAMA_CLOUD_KEY = "OLLAMA_API_KEY";

/**
 * The provider, as pi registers it. `baseUrl` is stated on the provider so a
 * model named in `PI_DEFAULT_MODEL` that is not in this list still reaches
 * Ollama's endpoint rather than the default OpenAI one.
 */
export function ollamaCloudProvider(): Provider<"openai-completions"> {
  return createProvider<"openai-completions">({
    id: OLLAMA_CLOUD_ID,
    name: "Ollama Cloud",
    baseUrl: OLLAMA_CLOUD_BASE_URL,
    // `envApiKeyAuth` builds the api-key half; `ProviderAuth` wraps it, because
    // a provider may offer both an api key and an OAuth flow.
    auth: { apiKey: envApiKeyAuth("Ollama Cloud API key", [OLLAMA_CLOUD_KEY]) },
    models: OLLAMA_CLOUD_MODELS.map((model) => ({
      id: model.id,
      name: model.name,
      api: "openai-completions" as const,
      provider: OLLAMA_CLOUD_ID,
      baseUrl: OLLAMA_CLOUD_BASE_URL,
      reasoning: false,
      input: ["text" as const],
      contextWindow: model.contextWindow,
      maxTokens: model.maxTokens,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    })),
    api: openAICompletionsApi(),
  });
}

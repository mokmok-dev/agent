import { builtinModels } from "@earendil-works/pi-ai/providers/all";
import type { CreateModelsOptions, CredentialStore, MutableModels } from "@earendil-works/pi-ai";
import type { ModelRef } from "@agent/capability";
import { OLLAMA_CLOUD_ID, ollamaCloudProvider } from "./ollama-cloud.ts";

/**
 * Which providers a deployment enables, and the environment keys they read.
 *
 * Pi's own collection already knows every provider and the key each one reads;
 * what a deployment needs is a choice of which to register, so a Worker only
 * carries the secrets it actually uses and a missing key is a named refusal
 * rather than a silent fallback.
 */

/** The providers this deployment may run. A name pi does not know is refused. */
export type ProviderConfig = {
  /** Providers to register, in preference order for `defaultModel` resolution. */
  providers: readonly string[];
  /** The model a turn runs when the session states none. */
  defaultModel: ModelRef;
};

/**
 * What each built-in provider needs. Only the providers here can be configured,
 * because a key name is part of the decision an operator makes.
 */
export const PROVIDER_KEYS: Readonly<Record<string, readonly string[]>> = {
  "cloudflare-workers-ai": ["CLOUDFLARE_API_KEY", "CLOUDFLARE_ACCOUNT_ID"],
  "cloudflare-ai-gateway": ["CLOUDFLARE_API_KEY", "CLOUDFLARE_ACCOUNT_ID"],
  openai: ["OPENAI_API_KEY"],
  anthropic: ["ANTHROPIC_API_KEY"],
  google: ["GEMINI_API_KEY"],
  "google-vertex": ["GOOGLE_CLOUD_API_KEY"],
  xai: ["XAI_API_KEY"],
  deepseek: ["DEEPSEEK_API_KEY"],
  groq: ["GROQ_API_KEY"],
  mistral: ["MISTRAL_API_KEY"],
  cerebras: ["CEREBRAS_API_KEY"],
  openrouter: ["OPENROUTER_API_KEY"],
  fireworks: ["FIREWORKS_API_KEY"],
  together: ["TOGETHER_API_KEY"],
  huggingface: ["HF_TOKEN"],
  nvidia: ["NVIDIA_API_KEY"],
  baseten: ["BASETEN_API_KEY"],
  minimax: ["MINIMAX_API_KEY"],
  moonshotai: ["MOONSHOT_API_KEY"],
  zai: ["ZAI_API_KEY"],
  // Ollama Cloud is not a pi built-in, so this module supplies it; the key name
  // is the one its own docs use.
  [OLLAMA_CLOUD_ID]: ["OLLAMA_API_KEY"],
};

/**
 * The providers this repository adds itself. Pi's collection knows the built-ins
 * and nothing else, so a provider that is not built in is registered here after
 * the collection is built.
 */
const CUSTOM_PROVIDERS: Readonly<
  Record<string, () => Parameters<MutableModels["setProvider"]>[0]>
> = {
  [OLLAMA_CLOUD_ID]: ollamaCloudProvider,
};

export type ProviderRefused =
  | { kind: "unknown-provider"; provider: string }
  | { kind: "no-default-model" }
  | { kind: "missing-key"; provider: string; keys: readonly string[] };

export type ProviderSetup =
  | { kind: "configured"; models: MutableModels; defaultModel: ModelRef }
  | { kind: "refused"; refusal: ProviderRefused };

/**
 * Build the model collection a deployment may use.
 *
 * Pi registers every built-in provider, because its catalog is one table. What
 * this deployment controls is which providers' keys are reachable: `env` is
 * filtered to the named providers, so a session cannot run on a provider the
 * operator did not enable even though pi knows it.
 *
 * A credential stored on the object wins over the environment, because a login
 * through OAuth has no secret to put there. A provider with neither is still
 * registered, since pi reports that as "not authenticated" with the key's name,
 * which is a better message than the provider being absent.
 */
export function configureProviders(
  config: ProviderConfig,
  env: (name: string) => string | undefined,
  credentials?: CredentialStore,
): ProviderSetup {
  const unknown = config.providers.find((provider) => PROVIDER_KEYS[provider] === undefined);
  if (unknown !== undefined) {
    return { kind: "refused", refusal: { kind: "unknown-provider", provider: unknown } };
  }
  if (config.defaultModel.provider === "" || config.defaultModel.modelId === "") {
    return { kind: "refused", refusal: { kind: "no-default-model" } };
  }
  const enabled = new Set(config.providers);
  if (!enabled.has(config.defaultModel.provider)) {
    return {
      kind: "refused",
      refusal: { kind: "unknown-provider", provider: config.defaultModel.provider },
    };
  }
  const options = authContext(env, config.providers);
  if (credentials !== undefined) options.credentials = credentials;
  const models = builtinModels(options);
  // Pi registers its built-ins; the providers this repository adds are
  // registered after it, and only when the deployment enabled them.
  for (const provider of config.providers) {
    const build = CUSTOM_PROVIDERS[provider];
    if (build !== undefined) models.setProvider(build());
  }
  return { kind: "configured", models, defaultModel: config.defaultModel };
}

/**
 * The providers a deployment can actually run: those whose keys are all set.
 * A caller uses this to answer "what can I choose" without trying a call.
 */
export function availableProviders(
  configured: readonly string[],
  env: (name: string) => string | undefined,
): string[] {
  return configured.filter((provider) =>
    (PROVIDER_KEYS[provider] ?? []).every((key) => (env(key) ?? "") !== ""),
  );
}

function authContext(
  env: (name: string) => string | undefined,
  providers: readonly string[],
): CreateModelsOptions {
  // Only the keys of the enabled providers are reachable, so a secret for a
  // provider this deployment does not run cannot be used by accident.
  const allowed = new Set(providers.flatMap((provider) => PROVIDER_KEYS[provider] ?? []));
  return {
    authContext: {
      env: async (name: string) => (allowed.has(name) ? env(name) : undefined),
      fileExists: async () => false,
    },
  };
}

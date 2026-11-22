import type { Env } from "./worker.ts";

/** The provider config every test shares. */
export const CONFIG = {
  slack: { signingSecret: "shhh", botUserId: "UBOT" },
  discord: { publicKey: "aa" },
  linear: { webhookSecret: "linear" },
};

/** The Linear OAuth application a test authorizes against. */
export const OAUTH = {
  LINEAR_CLIENT_ID: "client-1",
  LINEAR_CLIENT_SECRET: "secret-1",
  LINEAR_REDIRECT_URI: "https://example.test/oauth/linear/callback",
};

/**
 * An `Env` with the provider config and the OAuth application filled in. A test
 * overrides only the bindings it is exercising.
 */
export function envWith(overrides: Partial<Env> = {}): Env {
  return {
    ...CONFIG,
    ...OAUTH,
    THREAD_INDEX: { getByName: () => ({}) },
    SESSIONS: { getByName: () => ({}) },
    LINEAR_OAUTH: { getByName: () => ({}) },
    ...overrides,
  };
}

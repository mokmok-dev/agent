import type { AdapterResult, RawRequest as Raw } from "@agent/adapters";
import {
  adaptDiscord,
  adaptLinear,
  adaptSlack,
  authorizeUrl,
  refuse,
  type LinearOAuthConfig,
} from "@agent/adapters";

/** The provider secrets and ids, as the platform exposes them. */
export type ProviderEnv = {
  SLACK_SIGNING_SECRET: string;
  SLACK_BOT_USER_ID: string;
  DISCORD_PUBLIC_KEY: string;
  LINEAR_WEBHOOK_SECRET: string;
};

/** The nested config the adapters take. */
export type ProviderConfig = {
  slack: { signingSecret: string; botUserId: string };
  discord: { publicKey: string };
  linear: { webhookSecret: string };
};

/**
 * The flat platform names as the nested config the adapters read. Without this
 * the Worker would look for `env.slack`, which a Worker never has.
 */
export function providerConfigOf(env: ProviderEnv): ProviderConfig {
  return {
    slack: { signingSecret: env.SLACK_SIGNING_SECRET, botUserId: env.SLACK_BOT_USER_ID },
    discord: { publicKey: env.DISCORD_PUBLIC_KEY },
    linear: { webhookSecret: env.LINEAR_WEBHOOK_SECRET },
  };
}

/**
 * The Linear OAuth application, as the platform exposes it: two secrets and a
 * var, each named for the binding it comes from.
 */
export type LinearOAuthEnv = {
  LINEAR_CLIENT_ID: string;
  LINEAR_CLIENT_SECRET: string;
  /** The exact redirect URI registered on the Linear application. */
  LINEAR_REDIRECT_URI: string;
};

/** The same application, in the shape the adapters take. */
export function oauthConfigOf(env: LinearOAuthEnv): LinearOAuthConfig {
  return {
    clientId: env.LINEAR_CLIENT_ID,
    clientSecret: env.LINEAR_CLIENT_SECRET,
    redirectUri: env.LINEAR_REDIRECT_URI,
  };
}

/**
 * Whether the OAuth application is configured. A missing secret makes the
 * consent screen useless, so the start route reports that instead of sending a
 * person to Linear with an empty client id.
 */
export function oauthConfigured(env: LinearOAuthEnv): boolean {
  return env.LINEAR_CLIENT_ID !== "" && env.LINEAR_CLIENT_SECRET !== "";
}

export type Route =
  | { kind: "slack" }
  | { kind: "discord" }
  | { kind: "linear" }
  | { kind: "linear-oauth-start" }
  | { kind: "linear-oauth-callback" }
  | { kind: "linear-oauth-status" }
  | { kind: "providers-status" }
  | { kind: "gateway" }
  | { kind: "health" }
  | { kind: "not-found" };

const ROUTES = new Map<string, Route>([
  ["/webhooks/slack", { kind: "slack" }],
  ["/webhooks/discord", { kind: "discord" }],
  ["/webhooks/linear", { kind: "linear" }],
  ["/oauth/linear", { kind: "linear-oauth-start" }],
  ["/oauth/linear/callback", { kind: "linear-oauth-callback" }],
  ["/oauth/linear/status", { kind: "linear-oauth-status" }],
  ["/providers/status", { kind: "providers-status" }],
  ["/gateway", { kind: "gateway" }],
  ["/health", { kind: "health" }],
]);

export function routeOf(pathname: string): Route {
  return ROUTES.get(pathname) ?? { kind: "not-found" };
}

export async function rawRequestOf(request: Request): Promise<Raw> {
  const headers: Record<string, string | undefined> = {};
  for (const [name, value] of request.headers) headers[name.toLowerCase()] = value;
  return { method: request.method, headers, body: await request.text() };
}

export async function handleWebhook(
  provider: "slack" | "discord" | "linear",
  request: Raw,
  config: ProviderConfig,
  nowSeconds: number,
): Promise<AdapterResult> {
  if (request.method !== "POST") return refuse(405, "only POST is accepted");
  if (provider === "slack") return adaptSlack(request, config.slack, nowSeconds);
  if (provider === "discord") return adaptDiscord(request, config.discord);
  return adaptLinear(request, config.linear);
}

/**
 * Send a person to Linear's consent screen. The `state` is written to the
 * object before this returns, so the callback can prove it started the flow.
 */
export function oauthStart(config: LinearOAuthConfig, state: string): Response {
  return Response.redirect(authorizeUrl(config, state), 302);
}

/**
 * The status and body to answer an adapter result with. A challenge returns
 * the challenge verbatim; an event is acknowledged at once so the provider
 * does not retry, and the work happens behind the acknowledgement.
 */
export function responseFor(result: AdapterResult): { status: number; body: unknown } {
  switch (result.kind) {
    case "challenge":
      return { status: 200, body: isPong(result.response) ? { type: 1 } : result.response };
    case "event":
      return { status: 202, body: { ok: true, eventId: result.event.eventId } };
    case "ignored":
      return { status: 200, body: { ok: true, ignored: result.reason } };
    case "refused":
      return { status: result.status, body: { error: { message: result.message } } };
  }
}

function isPong(response: string): boolean {
  return response === "pong";
}

/**
 * The answer for a fault in this repository. An adapter never reaches here,
 * because a body it cannot read is a `refused` result; this covers a bug, so it
 * is the only place a thrown value becomes a response.
 */
export function errorResponse(error: unknown): Response {
  const message = error instanceof Error ? error.message : String(error);
  return Response.json({ error: { message } }, { status: 500 });
}

/** A small HTML page, so a person finishing a login sees a result. */
export function page(title: string, detail: string): Response {
  const escaped = detail.replace(/[<>&]/g, (character) =>
    character === "<" ? "&lt;" : character === ">" ? "&gt;" : "&amp;",
  );
  return new Response(
    `<!doctype html><meta charset="utf-8"><title>${title}</title>` +
      `<body style="font:16px system-ui;max-width:40rem;margin:4rem auto"><h1>${title}</h1>` +
      `<p>${escaped}</p></body>`,
    { status: 200, headers: { "content-type": "text/html; charset=utf-8" } },
  );
}

export { isPong };

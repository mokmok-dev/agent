import type { AdapterResult, InboundEvent, RawRequest as Raw } from "@agent/adapters";
import { adaptDiscord, adaptLinear, adaptSlack, AdapterError } from "@agent/adapters";

export type ProviderConfig = {
  slack: { signingSecret: string; botUserId: string };
  discord: { publicKey: string };
  linear: { webhookSecret: string };
};

export type Route =
  | { kind: "slack" }
  | { kind: "discord" }
  | { kind: "linear" }
  | { kind: "health" }
  | { kind: "not-found" };

const ROUTES = new Map<string, Route>([
  ["/webhooks/slack", { kind: "slack" }],
  ["/webhooks/discord", { kind: "discord" }],
  ["/webhooks/linear", { kind: "linear" }],
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
  provider: Route["kind"],
  request: Raw,
  config: ProviderConfig,
  nowSeconds: number,
): Promise<AdapterResult> {
  if (request.method !== "POST") throw new AdapterError(405, "only POST is accepted");
  if (provider === "slack") return adaptSlack(request, config.slack, nowSeconds);
  if (provider === "discord") return adaptDiscord(request, config.discord);
  if (provider === "linear") return adaptLinear(request, config.linear);
  throw new AdapterError(404, "no adapter for this route");
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
  }
}

function isPong(response: string): boolean {
  return response === "pong";
}

export function errorResponse(error: unknown): Response {
  if (error instanceof AdapterError) {
    return Response.json({ error: { message: error.message } }, { status: error.status });
  }
  const message = error instanceof Error ? error.message : String(error);
  return Response.json({ error: { message } }, { status: 500 });
}

export { isPong };
export type { InboundEvent };

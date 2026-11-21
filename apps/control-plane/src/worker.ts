import type { InboundEvent } from "@agent/adapters";
import type { Session, ThreadRef, Turn, TurnUpdate } from "@agent/store";
import {
  errorResponse,
  handleWebhook,
  rawRequestOf,
  responseFor,
  routeOf,
  type ProviderConfig,
} from "./routes.ts";

export interface Namespace {
  getByName(name: string): unknown;
}

/** The bindings the Worker needs. Names match `wrangler.toml`. */
export interface Env extends ProviderConfig {
  SESSIONS: Namespace;
  THREAD_INDEX: Namespace;
}

interface SessionRpc {
  active(sessionId: string): Promise<Turn | undefined>;
  deliver(
    sessionId: string,
    thread: ThreadRef,
    prompt: string,
    idempotencyKey: string,
    now: number,
  ): Promise<
    | { kind: "accepted" | "replayed"; session: Session; turn: Turn }
    | { kind: "refused"; reason: string }
  >;
  transition(sessionId: string, turnId: string, update: TurnUpdate, now: number): Promise<unknown>;
}

interface IndexRpc {
  lookup(thread: ThreadRef): Promise<string>;
}

export function deliveryBody(
  result:
    | { kind: "challenge"; response: string }
    | { kind: "ignored"; reason: string }
    | { kind: "event"; event: { eventId: string } },
  outcome: unknown,
): { status: number; body: unknown } {
  const base = responseFor(result as never);
  return result.kind === "event"
    ? { status: base.status, body: { ...(base.body as object), outcome } }
    : base;
}

export async function fetch(request: Request, env: Env, now = Date.now()): Promise<Response> {
  const route = routeOf(new URL(request.url).pathname);
  if (route.kind === "not-found") return Response.json({ error: "not found" }, { status: 404 });
  if (route.kind === "health") return Response.json({ ok: true });
  try {
    const result = await handleWebhook(
      route.kind,
      await rawRequestOf(request),
      env,
      Math.floor(now / 1000),
    );
    if (result.kind !== "event") {
      const { status, body } = responseFor(result);
      return Response.json(body, { status });
    }
    const outcome = await accept(env, result.event, now);
    const { status, body } = deliveryBody(result, outcome);
    return Response.json(body, { status });
  } catch (error) {
    return errorResponse(error);
  }
}

/**
 * Route one inbound event to its session. The thread index names the session,
 * so a delivery that races another for the same thread lands on one object.
 */
export async function accept(
  env: Env,
  event: Extract<InboundEvent, { kind: "message" | "cancel" }>,
  now: number,
): Promise<unknown> {
  const thread: ThreadRef = { provider: event.provider, ...event.thread };
  const index = env.THREAD_INDEX.getByName("index") as IndexRpc;
  const sessionId = await index.lookup(thread);
  const sessions = env.SESSIONS.getByName(sessionId) as SessionRpc;
  if (event.kind === "cancel") {
    const active = await sessions.active(sessionId);
    if (active === undefined) return { kind: "refused", reason: "no-active-turn" };
    return sessions.transition(sessionId, active.id, { status: "cancelled" }, now);
  }
  return sessions.deliver(sessionId, thread, event.text, event.eventId, now);
}

export default { fetch };

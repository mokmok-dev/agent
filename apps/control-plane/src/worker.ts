import { callbackOf, type InboundEvent } from "@agent/adapters";
import type { Session, ThreadRef, Turn, TurnUpdate } from "@agent/store";
import {
  errorResponse,
  handleWebhook,
  rawRequestOf,
  oauthConfigOf,
  responseFor,
  routeOf,
  type LinearOAuthEnv,
  providerConfigOf,
  type ProviderEnv,
} from "./routes.ts";
import { finishLogin, loginStatus, startLogin, type LinearOAuthPort } from "./linear-oauth-port.ts";
import { linearReplies, type LinearReplyPort, type ReplyOutcome } from "./linear-reply.ts";

export interface Namespace {
  getByName(name: string): unknown;
}

/** The bindings the Worker needs. Names match `wrangler.toml`. */
export interface Env extends ProviderEnv, LinearOAuthEnv, ModelEnv {
  SESSIONS: Namespace;
  THREAD_INDEX: Namespace;
  LINEAR_OAUTH: Namespace;
}

/**
 * The model a deployment runs, stated as configuration. `PI_PROVIDERS` is a
 * comma-separated list and `PI_DEFAULT_MODEL` is `provider:modelId`, so an
 * operator changes providers with `wrangler secret put` and a var rather than a
 * deploy of new code.
 */
export interface ModelEnv {
  PI_PROVIDERS?: string;
  PI_DEFAULT_MODEL?: string;
}

interface SessionRpc {
  active(sessionId: string): Promise<Turn | undefined>;
  /**
   * A delivery either reaches a turn, which carries the run it produced, or is
   * refused, which carries no run.
   */
  accept(
    sessionId: string,
    thread: ThreadRef,
    prompt: string,
    idempotencyKey: string,
    now: number,
  ): Promise<
    | {
        kind: "accepted" | "replayed";
        session: Session;
        turn: Turn;
        run?: { kind: string; text?: string; error?: string };
      }
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
  const url = new URL(request.url);
  const route = routeOf(url.pathname);
  if (route.kind === "not-found") return Response.json({ error: "not found" }, { status: 404 });
  if (route.kind === "health") return Response.json({ ok: true });
  if (
    route.kind === "linear-oauth-start" ||
    route.kind === "linear-oauth-callback" ||
    route.kind === "linear-oauth-status"
  ) {
    const port = env.LINEAR_OAUTH.getByName("linear") as LinearOAuthPort;
    if (route.kind === "linear-oauth-start") return startLogin(port, env, oauthConfigOf(env));
    if (route.kind === "linear-oauth-status") return loginStatus(port, now);
    return finishLogin(port, callbackOf(url.searchParams), now);
  }
  try {
    const result = await handleWebhook(
      route.kind,
      await rawRequestOf(request),
      providerConfigOf(env),
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
 *
 * Linear opens a session with a `created` event and expects a thought within ten
 * seconds; the acknowledgement is sent before the turn is recorded, so a slow
 * turn cannot make the session look unresponsive.
 */
export async function accept(
  env: Env,
  event: Extract<InboundEvent, { kind: "message" | "cancel" }>,
  now: number,
  replies?: LinearReplyPort,
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
  const port =
    event.provider === "linear"
      ? (replies ?? linearReplies(env.LINEAR_OAUTH.getByName("linear") as LinearOAuthPort))
      : undefined;
  if (port !== undefined && event.stage === "start") {
    await port.acknowledge(event.thread.threadId, now);
  }
  const outcome = await sessions.accept(sessionId, thread, event.text, event.eventId, now);
  if (port === undefined) return outcome;
  // A refusal is passed through as it is; a delivery that reached a turn
  // carries the run its agent produced, which is what the reply is made of.
  if (outcome.kind === "refused") return outcome;
  const reply = await replyFor(port, event.thread.threadId, outcome.run, now);
  return reply === undefined ? outcome : { ...outcome, reply };
}

/**
 * Answer the thread with the agent's words. A turn that failed says so, because
 * a silence looks like a hung agent; a turn that never ran returns `undefined`
 * so a deployment with no runner does not send an empty answer.
 */
async function replyFor(
  port: LinearReplyPort,
  sessionId: string,
  run: { kind: string; text?: string; error?: string } | undefined,
  now: number,
): Promise<ReplyOutcome | undefined> {
  if (run === undefined) return undefined;
  if (run.kind === "answered") return port.answer(sessionId, run.text ?? "", now);
  if (run.kind === "failed") {
    return port.answer(sessionId, `The run failed: ${run.error ?? "no reason given"}`, now);
  }
  return undefined;
}

export default { fetch };

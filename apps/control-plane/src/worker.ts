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
import {
  finishLogin,
  loginStatus,
  providerStatus,
  startLogin,
  type LinearOAuthPort,
} from "./linear-oauth-port.ts";
import { linearReplies, type LinearReplyPort, type ReplyOutcome } from "./linear-reply.ts";
import type { RoutingEnv } from "./routing.ts";

export interface Namespace {
  getByName(name: string): unknown;
}

/** The bindings the Worker needs. Names match `wrangler.toml`. */
export interface Env extends ProviderEnv, LinearOAuthEnv, ModelEnv, RoutingEnv {
  SESSIONS: Namespace;
  THREAD_INDEX: Namespace;
  LINEAR_OAUTH: Namespace;
  /** The one object that holds the provider credentials for the deployment. */
  PROVIDERS?: Namespace;
}

/**
 * The model a deployment runs, stated as configuration. `PI_PROVIDERS` is a
 * comma-separated list and `PI_DEFAULT_MODEL` is `provider:modelId`, so an
 * operator changes providers with `wrangler secret put` and a var rather than a
 * deploy of new code.
 */
export interface ModelEnv {
  /** `1` logs each webhook's outcome and body, for diagnosing an ignored one. */
  DEBUG_WEBHOOKS?: string;
  PI_PROVIDERS?: string;
  PI_DEFAULT_MODEL?: string;
}

interface SessionRpc {
  active(sessionId: string): Promise<Turn | undefined>;
  /** The oldest queued turn, which is what a gateway waiting for work serves. */
  next(sessionId: string): Promise<Turn | undefined>;
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
  remember(thread: ThreadRef, sessionId: string): Promise<void>;
  recent(provider: ThreadRef["provider"]): Promise<string[]>;
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

  if (route.kind === "gateway") {
    // The Worker only routes: the gateway names the session it serves, and that
    // object owns the socket. A non-upgrade is refused here, so the answer does
    // not depend on reaching the object at all.
    if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
      return Response.json({ error: "expected a WebSocket upgrade" }, { status: 426 });
    }
    const requested = url.searchParams.get("session");
    if (requested === null || requested === "") {
      return Response.json(
        { error: "name the session a gateway serves with ?session=<id>" },
        { status: 400 },
      );
    }
    // `auto` is a gateway that connected before the delegation it serves: a
    // Linear agent session does not exist until someone delegates, so there is
    // no id to name yet. The newest session with a queued turn is the one
    // waiting for a gateway, which is what the answer names.
    const sessionId = requested === "auto" ? await waitingSessionFor(env) : requested;
    if (sessionId === undefined) {
      return Response.json(
        { error: "no session is waiting for a gateway; delegate an issue first" },
        { status: 404 },
      );
    }
    const object = env.SESSIONS.getByName(sessionId) as {
      fetch(request: Request): Promise<Response>;
    };
    // The object is named by its id, but a gateway that asked for `auto` needs
    // to be told which one it reached. The header carries that across the
    // handoff, because the upgrade request itself is the only channel.
    const forwarded = new Request(request, {
      headers: new Headers([...request.headers, ["x-agent-session", sessionId]]),
    });
    return object.fetch(forwarded);
  }
  if (route.kind === "providers-status") {
    return providersStatus(env);
  }
  try {
    const raw = await rawRequestOf(request);
    const result = await handleWebhook(
      route.kind as "slack" | "discord" | "linear",
      raw,
      providerConfigOf(env),
      Math.floor(now / 1000),
    );
    // A webhook that is ignored is invisible otherwise: the provider sees 200,
    // and nothing records why no turn was created. `DEBUG_WEBHOOKS` answers that
    // without logging a body that may carry a person's message by default.
    if (env.DEBUG_WEBHOOKS === "1") {
      console.log(
        `webhook ${route.kind} kind=${result.kind}` +
          (result.kind === "ignored" ? ` reason=${result.reason}` : "") +
          ` body=${raw.body.slice(0, 1500)}`,
      );
    }
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
 * The newest session that has a queued turn, for a gateway that connected
 * before the delegation it serves.
 *
 * A Linear agent session id is created by Linear at delegation time, so a
 * gateway cannot name it up front. It asks for the session that is actually
 * waiting, which is the newest one with a `queued` turn. An older session's
 * stale turn does not shadow a newer one.
 */
async function waitingSessionFor(env: Env): Promise<string | undefined> {
  const index = env.THREAD_INDEX.getByName("index") as IndexRpc;
  const candidates = await index.recent("linear");
  for (const sessionId of candidates) {
    const sessions = env.SESSIONS.getByName(sessionId) as SessionRpc;
    const queued = await sessions.next(sessionId);
    if (queued !== undefined) return sessionId;
  }
  return undefined;
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
  // A gateway that connected before this delivery needs to find it, so the
  // session is recorded where a gateway looks. It is written before the turn,
  // so a gateway that attaches now waits for a turn rather than missing it.
  await index.remember(thread, sessionId);
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
  // The reply is not built here: the run settles after this webhook has already
  // answered Linear, so the session object sends the answer from the run.
  return sessions.accept(sessionId, thread, event.text, event.eventId, now);
}

/**
 * Answer the thread with the agent's words. A turn that failed says so, because
 * a silence looks like a hung agent; a turn that never ran returns `undefined`
 * so a deployment with no runner does not send an empty answer.
 */
export async function replyFor(
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

/**
 * `GET /providers/status`: the providers this deployment may run and whether
 * each has a credential. A name and a credential type are safe to show; the
 * credential itself never appears.
 */
export async function providersStatus(env: Env): Promise<Response> {
  const configured = modelProvidersOf(env);
  const stored =
    env.PROVIDERS === undefined
      ? []
      : await (env.PROVIDERS.getByName("providers") as ProviderRpc).list();
  return providerStatus(configured, stored);
}

/** The provider list a deployment states, from `PI_PROVIDERS`. */
export function modelProvidersOf(env: Env): readonly string[] {
  const stated = (env.PI_PROVIDERS ?? "")
    .split(",")
    .map((name) => name.trim())
    .filter((name) => name !== "");
  return stated.length > 0 ? stated : ["cloudflare-workers-ai"];
}

/** The credential object's RPC surface, as the status route calls it. */
interface ProviderRpc {
  list(): Promise<readonly { providerId: string; type: string }[]>;
}

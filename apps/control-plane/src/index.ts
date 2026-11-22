import { DurableObject } from "cloudflare:workers";
import type { ThreadRef } from "@agent/store";
import { LinearOAuthObject, type LinearCredential } from "./linear-oauth-do.ts";
import { objectStorage } from "./platform-storage.ts";
import { SessionObject } from "./session-do.ts";
import { ThreadIndexObject } from "./thread-index-do.ts";
import { oauthConfigOf } from "./routes.ts";
import { catalogOf, routingPolicyOf } from "./routing.ts";
import type { AccessToken, Completed } from "./linear-oauth-do.ts";
import type { Credential, CredentialInfo } from "@earendil-works/pi-ai";
import { ProviderCredentialObject } from "./providers-do.ts";
import { piRunnerFor } from "./pi-runtime.ts";
import { fetch as handle, replyFor, type Env } from "./worker.ts";
import { linearReplies } from "./linear-reply.ts";
import type { LinearOAuthPort } from "./linear-oauth-port.ts";

/**
 * The entry Wrangler deploys. A Durable Object's RPC is class-based, so each
 * class carries the platform's constructor and storage adapter; the logic is in
 * `SessionObject` and `ThreadIndexObject`, which a test drives without the
 * platform. This module is the only one that imports `cloudflare:workers`.
 */
export class SessionDurableObject extends DurableObject<Env> {
  readonly #object: SessionObject;
  /** Chains one connection's messages, so a bind and a settle cannot interleave. */
  #gatewayQueue: Promise<void> = Promise.resolve();

  constructor(state: DurableObjectState, env: Env) {
    super(state, env);
    // The route and the model are stated together, because the router pins a
    // model that pi then resolves against the providers this deployment
    // registered. A catalog naming another provider's model fails `no_model`.
    const policy = routingPolicyOf(env);
    this.#object = new SessionObject(
      objectStorage(state.storage),
      { policy, catalog: catalogOf(env, policy.defaultAgent) },
      piRunnerFor(state, env),
      // `accept` answers Linear inside its five-second window, so the model call
      // runs after that answer. Without waitUntil the object hibernates on
      // return and cancels the run mid-flight.
      (work) => state.waitUntil(work),
      // The reply follows the run, not the webhook: by the time the agent speaks
      // Linear has already been answered, so this is the only place the words
      // can reach the thread.
      (outcome, threadId) => this.#answer(env, outcome, threadId),
    );
  }

  async #answer(
    env: Env,
    outcome: { kind: "answered"; text: string } | { kind: "failed"; error: string },
    threadId: string,
  ): Promise<void> {
    const port = linearReplies(env.LINEAR_OAUTH.getByName("linear") as LinearOAuthPort);
    // The reply is the last link between a run and the person, and it happens
    // after the webhook has already answered, so nothing else records what it
    // did. Its outcome is logged for exactly that reason.
    try {
      const result = await replyFor(port, threadId, outcome, Date.now());
      console.log(
        `reply for ${threadId}: ${result === undefined ? "nothing-to-say" : result.kind}` +
          (result?.kind === "failed" ? ` reason=${result.error}` : ""),
      );
    } catch (error) {
      console.log(
        `reply for ${threadId} threw: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  /**
   * Take the gateway's WebSocket. A Durable Object answers `fetch` for its own
   * name, so the upgrade happens here rather than in the Worker's route table,
   * and every message is driven by the object's own connection handler.
   */
  override async fetch(request: Request): Promise<Response> {
    if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
      return Response.json({ error: "expected a WebSocket upgrade" }, { status: 426 });
    }
    const pair = new WebSocketPair();
    const [client, server] = [pair[0], pair[1]];
    server.accept();
    const gateway = {
      sessionId: undefined as string | undefined,
      send: (frame: string) => server.send(frame),
    };
    // The Worker resolved the session's real name before handing the upgrade
    // over, because a gateway that asked for `auto` only knows the word.
    const resolved = request.headers.get("x-agent-session") ?? undefined;
    server.addEventListener("message", (event) => {
      const raw = typeof event.data === "string" ? event.data : "";
      // Messages are serialized by chaining, so a bind and a completed event that
      // arrive together cannot both read the session as it was before the bind.
      this.#gatewayQueue = this.#gatewayQueue
        .then(() => this.#object.gateway(gateway, raw, Date.now(), resolved))
        // A message can claim the only queued turn, which removes the deadline
        // it was armed for. Re-arming after every message keeps the alarm in
        // step with what is actually waiting.
        .then(() => this.#armDeadline(resolved ?? gateway.sessionId ?? ""));
    });
    return new Response(null, { status: 101, webSocket: client });
  }

  accept(...args: Parameters<SessionObject["accept"]>) {
    return this.#object.accept(...args).then(async (outcome) => {
      // A delivery creates the turn, so this is the moment its deadline starts.
      // Arming here rather than on a timer is what makes an idle session sweep
      // itself: without it a queued turn waited for a later delivery or a
      // gateway connection that never came.
      await this.#armDeadline(args[0]);
      return outcome;
    });
  }

  /**
   * Schedule this object to wake at its next claim deadline, if it has one.
   *
   * The platform delivers `alarm` to the same object, so the sweep and the
   * turns it reads are the same single-threaded context. A deadline already in
   * the past is due now, and `setAlarm` accepts a past time.
   */
  async #armDeadline(sessionId: string): Promise<void> {
    const at = await this.#object.deadline(sessionId, Date.now());
    if (at === undefined) {
      // Nothing is waiting, so disarm rather than leave a wake-up that would
      // find an empty queue.
      await this.ctx.storage.deleteAlarm();
      return;
    }
    // A Durable Object takes one alarm, so the soonest deadline wins: scheduling
    // a later one would delay an earlier turn's failure.
    const armed = await this.ctx.storage.getAlarm();
    if (armed !== null && armed <= at) return;
    await this.ctx.storage.setAlarm(at);
  }

  /** The deadline this object was armed for. */
  override async alarm(): Promise<void> {
    // The alarm names no session, because it belongs to the object that armed
    // it; `this.ctx.id.name` is the session id the Worker resolved.
    const sessionId = this.ctx.id.name ?? "";
    const now = Date.now();
    await this.#object.expire(sessionId, now);
    await this.#armDeadline(sessionId);
  }

  deliver(...args: Parameters<SessionObject["deliver"]>) {
    return this.#object.deliver(...args);
  }

  session(...args: Parameters<SessionObject["session"]>) {
    return this.#object.session(...args);
  }

  turns(...args: Parameters<SessionObject["turns"]>) {
    return this.#object.turns(...args);
  }

  next(...args: Parameters<SessionObject["next"]>) {
    return this.#object.next(...args);
  }

  active(...args: Parameters<SessionObject["active"]>) {
    return this.#object.active(...args);
  }

  transition(...args: Parameters<SessionObject["transition"]>) {
    return this.#object.transition(...args);
  }

  bind(...args: Parameters<SessionObject["bind"]>) {
    return this.#object.bind(...args);
  }

  close(...args: Parameters<SessionObject["close"]>) {
    return this.#object.close(...args);
  }
}

export class ThreadIndexDurableObject extends DurableObject<Env> {
  readonly #object: ThreadIndexObject;

  constructor(state: DurableObjectState, env: Env) {
    super(state, env);
    this.#object = new ThreadIndexObject(objectStorage(state.storage));
  }

  lookup(thread: ThreadRef): Promise<string> {
    return this.#object.lookup(thread);
  }

  forget(thread: ThreadRef): Promise<void> {
    return this.#object.forget(thread);
  }

  /** Record that a delivery created this session, so a waiting gateway finds it. */
  remember(thread: ThreadRef, sessionId: string): Promise<void> {
    return this.#object.remember(thread, sessionId);
  }

  /** The sessions a provider has seen, newest first, for a gateway asking for work. */
  recent(provider: ThreadRef["provider"]): Promise<string[]> {
    return this.#object.recent(provider);
  }
}

export default {
  fetch(request: Request, env: Env): Promise<Response> {
    return handle(request, env);
  },
};
/**
 * The Linear authorization. One object named `linear` holds the pairing for the
 * deployment, so completing a second login replaces the first.
 */
export class LinearOAuthDurableObject extends DurableObject<Env> {
  readonly #object: LinearOAuthObject;

  constructor(state: DurableObjectState, env: Env) {
    super(state, env);
    this.#object = new LinearOAuthObject(objectStorage(state.storage), oauthConfigOf(env));
  }

  start(state: string): Promise<void> {
    return this.#object.start(state);
  }

  status(): Promise<LinearCredential | undefined> {
    return this.#object.credential();
  }

  complete(params: Record<string, string>, now: number): Promise<Completed> {
    return this.#object.complete(params, now);
  }

  accessToken(now: number): Promise<AccessToken | undefined> {
    return this.#object.accessToken(now);
  }

  refresh(now: number): Promise<boolean> {
    return this.#object.refresh(now);
  }

  disconnect(): Promise<void> {
    return this.#object.disconnect();
  }
}

/**
 * The provider credentials for the deployment. One object named `providers`,
 * so a credential an operator stores applies to every session.
 */
export class ProviderCredentialDurableObject extends DurableObject<Env> {
  readonly #object: ProviderCredentialObject;

  constructor(state: DurableObjectState, env: Env) {
    super(state, env);
    this.#object = new ProviderCredentialObject(objectStorage(state.storage));
  }

  read(providerId: string): Promise<Credential | undefined> {
    return this.#object.read(providerId);
  }

  list(): Promise<readonly CredentialInfo[]> {
    return this.#object.list();
  }

  write(providerId: string, credential: Credential): Promise<void> {
    return this.#object.write(providerId, credential);
  }

  delete(providerId: string): Promise<void> {
    return this.#object.delete(providerId);
  }
}

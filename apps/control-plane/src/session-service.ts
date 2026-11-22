import {
  acceptsTurns,
  SessionRepository,
  type AppendResult,
  type Session,
  type SessionSelection,
  type Storage,
  type ThreadRef,
  type Turn,
  type TurnUpdate,
} from "@agent/store";
import {
  CATALOG,
  DEFAULT_POLICY,
  route,
  type Catalog,
  type RoutingPolicy,
  type RoutingRequest,
} from "@agent/capability";
import type { TurnRunner } from "./turn-runner.ts";

/** What a delivery asks the router for. An empty request takes the policy's choice. */
export type DeliveryRequest = RoutingRequest;

export type DeliveryOutcome =
  | { kind: "accepted"; session: Session; turn: Turn; selection: SessionSelection }
  | { kind: "replayed"; session: Session; turn: Turn; selection: SessionSelection }
  | { kind: "refused"; reason: "closed" | "unroutable"; refusal?: string };

export type RouteDeps = {
  /** The catalog the router reads. Defaults to the one this deployment ships. */
  catalog?: Catalog;
  /** The deployment's standing choices. Defaults to pi in a sandbox first. */
  policy?: RoutingPolicy;
  /** The pi conversation id for a session. Defaults to the session id, so one session is one conversation. */
  conversationOf?: (sessionId: string) => string;
};

type RouteResult =
  | { ok: true; session: Session; selection: SessionSelection }
  | { ok: false; reason: "unroutable"; refusal: string };

/** How one turn ended. */
export type TurnRunOutcome =
  | { kind: "answered"; text: string }
  | { kind: "failed"; error: string }
  | { kind: "not-queued"; status: Turn["status"] }
  | { kind: "missing" };

/**
 * One conversation's records: its route, its turns, and the idempotency keys
 * that make a redelivery converge. The agent's own history lives in pi; this
 * keeps the identity the control plane needs before pi exists.
 */
export class SessionService {
  readonly #repo: SessionRepository;
  readonly #catalog: Catalog;
  readonly #policy: RoutingPolicy;
  readonly #conversationOf: (sessionId: string) => string;

  constructor(storage: Storage, deps: RouteDeps = {}) {
    this.#repo = new SessionRepository(storage);
    this.#catalog = deps.catalog ?? CATALOG;
    this.#policy = deps.policy ?? DEFAULT_POLICY;
    this.#conversationOf = deps.conversationOf ?? ((sessionId) => sessionId);
  }

  /**
   * Record a delivery and hand it the route its session runs on. A turn is
   * created only once the router has an answer, so a refused request leaves no
   * turn behind and a provider can retry it.
   */
  async deliver(
    sessionId: string,
    thread: ThreadRef,
    prompt: string,
    idempotencyKey: string,
    now: number,
    request: DeliveryRequest = {},
  ): Promise<DeliveryOutcome> {
    const session = await this.#repo.ensureSession(sessionId, thread, now);
    if (!acceptsTurns(session)) return { kind: "refused", reason: "closed" };
    const routed = await this.#resolve(session, request, now);
    if (!routed.ok) return { kind: "refused", reason: routed.reason, refusal: routed.refusal };
    const appended: AppendResult = await this.#repo.appendTurn(
      sessionId,
      prompt,
      idempotencyKey,
      now,
    );
    const outcome =
      appended.kind === "appended"
        ? { kind: "accepted" as const, session: routed.session, turn: appended.turn }
        : { kind: "replayed" as const, session: routed.session, turn: appended.turn };
    return { ...outcome, selection: routed.selection };
  }

  /**
   * The route this session runs on. A session that already chose keeps its
   * choice, so a later delivery cannot move a live conversation.
   */
  async #resolve(session: Session, request: DeliveryRequest, now: number): Promise<RouteResult> {
    if (session.selection !== null) {
      return { ok: true, session, selection: session.selection };
    }
    const result = route(request, this.#catalog, this.#policy);
    if (result.kind === "refused") {
      return { ok: false, reason: "unroutable", refusal: result.refusal.kind };
    }
    const selection: SessionSelection = {
      agent: result.selection.agent,
      runtime: result.selection.runtime,
      model: result.selection.model,
      protocol: result.selection.protocol,
    };
    const conversationId = this.#conversationOf(session.id);
    const routed = await this.#repo.route(session.id, selection, conversationId, now);
    return { ok: true, session: routed, selection };
  }

  session(sessionId: string): Promise<Session | undefined> {
    return this.#repo.session(sessionId);
  }

  turns(sessionId: string): Promise<Turn[]> {
    return this.#repo.turns(sessionId);
  }

  next(sessionId: string): Promise<Turn | undefined> {
    return this.#repo.nextTurn(sessionId);
  }

  active(sessionId: string): Promise<Turn | undefined> {
    return this.#repo.activeTurn(sessionId);
  }

  transition(sessionId: string, turnId: string, update: TurnUpdate, now: number) {
    return this.#repo.transition(sessionId, turnId, update, now);
  }

  /**
   * Run one accepted turn to a terminal state.
   *
   * The turn moves to `dispatched` before the agent sees it, so a crash leaves
   * evidence rather than a turn that looks queued forever, and the claim is a
   * compare-and-set: two deliveries racing for the same turn see one winner.
   * The runner's answer completes the turn, and a runner that cannot answer
   * fails it with the reason it gave.
   */
  async run(
    sessionId: string,
    turnId: string,
    conversationId: string,
    model: { provider: string; modelId: string },
    runner: TurnRunner,
    now: number,
  ): Promise<TurnRunOutcome> {
    const claimed = await this.#repo.claimTurn(sessionId, turnId, now);
    if (!claimed.ok) {
      const current = (await this.#repo.turns(sessionId)).find((it) => it.id === turnId);
      return current === undefined
        ? { kind: "missing" }
        : { kind: "not-queued", status: current.status };
    }
    const turn = claimed.next;
    const result = await runner.run({
      sessionId,
      conversationId,
      prompt: turn.prompt,
      operationId: turn.idempotencyKey,
      model,
      now,
    });
    return result.kind === "answered"
      ? this.#complete(sessionId, turnId, result.text, now)
      : this.#fail(sessionId, turnId, result.reason, now);
  }

  async #complete(sessionId: string, turnId: string, text: string, now: number) {
    await this.#repo.transition(sessionId, turnId, { status: "running" }, now);
    await this.#repo.transition(
      sessionId,
      turnId,
      { status: "completed", stopReason: "stop" },
      now,
    );
    return { kind: "answered" as const, text };
  }

  async #fail(sessionId: string, turnId: string, error: string, now: number) {
    await this.#repo.transition(sessionId, turnId, { status: "failed", error }, now);
    return { kind: "failed" as const, error };
  }

  bind(sessionId: string, acpSessionId: string, gatewayId: string, now: number) {
    return this.#repo.bindAgentSession(sessionId, acpSessionId, gatewayId, now);
  }

  close(sessionId: string, now: number) {
    return this.#repo.close(sessionId, now);
  }
}

import type { AgentCapability } from "@agent/protocol";
import {
  NO_MODEL_REASON,
  offersModel,
  type Session,
  type Turn,
  type TurnUpdate,
} from "@agent/store";
import { effectOf, helloOf, nextPrompt, reportOf } from "./gateway-plane.ts";

/**
 * One gateway connection, as the control plane drives it.
 *
 * The socket is the caller's: this module takes what arrived, decides what it
 * means, and returns the frames to send. That keeps the decision testable
 * without a socket and lets the Worker and the dev server run the same code.
 */

/** What this connection knows about itself. */
export type GatewayConnection = {
  /** The session this gateway speaks for, once it has said hello. */
  sessionId: string | undefined;
};

/** The frames to send back, the connection's next state, and what settled. */
export type GatewayStep = {
  send: string[];
  connection: GatewayConnection;
  /** The turn this message settled, which is what the reply path answers. */
  settled?:
    | { sessionId: string; replyTo: string; text: string }
    | { sessionId: string; replyTo: string; error: string };
};

/** The object surface a gateway connection uses, which the session object fills. */
export interface GatewaySessions {
  session(sessionId: string): Promise<Session | undefined>;
  turns(sessionId: string): Promise<Turn[]>;
  transition(sessionId: string, turnId: string, update: TurnUpdate, now: number): Promise<unknown>;
  bind(sessionId: string, acpSessionId: string, gatewayId: string, now: number): Promise<unknown>;
  /** Keep what the agent said it can run, and return the updated session. */
  capability(sessionId: string, capability: AgentCapability, now: number): Promise<Session>;
}

/**
 * What to send when a gateway says which session it serves.
 *
 * A queued turn is claimed before its frame leaves, so a second connection is
 * never handed the same prompt. A turn already dispatched is not re-sent: the
 * gateway has no record of it, and its prompt would run twice.
 */
export async function onHello(
  sessions: GatewaySessions,
  sessionId: string,
  now: number,
): Promise<GatewayStep> {
  const session = await sessions.session(sessionId);
  if (session === undefined) return { send: [], connection: { sessionId } };
  const turns = await sessions.turns(sessionId);
  const frame = nextPrompt(session, turns);
  if (frame === undefined) return { send: [], connection: { sessionId } };
  await sessions.transition(sessionId, frame.turnId, { status: "dispatched" }, now);
  return { send: [frameText(frame)], connection: { sessionId } };
}

/**
 * What a gateway's message does. A bind records the ACP session id; a settle
 * moves the turn to its final state. Anything else is ignored, so an unknown
 * frame cannot disturb a run.
 */
export async function onMessage(
  sessions: GatewaySessions,
  connection: GatewayConnection,
  data: string,
  now: number,
): Promise<GatewayStep> {
  const parsed = parseJson(data);
  if (parsed === undefined) return { send: [], connection };

  const hello = helloOf(parsed);
  if (hello !== undefined) return onHello(sessions, hello, now);

  const report = reportOf(parsed);
  if (report === undefined) return { send: [], connection };
  const sessionId = connection.sessionId ?? report.sessionId;
  const session = await sessions.session(sessionId);
  if (session === undefined) return { send: [], connection: { sessionId } };
  const turns = await sessions.turns(sessionId);
  const effect = effectOf(report, session, turns);

  if (effect.kind === "bind") {
    await sessions.bind(sessionId, effect.acpSessionId, "", now);
    return { send: [], connection: { sessionId } };
  }
  if (effect.kind === "capability") {
    const updated = await sessions.capability(sessionId, effect.capability, now);
    // A `stdio` agent chooses its own model, so a routed model it does not
    // offer cannot be corrected here. A turn no gateway has claimed yet can
    // still be stopped, so it fails naming the real cause instead of running
    // on a model the route never intended.
    const refusal = await refusedModel(sessions, updated, effect.capability, now);
    return { send: [], connection: { sessionId }, ...refusal };
  }
  if (effect.kind === "settle") {
    await sessions.transition(sessionId, effect.turnId, effect.update, now);
    const replyTo = replyTargetOf(session);
    if (effect.update.status === "completed") {
      return {
        send: [],
        connection: { sessionId },
        settled: { sessionId, replyTo, text: effect.text ?? "" },
      };
    }
    if (effect.update.status === "failed") {
      return {
        send: [],
        connection: { sessionId },
        settled: { sessionId, replyTo, error: effect.update.error },
      };
    }
  }
  return { send: [], connection: { sessionId } };
}

/**
 * Stop a turn the agent's own capability rules out.
 *
 * Only a queued turn is stopped: a `stdio` agent already running a dispatched
 * turn chose its model itself, so rewriting that turn's outcome would report a
 * failure the agent never gave. A queued turn has not started, which is the one
 * moment the record can still say why it will not.
 */
async function refusedModel(
  sessions: GatewaySessions,
  session: Session,
  capability: AgentCapability,
  now: number,
): Promise<{ settled?: { sessionId: string; replyTo: string; error: string } }> {
  const { selection } = session;
  // The route is what names the model to check, and without one there is
  // nothing to refuse.
  if (selection === null) return {};
  // An agent that named no models said nothing about what it can run, so its
  // silence is not a refusal; only a stated list can rule a model out.
  if (!capability.statesModels) return {};
  if (offersModel(capability, selection.model.modelId)) return {};
  const turns = await sessions.turns(session.id);
  const queued = turns
    .filter((turn) => turn.status === "queued")
    .sort((a, b) => a.createdAt - b.createdAt)[0];
  if (queued === undefined) return {};
  const error = NO_MODEL_REASON(capability, selection.model.modelId);
  await sessions.transition(session.id, queued.id, { status: "failed", error }, now);
  return { settled: { sessionId: session.id, replyTo: replyTargetOf(session), error } };
}

/** Where a reply goes: the thread the session answers into. */
function replyTargetOf(session: Session): string {
  return session.thread.threadId;
}

/**
 * A frame as the JSON-RPC notification the gateway's codec reads. The gateway
 * parses every message with the ACP codec, so a bare object would be dropped.
 */
export function frameText(frame: { type: string; [key: string]: unknown }): string {
  return JSON.stringify({ jsonrpc: "2.0", method: "gateway/command", params: frame });
}

function parseJson(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

/** What a socket needs to expose for one gateway to be driven. */
export interface GatewaySocket {
  /** Send one frame to the gateway. */
  send(frame: string): void;
  /** The session this connection speaks for, once it has said hello. */
  sessionId: string | undefined;
}

/**
 * One message from a gateway, applied to the connection.
 *
 * The Worker and the dev server both call this, so the deployed path and the
 * local path cannot drift. Messages on one connection are serialized by the
 * caller, so a bind and a completed event that arrive together cannot both read
 * the session as it was before the bind; the returned promise is what a caller
 * chains.
 */
export async function handleSocketMessage(
  sessions: GatewaySessions,
  socket: GatewaySocket,
  raw: string,
  now: number,
  onSettled?: (
    settled:
      | { sessionId: string; replyTo: string; text: string }
      | { sessionId: string; replyTo: string; error: string },
  ) => Promise<void>,
): Promise<void> {
  const step = await onMessage(sessions, { sessionId: socket.sessionId }, raw, now);
  socket.sessionId = step.connection.sessionId;
  for (const frame of step.send) socket.send(frame);
  if (step.settled !== undefined && onSettled !== undefined) await onSettled(step.settled);
}

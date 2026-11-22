import type { Session, Turn, TurnUpdate } from "@agent/store";
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
 * Where a reply goes. A Linear session answers on its thread, which the store
 * keeps as the session's channel id; the thread id is the agent session.
 */
function replyTargetOf(session: Session): string {
  return session.thread.threadId;
}

/**
 * A frame as the JSON-RPC notification the gateway's codec reads. The gateway
 * parses every message with the ACP codec, so a bare object would be dropped.
 */
function frameText(frame: { type: string; [key: string]: unknown }): string {
  return JSON.stringify({ jsonrpc: "2.0", method: "gateway/command", params: frame });
}

function parseJson(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

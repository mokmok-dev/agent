import { AcpBridge } from "./bridge.ts";
import {
  agentExitFailure,
  agentStartFailureFor,
  encodeEvent,
  EventLog,
  helloFrame,
  identityOf,
  parseEdgeFrame,
} from "./edge.ts";
import { AgentSupervisor, localVmTarget } from "@agent/runtime";

const edgeUrl = process.env.EDGE_URL ?? "ws://localhost:8787/gateway";
const agentCommand = process.env.AGENT_COMMAND ?? "bun";
const agentArgs = (process.env.AGENT_ARGS ?? "apps/gateway/src/agent/stdio-agent.ts")
  .split(" ")
  .filter(Boolean);
const cwd = process.env.AGENT_CWD ?? process.cwd();
/** The session this gateway serves, so the control plane knows whose turn to send. */
const sessionId = process.env.AGENT_SESSION_ID;

/**
 * Where this gateway hosts its agent. A gateway runs on the VM, so it always
 * uses the VM target. The sandbox target is for a Durable Object that hosts
 * the agent directly; both targets satisfy the same port, so the supervisor
 * and the bridge below are identical either way.
 */
const target = localVmTarget();

const log = new EventLog(500);
const supervisor = new AgentSupervisor({ spawn: target.spawn });
let socket: WebSocket | undefined;

function flush(): void {
  const pending = log.unacknowledged();
  if (socket?.readyState !== WebSocket.OPEN || pending.length === 0) return;
  for (const event of pending) socket.send(encodeEvent(event));
  log.acknowledge();
}

/** Queue an event for the control plane, which is how a failure reaches a thread. */
function emit(event: Parameters<EventLog["append"]>[0]): void {
  // A turn's end is the one line an operator needs from a live run, so it is
  // printed whether or not a control plane is listening.
  if (event.type === "turn.completed") {
    console.error(`gateway: turn ${event.turnId} completed (${event.stopReason})`);
  } else if (event.type === "turn.failed") {
    console.error(`gateway: turn ${event.turnId} failed: ${event.error}`);
  }
  log.append(event);
  flush();
}

const bridge = new AcpBridge({
  channel: { send: (text) => supervisor.write(text) },
  // The agent decides the empty `mcpServers` shape: opencode2 refuses an array,
  // grok refuses a record. An operator states which agent the command starts.
  mcpServers: process.env.AGENT_MCP_SERVERS === "record" ? "record" : "array",
  emit,
});

/**
 * The turn this gateway is running, so an agent that dies mid-turn can fail it
 * rather than leave the thread waiting on a process that is gone.
 */
let inFlight: { sessionId: string; turnId: string } | undefined;

/**
 * Why the agent could not be started, when that is what happened.
 *
 * A gateway with no agent answers no prompt, so every turn it receives has to
 * fail with this reason. Recording it at startup is what turns a silent hang
 * into a sentence the thread can read.
 */
let agentStartFailure: string | undefined;

/**
 * Start the agent, reporting a binary that cannot run instead of dying.
 *
 * `spawn` throws synchronously when the command does not exist, and an
 * uncaught throw here kills the whole gateway: the operator sees a silent exit
 * and the session it was serving has no gateway. A missing agent is a
 * configuration fault to report, not a reason to take the process down.
 */
function startAgent(): void {
  try {
    supervisor.start(agentCommand, agentArgs, {
      onLine: (line) => bridge.receive(line),
      onExit: (code) => {
        socket?.send(JSON.stringify({ type: "gateway.agent_exited", code }));
        // An agent that exits while a turn is running leaves that turn
        // `running` forever: the prompt response never arrives, and nothing
        // else settles it. The exit is the only evidence of what happened, so
        // it becomes the turn's failure.
        const turn = inFlight;
        if (turn !== undefined) {
          inFlight = undefined;
          emit(agentExitFailure(turn, code));
        }
      },
    });
  } catch (error) {
    agentStartFailure = error instanceof Error ? error.message : String(error);
    console.error(`gateway: cannot start ${agentCommand}: ${agentStartFailure}`);
  }
}

startAgent();

let acpSessionId: string | undefined;
/** The session this connection serves, once the control plane has named it. */
let serving: string | undefined = sessionId;

/**
 * The control plane's answer to a hello. A gateway that asked for `auto` did not
 * know the session's id, so this is how it learns which one it reached; the
 * reply is sent as a gateway command, the same envelope as every other frame.
 */
function adoptIdentity(text: string): boolean {
  const named = identityOf(text);
  if (named === undefined) return false;
  serving = named;
  console.error(`gateway: serving session ${serving}`);
  // The gateway has just learned its id, so it asks again: the queued turn is
  // dispatched against the session it serves rather than the word "auto".
  socket?.send(JSON.stringify({ type: "hello", sessionId: serving }));
  return true;
}

/**
 * The ACP session this prompt runs in.
 *
 * The control plane stores the id the gateway reported, and sends it with every
 * later prompt, which is what makes a second mention continue the same agent
 * conversation. A gateway that restarted has no session of its own, so it tries
 * that id first. `session/load` is an agent's own decision: an agent that
 * refuses it, or no longer has the session, earns a fresh `session/new` rather
 * than a failed turn.
 */
async function adopt(frame: { sessionId: string; acpSessionId: string }): Promise<string> {
  if (frame.acpSessionId !== "") {
    const resumed = await bridge.resume(frame.sessionId, frame.acpSessionId, cwd);
    if (resumed !== undefined) {
      console.error(`gateway: resumed ACP session ${resumed}`);
      return resumed;
    }
    console.error(`gateway: agent did not resume ${frame.acpSessionId}; opening a new session`);
  }
  return bridge.open(frame.sessionId, cwd);
}

async function handleCommand(text: string): Promise<void> {
  if (adoptIdentity(text)) return;
  const frame = parseEdgeFrame(text);
  if (frame === undefined) return;
  switch (frame.type) {
    case "prompt":
      // A gateway whose agent never started can answer nothing. Saying so is
      // the difference between a thread that learns the agent is missing and
      // one that waits on a prompt no process will ever read.
      if (agentStartFailure !== undefined) {
        console.error(`gateway: turn ${frame.turnId} cannot run: ${agentStartFailure}`);
        emit(agentStartFailureFor(frame, agentStartFailure));
        return;
      }
      try {
        console.error(`gateway: turn ${frame.turnId} received`);
        if (acpSessionId === undefined) acpSessionId = await adopt(frame);
        console.error(`gateway: turn ${frame.turnId} running in ${acpSessionId}`);
        // The agent picked its own model when it opened the session; the route
        // may have chosen another, which only an agent that lists models takes.
        if (frame.model !== undefined) {
          const selected = await bridge.selectModel(acpSessionId, frame.model);
          console.error(
            `gateway: model ${frame.model.provider}:${frame.model.modelId} ${
              selected ? "selected on the agent" : "left to the agent's own choice"
            }`,
          );
        }
        inFlight = { sessionId: frame.sessionId, turnId: frame.turnId };
        await bridge.run(frame.sessionId, acpSessionId, frame.turnId, frame.text);
      } catch (error) {
        // A session that cannot open is a failed turn, not a dead gateway: the
        // thread has to learn why nothing ran, and the next prompt must still
        // be able to try again.
        acpSessionId = undefined;
        console.error(
          `gateway: turn ${frame.turnId} failed before it ran: ${
            error instanceof Error ? error.message : String(error)
          }`,
        );
        emit({
          type: "turn.failed",
          sessionId: frame.sessionId,
          turnId: frame.turnId,
          error: error instanceof Error ? error.message : String(error),
        });
      } finally {
        // A run that reported its own failure has already settled the turn, so
        // clearing this only stops the exit handler failing it a second time.
        inFlight = undefined;
      }
      return;
    case "cancel":
      bridge.cancel(frame.acpSessionId);
      return;
    case "permission":
      bridge.answer(frame.toolCallId, frame.optionId ?? undefined);
      return;
    case "ping":
      socket?.send(JSON.stringify({ type: "pong" }));
  }
}

const connect = (): void => {
  const opened = new WebSocket(edgeUrl);
  /** One connection's frames, handled in order. */
  let queue: Promise<void> = Promise.resolve();
  opened.addEventListener("open", () => {
    socket = opened;
    // Every connection announces itself, because a gateway that asked for
    // `auto` has no id yet and the control plane only sends a turn in answer to
    // a hello. Sending the literal `auto` is what asks the object to name the
    // session it resolved.
    opened.send(helloFrame(serving));
    flush();
  });
  opened.addEventListener("message", (message) => {
    // Frames are chained, not fired in parallel: the control plane can send the
    // prompt immediately after its hello, and two prompts racing would both see
    // no ACP session and start two agents.
    queue = queue.then(() => handleCommand(String(message.data)));
  });
  opened.addEventListener("close", () => {
    socket = undefined;
    // A gateway that asked for `auto` and never got a session was told there is
    // nothing waiting. The platform reports that as a plain close, so the
    // cadence follows what happened rather than a code: retrying a refusal once
    // a second would hammer the route until someone delegates.
    const refused = serving === undefined || serving === "auto";
    if (refused) {
      console.error("gateway: waiting for a session; no delegation has created one yet");
    }
    setTimeout(connect, refused ? 5_000 : 1_000);
  });
};

connect();

export { handleCommand, log, supervisor };

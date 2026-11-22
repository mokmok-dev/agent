import { AcpBridge } from "./bridge.ts";
import { encodeEvent, EventLog, helloFrame, identityOf, parseEdgeFrame } from "./edge.ts";
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
      },
    });
  } catch (error) {
    console.error(
      `gateway: cannot start ${agentCommand}: ${error instanceof Error ? error.message : String(error)}`,
    );
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

async function handleCommand(text: string): Promise<void> {
  if (adoptIdentity(text)) return;
  const frame = parseEdgeFrame(text);
  if (frame === undefined) return;
  switch (frame.type) {
    case "prompt":
      try {
        console.error(`gateway: turn ${frame.turnId} received`);
        if (acpSessionId === undefined) acpSessionId = await bridge.open(frame.sessionId, cwd);
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

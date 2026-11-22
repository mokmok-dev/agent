import { AcpBridge } from "./bridge.ts";
import { encodeEvent, EventLog, parseEdgeFrame } from "./edge.ts";
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

const bridge = new AcpBridge({
  channel: { send: (text) => supervisor.write(text) },
  // The agent decides the empty `mcpServers` shape: opencode2 refuses an array,
  // grok refuses a record. An operator states which agent the command starts.
  mcpServers: process.env.AGENT_MCP_SERVERS === "record" ? "record" : "array",
  emit: (event) => {
    log.append(event);
    flush();
  },
});

supervisor.start(agentCommand, agentArgs, {
  onLine: (line) => bridge.receive(line),
  onExit: (code) => {
    socket?.send(JSON.stringify({ type: "gateway.agent_exited", code }));
  },
});

let acpSessionId: string | undefined;
/** The session this connection serves, once the control plane has named it. */
let serving: string | undefined = sessionId;

/**
 * The control plane's answer to a hello. A gateway that asked for `auto` did not
 * know the session's id, so this is how it learns which one it reached; the
 * reply is sent as a gateway command, the same envelope as every other frame.
 */
function adoptIdentity(text: string): boolean {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return false;
  }
  const record = parsed as { params?: { type?: string; sessionId?: unknown } };
  if (record?.params?.type !== "hello" || typeof record.params.sessionId !== "string") {
    return false;
  }
  serving = record.params.sessionId;
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
      if (acpSessionId === undefined) acpSessionId = await bridge.open(frame.sessionId, cwd);
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
  opened.addEventListener("open", () => {
    socket = opened;
    // A gateway that names a session announces it, so the control plane sends
    // that session's queued turn rather than waiting to be asked. The control
    // plane answers the hello with the session it resolved, which is how a
    // gateway that asked for `auto` learns the id it is serving.
    if (sessionId !== undefined) {
      opened.send(JSON.stringify({ type: "hello", sessionId }));
    }
    flush();
  });
  opened.addEventListener("message", (message) => {
    void handleCommand(String(message.data));
  });
  opened.addEventListener("close", (event) => {
    socket = undefined;
    // A control plane that answered "no session is waiting" closes the socket
    // until a delegation creates one, so a fixed wait is right for that case
    // and for a network drop alike: retrying sooner would only repeat it.
    const code = (event as CloseEvent).code;
    const wait = code === 4004 || code === 1008 ? 5_000 : 1_000;
    if (code === 4004) {
      console.error("gateway: waiting for a session; no delegation has created one yet");
    }
    setTimeout(connect, wait);
  });
};

connect();

export { handleCommand, log, supervisor };

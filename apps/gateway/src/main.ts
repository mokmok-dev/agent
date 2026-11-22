import { AcpBridge } from "./bridge.ts";
import { encodeEvent, EventLog, parseEdgeFrame } from "./edge.ts";
import { AgentSupervisor, localVmTarget } from "@agent/runtime";

const edgeUrl = process.env.EDGE_URL ?? "ws://localhost:8787/gateway";
const agentCommand = process.env.AGENT_COMMAND ?? "bun";
const agentArgs = (process.env.AGENT_ARGS ?? "apps/gateway/src/agent/stdio-agent.ts")
  .split(" ")
  .filter(Boolean);
const cwd = process.env.AGENT_CWD ?? process.cwd();

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

async function handleCommand(text: string): Promise<void> {
  const frame = parseEdgeFrame(text);
  if (frame === undefined) return;
  switch (frame.type) {
    case "prompt":
      if (acpSessionId === undefined) acpSessionId = await bridge.open(frame.sessionId, cwd);
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

function connect(): void {
  const opened = new WebSocket(edgeUrl);
  opened.addEventListener("open", () => {
    socket = opened;
    flush();
  });
  opened.addEventListener("message", (message) => {
    void handleCommand(String(message.data));
  });
  opened.addEventListener("close", () => {
    socket = undefined;
    setTimeout(connect, 1000);
  });
}

connect();

export { handleCommand, log, supervisor };

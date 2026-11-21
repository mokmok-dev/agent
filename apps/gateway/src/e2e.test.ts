import { describe, expect, test } from "bun:test";
import { AcpBridge, type GatewayEvent } from "./bridge.ts";
import { spawnProcess } from "./agent/proc.ts";
import { AgentSupervisor } from "./supervisor.ts";

const AGENT = new URL("./agent/stdio-agent.ts", import.meta.url).pathname;

/** Boot a supervisor plus bridge against the real scripted agent. */
function boot(script: string) {
  const events: GatewayEvent[] = [];
  const bridge = new AcpBridge({
    channel: { send: (text) => supervisor.write(text) },
    emit: (event) => events.push(event),
  });
  const supervisor = new AgentSupervisor({
    spawn: (command, args) => spawnProcess(command, args, { ...process.env, AGENT_SCRIPT: script }),
    maxRestarts: 0,
  });
  supervisor.start("bun", [AGENT], { onLine: (line) => bridge.receive(line) });
  return { events, bridge, supervisor };
}

/** Wait until an event of `type` appears, so a slow process does not race the assertion. */
async function waitFor<T extends GatewayEvent["type"]>(
  events: GatewayEvent[],
  type: T,
  attempts = 50,
): Promise<Extract<GatewayEvent, { type: T }>> {
  for (let attempt = 0; attempt < attempts; attempt++) {
    const found = events.find((event) => event.type === type);
    if (found) return found as Extract<GatewayEvent, { type: T }>;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error(`no ${type} event; saw ${events.map((event) => event.type).join(", ")}`);
}

describe("end to end against the real scripted agent", () => {
  test("initialize, open, and one prompt round trip", async () => {
    const { events, bridge, supervisor } = boot("echo");
    const opened = await bridge.open("sess-1", process.cwd());
    expect(opened).toBe("acp-session-1");
    await bridge.run("sess-1", opened, "turn-1", "hello agent");
    const completed = await waitFor(events, "turn.completed");
    supervisor.stop();

    expect(events.map((event) => event.type)).toEqual([
      "session.bound",
      "turn.started",
      "turn.update",
      "turn.completed",
    ]);
    expect(events.find((event) => event.type === "turn.update")).toMatchObject({
      turnId: "turn-1",
      update: {
        sessionUpdate: "agent_message_chunk",
        content: { type: "text", text: "hello agent" },
      },
    });
    expect(completed).toMatchObject({ stopReason: "end_turn" });
  });

  test("a permission prompt pauses the turn until it is answered", async () => {
    const { events, bridge, supervisor } = boot("permission");
    const opened = await bridge.open("sess-1", process.cwd());
    const running = bridge.run("sess-1", opened, "turn-1", "write it");
    const asked = await waitFor(events, "permission.asked");
    expect(asked).toMatchObject({ toolCallId: "call-1", options: ["allow", "deny"] });
    expect(events.some((event) => event.type === "turn.completed")).toBe(false);

    bridge.answer("call-1", "allow");
    await running;
    const completed = await waitFor(events, "turn.completed");
    supervisor.stop();
    expect(completed).toMatchObject({ stopReason: "end_turn" });
    expect(events.filter((event) => event.type === "turn.update")).toHaveLength(1);
  });

  test("a declined permission completes with no message", async () => {
    const { events, bridge, supervisor } = boot("permission");
    const opened = await bridge.open("sess-1", process.cwd());
    const running = bridge.run("sess-1", opened, "turn-1", "write it");
    await waitFor(events, "permission.asked");
    bridge.answer("call-1", undefined);
    await running;
    await waitFor(events, "turn.completed");
    supervisor.stop();
    expect(events.filter((event) => event.type === "turn.update")).toHaveLength(0);
  });

  test("an agent error fails the turn with the agent's code", async () => {
    const { events, bridge, supervisor } = boot("fail");
    const opened = await bridge.open("sess-1", process.cwd());
    await bridge.run("sess-1", opened, "turn-1", "go");
    const failed = await waitFor(events, "turn.failed");
    supervisor.stop();
    expect(failed).toMatchObject({ error: "-32603: agent refused" });
  });

  test("the supervisor kills a real process", async () => {
    const child = spawnProcess("bun", [AGENT], { ...process.env, AGENT_SCRIPT: "echo" });
    expect(child.stdin).not.toBeNull();
    child.kill();
  });
});

#!/usr/bin/env bun
/**
 * A scripted ACP agent for the end-to-end test. It reads one JSON-RPC frame
 * per line on stdin and answers on stdout, so the gateway exercises the real
 * codec and the real process boundary.
 *
 * `AGENT_SCRIPT` selects what the agent does with a prompt:
 * - `echo` sends one agent message chunk holding the prompt text.
 * - `permission` asks for permission first, then sends the message.
 * - `fail` answers the prompt with an error.
 */
import {
  decodeMessage,
  encodeErrorResponse,
  encodeNotification,
  encodeRequest,
  encodeResult,
  RpcErrorCode,
} from "@agent/protocol";

const script = process.env.AGENT_SCRIPT ?? "echo";
const stdin = Bun.stdin.stream().getReader();
const reader = stdin;
const decoder = new TextDecoder();
let buffer = "";
let nextId = 1000;

function write(text: string): void {
  process.stdout.write(`${text}\n`);
}

function sessionUpdate(sessionId: string, update: unknown): void {
  write(encodeNotification("session/update", { sessionId, update }));
}

function chunk(sessionId: string, text: string): void {
  sessionUpdate(sessionId, {
    sessionUpdate: "agent_message_chunk",
    content: { type: "text", text },
  });
}

async function askPermission(sessionId: string, prompt: string): Promise<void> {
  const id = nextId++;
  write(
    encodeRequest(id, "session/request_permission", {
      sessionId,
      toolCall: { toolCallId: "call-1", title: "Write the answer", kind: "edit" },
      options: [
        { optionId: "allow", name: "Allow", kind: "allow_once" },
        { optionId: "deny", name: "Deny", kind: "reject_once" },
      ],
    }),
  );
  const answer = await waiter(id);
  if (answer === undefined) return;
  chunk(sessionId, prompt);
}

const waiters = new Map<number, (optionId: string | undefined) => void>();

function waiter(id: number): Promise<string | undefined> {
  return new Promise((resolve) => waiters.set(id, resolve));
}

async function handleFrame(line: string): Promise<void> {
  const message = decodeMessage(line);
  if (message.kind === "result") {
    if (typeof message.id === "number") {
      const resolve = waiters.get(message.id);
      if (resolve) {
        waiters.delete(message.id);
        const result = message.result as { outcome?: string; optionId?: string } | null;
        resolve(result?.outcome === "selected" ? result.optionId : undefined);
      }
    }
    return;
  }
  if (message.kind !== "request") return;
  switch (message.method) {
    case "initialize":
      write(
        encodeResult(message.id, { protocolVersion: 1, agentCapabilities: { loadSession: true } }),
      );
      return;
    case "session/new":
      write(encodeResult(message.id, { sessionId: "acp-session-1" }));
      return;
    case "session/prompt": {
      const params = message.params as { sessionId: string; prompt: Array<{ text?: string }> };
      const prompt = params.prompt.map((block) => block.text ?? "").join("");
      if (script === "fail") {
        write(
          encodeErrorResponse(message.id, {
            code: RpcErrorCode.InternalError,
            message: "agent refused",
          }),
        );
        return;
      }
      if (script === "permission") await askPermission(params.sessionId, prompt);
      else chunk(params.sessionId, prompt);
      write(encodeResult(message.id, { stopReason: "end_turn" }));
      return;
    }
    case "session/cancel":
      return;
    default:
      write(
        encodeErrorResponse(message.id, {
          code: RpcErrorCode.MethodNotFound,
          message: message.method,
        }),
      );
  }
}

for (;;) {
  const { value, done } = await reader.read();
  if (done) break;
  buffer += decoder.decode(value, { stream: true });
  let newline = buffer.indexOf("\n");
  while (newline >= 0) {
    const line = buffer.slice(0, newline).trim();
    buffer = buffer.slice(newline + 1);
    // A prompt may wait for a permission reply, so framing must keep reading.
    if (line !== "") void handleFrame(line);
    newline = buffer.indexOf("\n");
  }
}

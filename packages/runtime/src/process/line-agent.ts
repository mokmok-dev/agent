#!/usr/bin/env bun
/**
 * A minimal line agent for the runtime conformance test. It answers every
 * request with its method name, so both targets can prove they carry one frame
 * in and one frame out. It reads one JSON object per line on stdin.
 */
const reader = Bun.stdin.stream().getReader();
const decoder = new TextDecoder();
let buffer = "";

for (;;) {
  const { value, done } = await reader.read();
  if (done) break;
  buffer += decoder.decode(value, { stream: true });
  let newline = buffer.indexOf("\n");
  while (newline >= 0) {
    const line = buffer.slice(0, newline).trim();
    buffer = buffer.slice(newline + 1);
    if (line !== "") answer(line);
    newline = buffer.indexOf("\n");
  }
}

function answer(line: string): void {
  const request = JSON.parse(line) as { id?: number; method?: string };
  process.stdout.write(
    `${JSON.stringify({ jsonrpc: "2.0", id: request.id, result: { protocolVersion: 1, method: request.method } })}\n`,
  );
}

export {};

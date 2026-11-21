export class LineFramer {
  #buffer = "";
  readonly #decoder = new TextDecoder();

  push(chunk: Uint8Array): string[] {
    this.#buffer += this.#decoder.decode(chunk, { stream: true });
    const parts = this.#buffer.split("\n");
    const complete = parts.slice(0, -1);
    this.#buffer = parts[parts.length - 1]!;
    return complete.map((part) => part.trim()).filter((line) => line !== "");
  }

  flush(): string | undefined {
    const rest = this.#buffer.trim();
    this.#buffer = "";
    return rest === "" ? undefined : rest;
  }
}

export async function readLines(
  stream: ReadableStream<Uint8Array>,
  listener: (line: string) => void,
): Promise<void> {
  const reader = stream.getReader();
  const framer = new LineFramer();
  for (;;) {
    const chunk = await reader.read();
    if (chunk.done) break;
    for (const line of framer.push(chunk.value)) listener(line);
  }
  const rest = framer.flush();
  if (rest !== undefined) listener(rest);
}

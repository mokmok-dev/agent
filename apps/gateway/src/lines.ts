/**
 * Split a byte stream into lines. A line is complete at `\n`; the text after
 * the last newline is held until the stream ends or another chunk arrives, so
 * a frame split across reads is reassembled.
 */
export class LineFramer {
  #buffer = "";
  readonly #decoder = new TextDecoder();

  /** Feed one chunk and get the lines it completes. */
  push(chunk: Uint8Array): string[] {
    this.#buffer += this.#decoder.decode(chunk, { stream: true });
    // Only complete lines are taken; whatever follows the last newline stays.
    const parts = this.#buffer.split("\n");
    const complete = parts.slice(0, -1);
    this.#buffer = parts[parts.length - 1]!;
    return complete.map((part) => part.trim()).filter((line) => line !== "");
  }

  /** The trailing text with no newline yet, if any. */
  flush(): string | undefined {
    const rest = this.#buffer.trim();
    this.#buffer = "";
    return rest === "" ? undefined : rest;
  }
}

/** Read a byte stream to its end, handing every complete line to `listener`. */
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

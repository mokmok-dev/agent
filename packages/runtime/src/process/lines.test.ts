import { describe, expect, test } from "bun:test";
import { LineFramer, readLines } from "./lines.ts";

const encoder = new TextEncoder();

describe("LineFramer", () => {
  test("emits the line a chunk completes", () => {
    const framer = new LineFramer();
    expect(framer.push(encoder.encode("one\n"))).toEqual(["one"]);
  });

  test("emits several lines from one chunk", () => {
    const framer = new LineFramer();
    expect(framer.push(encoder.encode("one\ntwo\nthree\n"))).toEqual(["one", "two", "three"]);
  });

  test("holds a partial line until the next chunk completes it", () => {
    const framer = new LineFramer();
    expect(framer.push(encoder.encode("par"))).toEqual([]);
    expect(framer.push(encoder.encode("tial\n"))).toEqual(["partial"]);
  });

  test("holds the text after the last newline", () => {
    const framer = new LineFramer();
    expect(framer.push(encoder.encode("done\nrest"))).toEqual(["done"]);
    expect(framer.push(encoder.encode(" more\n"))).toEqual(["rest more"]);
  });

  test("trims surrounding whitespace from a line", () => {
    const framer = new LineFramer();
    expect(framer.push(encoder.encode("  padded \n"))).toEqual(["padded"]);
  });

  test("drops an empty line", () => {
    const framer = new LineFramer();
    expect(framer.push(encoder.encode("\n\ntwo\n"))).toEqual(["two"]);
  });

  test("flush returns the trailing text and clears it", () => {
    const framer = new LineFramer();
    framer.push(encoder.encode("no newline"));
    expect(framer.flush()).toBe("no newline");
    expect(framer.flush()).toBeUndefined();
  });

  test("flush returns undefined when the buffer is only whitespace", () => {
    const framer = new LineFramer();
    framer.push(encoder.encode("  "));
    expect(framer.flush()).toBeUndefined();
  });

  test("flush trims its text", () => {
    const framer = new LineFramer();
    framer.push(encoder.encode("  tail  "));
    expect(framer.flush()).toBe("tail");
  });

  test("a multibyte character split across chunks is reassembled", () => {
    const framer = new LineFramer();
    const bytes = encoder.encode("é\n");
    expect(framer.push(bytes.slice(0, 1))).toEqual([]);
    expect(framer.push(bytes.slice(1))).toEqual(["é"]);
  });

  test("an empty chunk completes nothing", () => {
    const framer = new LineFramer();
    expect(framer.push(new Uint8Array())).toEqual([]);
  });
});

describe("readLines", () => {
  const stream = (chunks: string[]): ReadableStream<Uint8Array> =>
    new ReadableStream({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
        controller.close();
      },
    });

  const collect = async (chunks: string[]): Promise<string[]> => {
    const lines: string[] = [];
    await readLines(stream(chunks), (line) => lines.push(line));
    return lines;
  };

  test("reads every line across chunks", async () => {
    expect(await collect(["one\n", "two\n", "three\n"])).toEqual(["one", "two", "three"]);
  });

  test("reassembles a line split across chunks", async () => {
    expect(await collect(["par", "tial\nnext\n"])).toEqual(["partial", "next"]);
  });

  test("emits the trailing text at end of stream", async () => {
    expect(await collect(["one\nrest"])).toEqual(["one", "rest"]);
  });

  test("an empty stream emits nothing", async () => {
    expect(await collect([])).toEqual([]);
  });

  test("a stream of blank lines emits nothing", async () => {
    expect(await collect(["\n\n\n"])).toEqual([]);
  });
});

describe("multiple trailing lines", () => {
  test("advances past each newline, not by one character", () => {
    const framer = new LineFramer();
    expect(framer.push(encoder.encode("aaa\nbbb\nccc\n"))).toEqual(["aaa", "bbb", "ccc"]);
  });

  test("reassembles after a middle chunk ends exactly on a newline", () => {
    const framer = new LineFramer();
    expect(framer.push(encoder.encode("a\nb\n"))).toEqual(["a", "b"]);
    expect(framer.push(encoder.encode("c\n"))).toEqual(["c"]);
  });

  test("a chunk that ends exactly on a newline leaves nothing buffered", () => {
    const framer = new LineFramer();
    expect(framer.push(encoder.encode("a\n"))).toEqual(["a"]);
    expect(framer.flush()).toBeUndefined();
  });
});

describe("stream completion", () => {
  test("the read loop stops at the end of the stream", async () => {
    const lines: string[] = [];
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(encoder.encode("last\n"));
        controller.close();
      },
    });
    await readLines(stream, (line) => lines.push(line));
    expect(lines).toEqual(["last"]);
  });
});

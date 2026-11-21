import { afterEach, describe, expect, test } from "bun:test";
import { DiskHost } from "./disk-host.ts";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const dirs: string[] = [];

const host = (): DiskHost => {
  const dir = mkdtempSync(join(tmpdir(), "agent-state-"));
  dirs.push(dir);
  return new DiskHost(join(dir, "objects"));
};

afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("DiskHost", () => {
  test("a written value reads back", async () => {
    const storage = host();
    await storage.put("session:s1", { id: "s1", nested: { ok: true } });
    expect(await storage.get("session:s1")).toEqual({ id: "s1", nested: { ok: true } });
  });

  test("a missing key reads as undefined", async () => {
    expect(await host().get("nope")).toBeUndefined();
  });

  test("a corrupt file reads as undefined rather than throwing", async () => {
    const storage = host();
    await storage.put("bad", { a: 1 });
    const path = storage.pathFor("bad");
    await Bun.write(path, "{not json");
    expect(await storage.get("bad")).toBeUndefined();
  });

  test("delete removes the value and is idempotent", async () => {
    const storage = host();
    await storage.put("k", 1);
    await storage.delete("k");
    expect(await storage.get("k")).toBeUndefined();
    await storage.delete("k");
  });

  test("list returns only the entries under the prefix", async () => {
    const storage = host();
    await storage.put("turn:s1:t1", { id: "t1" });
    await storage.put("turn:s1:t2", { id: "t2" });
    await storage.put("turn:s2:t1", { id: "other" });
    const listed = await storage.list<{ id: string }>({ prefix: "turn:s1:" });
    expect([...listed.keys()].sort()).toEqual(["turn:s1:t1", "turn:s1:t2"]);
  });

  test("a key with a colon and a space stays one file", async () => {
    const storage = host();
    const key = "thread:slack:T 1:C:1.5";
    await storage.put(key, "value");
    expect(await storage.get(key)).toBe("value");
    expect(await storage.list({ prefix: "thread:" })).toHaveProperty("size", 1);
  });

  test("two objects in different directories do not see each other", async () => {
    const root = mkdtempSync(join(tmpdir(), "agent-state-"));
    dirs.push(root);
    const first = new DiskHost(join(root, "a"));
    const second = new DiskHost(join(root, "b"));
    await first.put("k", 1);
    expect(await second.get("k")).toBeUndefined();
  });

  test("a JSON value of any shape round trips", async () => {
    const storage = host();
    for (const value of [null, 0, "", [], { deep: [1, 2] }, true]) {
      await storage.put("v", value);
      expect(await storage.get("v")).toEqual(value);
    }
  });
});

describe("files and decoding", () => {
  test("a corrupt file makes list skip only that entry", async () => {
    const storage = host();
    await storage.put("keep", 1);
    await storage.put("broken", 2);
    await Bun.write(storage.pathFor("broken"), "{nope");
    const listed = await storage.list<number>({ prefix: "" });
    expect([...listed.keys()]).toEqual(["keep"]);
    expect(listed.size).toBe(1);
  });

  test("the suffix is stripped from a file name", async () => {
    const storage = host();
    await storage.put("plain", "v");
    const listed = await storage.list<string>({ prefix: "" });
    expect([...listed.keys()]).toEqual(["plain"]);
    expect(listed.has("plain.json")).toBe(false);
  });

  test("a key encoded in a file name comes back decoded", async () => {
    const storage = host();
    await storage.put("a:b", 1);
    expect([...(await storage.list({ prefix: "" })).keys()]).toEqual(["a:b"]);
  });

  test("the constructor creates the directory it will write into", async () => {
    const root = mkdtempSync(join(tmpdir(), "agent-state-"));
    dirs.push(root);
    const nested = new DiskHost(join(root, "deep", "deeper"));
    await nested.put("k", 1);
    expect(await nested.get("k")).toBe(1);
  });
});

describe("reading without an encoding name", () => {
  test("a file with multibyte content reads as UTF-8 text", async () => {
    const storage = host();
    await storage.put("unicode", { text: "こんにちは 🙂" });
    expect(await storage.get("unicode")).toEqual({ text: "こんにちは 🙂" });
  });

  test("a JSON string value reads back as the same string", async () => {
    const storage = host();
    await storage.put("quoted", 'a "quoted" value');
    expect(await storage.get("quoted")).toBe('a "quoted" value');
  });
});

describe("unreadable files", () => {
  test("a directory where a file should be reads as undefined", async () => {
    const root = mkdtempSync(join(tmpdir(), "agent-state-"));
    dirs.push(root);
    const storage = new DiskHost(root);
    const { mkdirSync } = await import("node:fs");
    mkdirSync(storage.pathFor("dir"), { recursive: true });
    expect(await storage.get("dir")).toBeUndefined();
  });
});

describe("decoding", () => {
  test("a corrupt file and an absent file both read as undefined", async () => {
    const storage = host();
    await storage.put("bad", 1);
    await Bun.write(storage.pathFor("bad"), "not json");
    expect(await storage.get("bad")).toBeUndefined();
    expect(await storage.get("absent")).toBeUndefined();
  });

  test("a JSON null is a value, so it reads back as null", async () => {
    const storage = host();
    await storage.put("null", null);
    expect(await storage.get("null")).toBeNull();
  });

  test("writing twice keeps the second value", async () => {
    const storage = host();
    await storage.put("k", 1);
    await storage.put("k", 2);
    expect(await storage.get("k")).toBe(2);
  });
});

import { beforeAll, describe, expect, test } from "bun:test";
import {
  adaptDiscord,
  deferredAck,
  DiscordInteractionType,
  parseDiscordInteraction,
  verifyDiscord,
} from "./discord.ts";
import { AdapterError, type RawRequest } from "./events.ts";

const encoder = new TextEncoder();

let publicKeyHex: string;
let privateKey: CryptoKey;

beforeAll(async () => {
  const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, [
    "sign",
    "verify",
  ])) as unknown as CryptoKeyPair;
  privateKey = pair.privateKey;
  const raw = (await crypto.subtle.exportKey("raw", pair.publicKey)) as ArrayBuffer;
  publicKeyHex = [...new Uint8Array(raw)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
});

async function signed(body: unknown, timestamp = "1700000000"): Promise<RawRequest> {
  const text = JSON.stringify(body);
  const signature = await crypto.subtle.sign(
    { name: "Ed25519" },
    privateKey,
    encoder.encode(timestamp + text),
  );
  const hex = [...new Uint8Array(signature)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
  return {
    method: "POST",
    headers: { "x-signature-ed25519": hex, "x-signature-timestamp": timestamp },
    body: text,
  };
}

async function rejection(promise: Promise<unknown>): Promise<AdapterError> {
  try {
    await promise;
  } catch (error) {
    expect(error).toBeInstanceOf(AdapterError);
    return error as AdapterError;
  }
  throw new Error("expected an AdapterError");
}

const commandBody = (overrides: Record<string, unknown> = {}) => ({
  id: "int-1",
  type: DiscordInteractionType.ApplicationCommand,
  channel_id: "C1",
  channel: { id: "C1", guild_id: "G1" },
  member: { user: { id: "U1" } },
  data: { options: [{ name: "prompt", value: "write the tests" }] },
  ...overrides,
});

describe("verifyDiscord", () => {
  test("accepts a correctly signed interaction", async () => {
    await verifyDiscord(await signed(commandBody()), { publicKey: publicKeyHex });
  });

  test("rejects a missing signature header", async () => {
    const request = await signed(commandBody());
    expect(
      (await rejection(verifyDiscord({ ...request, headers: {} }, { publicKey: publicKeyHex })))
        .status,
    ).toBe(401);
  });

  test("rejects a missing timestamp header", async () => {
    const request = await signed(commandBody());
    const headers = { "x-signature-ed25519": request.headers["x-signature-ed25519"] };
    expect(
      (await rejection(verifyDiscord({ ...request, headers }, { publicKey: publicKeyHex }))).status,
    ).toBe(401);
  });

  test("rejects a body changed after signing", async () => {
    const request = await signed(commandBody());
    const tampered = {
      ...request,
      body: request.body.replace("write the tests", "delete the tests"),
    };
    expect((await rejection(verifyDiscord(tampered, { publicKey: publicKeyHex }))).status).toBe(
      401,
    );
  });

  test("rejects a signature with another application key", async () => {
    const other = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, [
      "sign",
      "verify",
    ])) as unknown as CryptoKeyPair;
    const raw = (await crypto.subtle.exportKey("raw", other.publicKey)) as ArrayBuffer;
    const otherHex = [...new Uint8Array(raw)]
      .map((byte) => byte.toString(16).padStart(2, "0"))
      .join("");
    expect(
      (await rejection(verifyDiscord(await signed(commandBody()), { publicKey: otherHex }))).status,
    ).toBe(401);
  });
});

describe("parseDiscordInteraction", () => {
  test("answers a PING with pong", () => {
    expect(parseDiscordInteraction({ type: DiscordInteractionType.Ping })).toEqual({
      kind: "challenge",
      response: "pong",
    });
  });

  test("turns a slash command into a message", () => {
    expect(parseDiscordInteraction(commandBody())).toEqual({
      kind: "event",
      event: {
        kind: "message",
        provider: "discord",
        eventId: "int-1",
        thread: { workspaceId: "G1", channelId: "C1", threadId: "C1" },
        authorId: "U1",
        text: "write the tests",
        replyTo: { channelId: "C1", threadId: "C1" },
      },
    });
  });

  test("a stop command becomes a cancel", () => {
    const body = commandBody({ data: { options: [{ name: "prompt", value: "stop" }] } });
    const result = parseDiscordInteraction(body);
    expect(result.kind === "event" && result.event.kind).toBe("cancel");
  });

  test("a button press uses its custom id as the prompt", () => {
    const body = commandBody({
      type: DiscordInteractionType.MessageComponent,
      data: { custom_id: "retry" },
    });
    const result = parseDiscordInteraction(body);
    expect(result.kind === "event" && result.event.kind === "message" && result.event.text).toBe(
      "retry",
    );
  });

  test("a direct message has no guild", () => {
    const body = commandBody({ channel: { id: "C2" }, channel_id: "C2" });
    const result = parseDiscordInteraction(body);
    expect(
      result.kind === "event" && result.event.kind === "message" && result.event.thread.workspaceId,
    ).toBe("");
  });

  test("reads the user id from the top level when there is no member", () => {
    const body = commandBody({ member: undefined, user: { id: "U9" } });
    const result = parseDiscordInteraction(body);
    expect(result.kind === "event" && result.event.authorId).toBe("U9");
  });

  test("an interaction with no author has an empty author id", () => {
    const body = commandBody({ member: undefined });
    const result = parseDiscordInteraction(body);
    expect(result.kind === "event" && result.event.authorId).toBe("");
  });

  test("falls back to the channel id when channel_id is absent", () => {
    const body = commandBody({ channel_id: undefined });
    const result = parseDiscordInteraction(body);
    expect(
      result.kind === "event" && result.event.kind === "message" && result.event.thread.threadId,
    ).toBe("C1");
  });

  test("rejects a payload that is not an interaction", () => {
    for (const body of [null, [], {}, { type: "x" }]) {
      expect(() => parseDiscordInteraction(body)).toThrow(AdapterError);
    }
  });

  test("rejects an interaction with no channel or id", () => {
    expect(() => parseDiscordInteraction(commandBody({ channel: {} }))).toThrow(AdapterError);
    expect(() => parseDiscordInteraction(commandBody({ id: undefined }))).toThrow(AdapterError);
  });

  test("rejects an interaction with no prompt text", () => {
    expect(() => parseDiscordInteraction(commandBody({ data: {} }))).toThrow(AdapterError);
    expect(() => parseDiscordInteraction(commandBody({ data: { options: [] } }))).toThrow(
      AdapterError,
    );
    expect(() =>
      parseDiscordInteraction(commandBody({ data: { options: [{ name: "other", value: "x" }] } })),
    ).toThrow(AdapterError);
    expect(() =>
      parseDiscordInteraction(commandBody({ data: { options: [{ name: "prompt", value: 1 }] } })),
    ).toThrow(AdapterError);
  });
});

describe("adaptDiscord", () => {
  test("verifies then parses", async () => {
    const result = await adaptDiscord(await signed(commandBody()), { publicKey: publicKeyHex });
    expect(result.kind).toBe("event");
  });

  test("a signature failure is a 401 before parsing", async () => {
    const request = { ...(await signed(commandBody())), body: "{" };
    expect((await rejection(adaptDiscord(request, { publicKey: publicKeyHex }))).status).toBe(401);
  });

  test("a signed body that is not JSON is a 400", async () => {
    const text = "{ nope";
    const timestamp = "1700000000";
    const signature = await crypto.subtle.sign(
      { name: "Ed25519" },
      privateKey,
      encoder.encode(timestamp + text),
    );
    const hex = [...new Uint8Array(signature)]
      .map((byte) => byte.toString(16).padStart(2, "0"))
      .join("");
    const request = {
      method: "POST",
      headers: { "x-signature-ed25519": hex, "x-signature-timestamp": timestamp },
      body: text,
    };
    expect((await rejection(adaptDiscord(request, { publicKey: publicKeyHex }))).status).toBe(400);
  });

  test("the deferred acknowledgement is interaction type 5", async () => {
    const response = deferredAck();
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ type: 5 });
  });
});

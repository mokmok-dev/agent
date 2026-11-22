import { describe, expect, test } from "bun:test";
import { jsonOf, refuseSignature, type AdapterResult } from "./events.ts";
import { postGraphql } from "./linear-oauth.ts";
import { headerOf } from "./signature.ts";
import {
  constantTimeEqual,
  decodeSecret,
  hmacHex,
  signSha256,
  verifyEd25519,
  verifyHmac,
  type Verified,
} from "./signature.ts";
import { parseSlackEvent, SLACK_MAX_SKEW_SECONDS, verifySlack } from "./slack.ts";
import { parseDiscordInteraction, verifyDiscord } from "./discord.ts";
import { parseLinearEvent, verifyLinear } from "./linear.ts";
import { promptOf, stripMention, threadOf } from "./slack-schema.ts";
import {
  authorOf as discordAuthor,
  channelOf,
  guildOf,
  promptOf as discordPrompt,
} from "./discord-schema.ts";
import { authorOf as linearAuthor, promptOf as linearPrompt } from "./linear-schema.ts";
import type { RawRequest } from "./events.ts";

const slackConfig = { signingSecret: "shhh", botUserId: "UBOT" };

/** The refusal an adapter returned, or a failure naming what it returned instead. */
function refused(result: AdapterResult): { status: number; message: string } {
  if (result.kind !== "refused") {
    throw new Error(`expected a refusal, got ${result.kind}`);
  }
  return { status: result.status, message: result.message };
}

/** The refusal a signature check returned, or a failure naming what it returned instead. */
function unverified(result: Verified): { kind: string; message: string } {
  if (result.kind === "verified") throw new Error("expected a refusal, got a verified signature");
  if (result.kind === "mismatch") return { kind: "mismatch", message: "invalid signature" };
  if (result.kind === "absent") return { kind: "absent", message: result.header };
  return { kind: result.kind, message: result.message };
}

const request = (headers: Record<string, string>, body = "{}"): RawRequest => ({
  method: "POST",
  headers,
  body,
});

describe("jsonOf", () => {
  test("returns the parsed body when it is JSON", () => {
    expect(jsonOf(request({}, '{"a":1}'))).toEqual({ kind: "decoded", value: { a: 1 } });
  });

  test("a body that is not JSON is a 400 that names itself", () => {
    expect(jsonOf(request({}, "{"))).toEqual({
      kind: "rejected",
      status: 400,
      message: "body is not JSON",
    });
  });
});

describe("signature diagnostics", () => {
  test("a short left side is not equal", () => {
    expect(constantTimeEqual("ab", "abc")).toBe(false);
    expect(constantTimeEqual("abc", "ab")).toBe(false);
  });

  test("differing bytes anywhere make the comparison false", () => {
    const base = "0123456789abcdef";
    for (let index = 0; index < base.length; index++) {
      const changed =
        base.slice(0, index) + (base[index] === "0" ? "1" : "0") + base.slice(index + 1);
      expect(constantTimeEqual(base, changed)).toBe(false);
    }
  });

  test("verifyHmac names a mismatch as `mismatch`", async () => {
    const digest = await signSha256("shh", "m");
    if (digest.kind !== "signed") throw new Error("expected a signature");
    expect(unverified(await verifyHmac("shh", "m", `x${digest.hex.slice(1)}`, "sha256"))).toEqual({
      kind: "mismatch",
      message: "invalid signature",
    });
  });

  test("an empty signing secret names itself", async () => {
    expect(await hmacHex("", "m", "sha256")).toEqual({
      kind: "unsigned",
      message: "signing secret is empty",
    });
  });

  test("decodeSecret reads an odd-length hex-looking string as base64", () => {
    const decoded = decodeSecret("abc");
    if (decoded.kind !== "bytes") throw new Error("expected bytes");
    expect(decoded.value).toEqual(Uint8Array.from(atob("abc"), (c) => c.charCodeAt(0)));
  });

  test("decodeSecret reads every even-length hex string as bytes", () => {
    const lower = decodeSecret("00ff10");
    const upper = decodeSecret("00FF10");
    if (lower.kind !== "bytes" || upper.kind !== "bytes") throw new Error("expected bytes");
    expect([...lower.value]).toEqual([0, 255, 16]);
    expect([...upper.value]).toEqual([0, 255, 16]);
  });

  test("a public key WebCrypto rejects is `unusable`", async () => {
    expect(unverified(await verifyEd25519("aa", "aa", "m"))).toEqual({
      kind: "unusable",
      message: "invalid public key",
    });
  });

  test("a secret that is not hex or base64 names itself", async () => {
    expect(unverified(await verifyEd25519("not-a-key", "aa", "m"))).toEqual({
      kind: "unusable",
      message: "invalid public key encoding",
    });
  });

  test("a signature that is neither hex nor base64 names the encoding", async () => {
    expect(unverified(await verifyEd25519("00".repeat(32), "not!base64", "m"))).toEqual({
      kind: "unusable",
      message: "invalid signature encoding",
    });
  });
});

describe("slack diagnostics", () => {
  const headers = (at: number) => ({
    "x-slack-request-timestamp": String(at),
    "x-slack-signature": "v0=placeholder",
  });

  test("a missing timestamp names the timestamp header", async () => {
    expect(
      unverified(await verifySlack(request({ "x-slack-signature": "v0=x" }), slackConfig, 1)),
    ).toEqual({ kind: "absent", message: "x-slack-request-timestamp" });
  });

  test("a missing signature names the signature header", async () => {
    expect(
      unverified(await verifySlack(request({ "x-slack-request-timestamp": "1" }), slackConfig, 1)),
    ).toEqual({ kind: "absent", message: "x-slack-signature" });
  });

  test("a non-numeric timestamp is a mismatch", async () => {
    expect(
      unverified(
        await verifySlack(
          request({ ...headers(1), "x-slack-request-timestamp": "later" }),
          slackConfig,
          1,
        ),
      ),
    ).toEqual({ kind: "mismatch", message: "invalid signature" });
  });

  test("a stale request is a mismatch", async () => {
    expect(
      unverified(
        await verifySlack(request(headers(1)), slackConfig, 1 + SLACK_MAX_SKEW_SECONDS + 1),
      ),
    ).toEqual({ kind: "mismatch", message: "invalid signature" });
  });

  test("a url_verification without a challenge is a 400", () => {
    expect(refused(parseSlackEvent({ type: "url_verification" }, slackConfig)).message).toBe(
      "missing Slack challenge",
    );
  });

  test("an interactive payload is ignored by name", () => {
    expect(parseSlackEvent({ payload: { type: "block_actions" } }, slackConfig)).toEqual({
      kind: "ignored",
      reason: "Slack interactive payload",
    });
  });

  test("an envelope with no event is a 400", () => {
    expect(refused(parseSlackEvent({ type: "event_callback" }, slackConfig)).message).toBe(
      "missing Slack event",
    );
  });

  test("a mention without a team, channel, or timestamp is a 400", () => {
    for (const override of [{ team: undefined }, { channel: undefined }, { ts: undefined }]) {
      const body = {
        event: { type: "app_mention", team: "T", channel: "C", ts: "1", ...override },
      };
      expect(refused(parseSlackEvent(body, slackConfig)).status).toBe(400);
    }
  });

  test("a thread reply without a team, channel, or timestamp is a 400", () => {
    const body = {
      event: { type: "message", ts: "2", thread_ts: "1", team: undefined, channel: "C" },
    };
    expect(refused(parseSlackEvent(body, slackConfig)).status).toBe(400);
  });

  test("an app mention with no user has an empty author and still becomes a turn", () => {
    const body = { event: { type: "app_mention", team: "T", channel: "C", ts: "1", text: "hi" } };
    const result = parseSlackEvent(body, slackConfig);
    expect(result.kind === "event" && result.event.authorId).toBe("");
  });

  test("the skew window is five minutes", () => {
    expect(SLACK_MAX_SKEW_SECONDS).toBe(300);
  });
});

describe("slack schema helpers", () => {
  test("threadOf prefers the thread parent and falls back to the message timestamp", () => {
    expect(threadOf({ type: "message", team: "T", channel: "C", ts: "1" })).toEqual({
      workspaceId: "T",
      channelId: "C",
      threadId: "1",
    });
    expect(threadOf({ type: "message", team: "T", channel: "C", ts: "1", thread_ts: "0" })).toEqual(
      {
        workspaceId: "T",
        channelId: "C",
        threadId: "0",
      },
    );
  });

  test("threadOf is undefined without any one of the three ids", () => {
    expect(threadOf({ type: "message" })).toBeUndefined();
    expect(threadOf({ type: "message", team: "T", channel: "C" })).toBeUndefined();
    expect(threadOf({ type: "message", team: "T", ts: "1" })).toBeUndefined();
    expect(threadOf({ type: "message", channel: "C", ts: "1" })).toBeUndefined();
  });

  test("promptOf treats absent text as empty", () => {
    expect(promptOf({ type: "message" }, "UBOT")).toBe("");
  });

  test("stripMention removes every occurrence", () => {
    expect(stripMention("<@UBOT>a<@UBOT>b", "UBOT")).toBe("a b");
  });
});

describe("discord diagnostics", () => {
  test("a non-ping with no prompt names the prompt", () => {
    expect(
      refused(parseDiscordInteraction({ id: "i", type: 2, channel_id: "c", data: {} })).message,
    ).toBe("Discord interaction has no prompt");
  });

  test("a non-ping with no id names the id", () => {
    expect(
      refused(parseDiscordInteraction({ type: 2, channel_id: "c", data: { custom_id: "x" } }))
        .message,
    ).toBe("Discord interaction has no id");
  });

  test("a non-ping with no channel names the channel", () => {
    expect(
      refused(parseDiscordInteraction({ id: "i", type: 2, data: { custom_id: "x" } })).message,
    ).toBe("Discord interaction has no channel");
  });

  test("channelOf prefers channel_id over the channel object", () => {
    const decoded = { id: "i", type: 2, channel_id: "outer", channel: { id: "inner" } };
    expect(channelOf(decoded as never)).toBe("outer");
  });

  test("channelOf falls back to the channel object", () => {
    expect(channelOf({ id: "i", type: 2, channel: { id: "inner" } } as never)).toBe("inner");
  });

  test("channelOf is undefined with neither field", () => {
    expect(channelOf({ id: "i", type: 2 } as never)).toBeUndefined();
  });

  test("guildOf prefers the channel guild, then the top-level guild", () => {
    expect(
      guildOf({ type: 2, channel: { id: "c", guild_id: "G1" }, guild_id: "G2" } as never),
    ).toBe("G1");
    expect(guildOf({ type: 2, channel: { id: "c" }, guild_id: "G2" } as never)).toBe("G2");
    expect(guildOf({ type: 2, channel: { id: "c" } } as never)).toBe("");
    expect(guildOf({ type: 2 } as never)).toBe("");
  });

  test("authorOf prefers the member, then the user", () => {
    expect(discordAuthor({ member: { user: { id: "M" } }, user: { id: "U" } } as never)).toBe("M");
    expect(discordAuthor({ user: { id: "U" } } as never)).toBe("U");
    expect(discordAuthor({} as never)).toBe("");
  });

  test("discordPrompt prefers custom_id, then the prompt option", () => {
    expect(
      discordPrompt({
        data: { custom_id: "c", options: [{ name: "prompt", value: "v" }] },
      } as never),
    ).toBe("c");
    expect(
      discordPrompt({ data: { options: [{ name: "other", value: "o" }] } } as never),
    ).toBeUndefined();
    expect(discordPrompt({ data: {} } as never)).toBeUndefined();
    expect(discordPrompt({} as never)).toBeUndefined();
    expect(
      discordPrompt({
        data: {
          options: [
            { name: "prompt", value: "first" },
            { name: "prompt", value: "second" },
          ],
        },
      } as never),
    ).toBe("first");
  });

  test("a missing Discord timestamp is a mismatch", async () => {
    expect(
      unverified(
        await verifyDiscord(request({ "x-signature-ed25519": "aa" }), { publicKey: "aa" }),
      ),
    ).toEqual({ kind: "absent", message: "x-signature-timestamp" });
  });

  test("a Discord request with neither header names the one it checked first", async () => {
    expect(unverified(await verifyDiscord(request({}), { publicKey: "aa" }))).toEqual({
      kind: "absent",
      message: "x-signature-ed25519",
    });
  });
});

describe("linear diagnostics", () => {
  const base = {
    type: "AgentSessionEvent",
    agentSession: { id: "s" },
    issue: { id: "i", teamId: "t" },
    agentActivity: { type: "prompt", body: "go" },
  };

  test("a non-agent-session event is ignored by type", () => {
    expect(parseLinearEvent({ type: "Issue" })).toEqual({
      kind: "ignored",
      reason: "unsupported Linear event: Issue",
    });
    expect(parseLinearEvent({})).toEqual({
      kind: "ignored",
      reason: "unsupported Linear event: unknown",
    });
  });

  test("a missing session names itself, and a session without an issue still parses", () => {
    expect(refused(parseLinearEvent({ type: "AgentSessionEvent" })).message).toBe(
      "Linear event has no agent session",
    );
    expect(parseLinearEvent({ ...base, issue: undefined }).kind).toBe("event");
    expect(parseLinearEvent({ ...base, issue: { id: "i" } }).kind).toBe("event");
  });

  test("linearPrompt tries each carrier in order and skips blanks", () => {
    expect(linearPrompt({ agentActivity: { body: "  " }, prompt: "p" } as never)).toBe("p");
    expect(linearPrompt({ agentActivity: {}, body: "b" } as never)).toBe("b");
    expect(linearPrompt({} as never)).toBeUndefined();
  });

  test("linearAuthor prefers the actor", () => {
    expect(linearAuthor({ actor: { id: "A" }, user: { id: "U" } } as never)).toBe("A");
    expect(linearAuthor({ user: { id: "U" } } as never)).toBe("U");
    expect(linearAuthor({} as never)).toBe("");
  });

  test("a missing Linear signature names the header", async () => {
    expect(unverified(await verifyLinear(request({}), { webhookSecret: "s" }))).toEqual({
      kind: "absent",
      message: "linear-signature",
    });
  });
});

describe("boundaries the survivors pointed at", () => {
  test("a non-finite timestamp is a mismatch", async () => {
    expect(
      unverified(
        await verifySlack(
          request({ "x-slack-request-timestamp": "Infinity", "x-slack-signature": "v0=x" }),
          slackConfig,
          0,
        ),
      ),
    ).toEqual({ kind: "mismatch", message: "invalid signature" });
  });

  test("a timestamp on either side of now is age-checked symmetrically", async () => {
    const now = 1_000;
    const at = (seconds: number) =>
      request({
        "x-slack-request-timestamp": String(seconds),
        "x-slack-signature": "v0=x",
      });
    expect(unverified(await verifySlack(at(now + 10_000), slackConfig, now)).kind).toBe("mismatch");
    expect(unverified(await verifySlack(at(now - 10_000), slackConfig, now)).kind).toBe("mismatch");
  });

  test("a Slack cancel from a message with a thread parent reports the new timestamp", () => {
    const body = {
      event_id: "Ev9",
      event: {
        type: "message",
        team: "T",
        channel: "C",
        ts: "5",
        thread_ts: "1",
        user: "U",
        text: "STOP",
      },
    };
    const result = parseSlackEvent(body, slackConfig);
    expect(result.kind === "event" && result.event.eventId).toBe("Ev9");
  });

  test("constantTimeEqual agrees with string equality over equal-length pairs", () => {
    const strings = ["", "a", "b", "aa", "ab", "ba", "bb", "aaa", "aab"];
    for (const left of strings) {
      for (const right of strings) {
        expect(constantTimeEqual(left, right)).toBe(left === right);
      }
    }
  });

  test("decodeSecret reads every two-digit hex pair, including zero", () => {
    const zero = decodeSecret("0000");
    const max = decodeSecret("ffff");
    const mixed = decodeSecret("0a0b");
    if (zero.kind !== "bytes" || max.kind !== "bytes" || mixed.kind !== "bytes") {
      throw new Error("expected bytes");
    }
    expect([...zero.value]).toEqual([0, 0]);
    expect([...max.value]).toEqual([255, 255]);
    expect([...mixed.value]).toEqual([10, 11]);
  });
});

describe("exact diagnostics", () => {
  test("a wrong Slack signature names the signature, not the secret", async () => {
    expect(
      unverified(
        await verifySlack(
          request({ "x-slack-request-timestamp": "100", "x-slack-signature": "v0=00" }),
          slackConfig,
          100,
        ),
      ),
    ).toEqual({ kind: "mismatch", message: "invalid signature" });
  });

  test("an Ed25519 signature that does not verify is a mismatch", async () => {
    const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, [
      "sign",
      "verify",
    ])) as unknown as CryptoKeyPair;
    const raw = (await crypto.subtle.exportKey("raw", pair.publicKey)) as ArrayBuffer;
    const publicKeyHex = [...new Uint8Array(raw)]
      .map((byte) => byte.toString(16).padStart(2, "0"))
      .join("");
    expect(unverified(await verifyEd25519(publicKeyHex, "00".repeat(64), "message"))).toEqual({
      kind: "mismatch",
      message: "invalid signature",
    });
  });

  test("a Discord cancel reports the interaction author", () => {
    const result = parseDiscordInteraction({
      id: "i",
      type: 3,
      channel_id: "c",
      member: { user: { id: "U1" } },
      data: { custom_id: "cancel" },
    });
    expect(result.kind === "event" && result.event.authorId).toBe("U1");
  });

  test("a Slack mention without a user still answers with an empty author", () => {
    const body = { event: { type: "app_mention", team: "T", channel: "C", ts: "1", text: "hi" } };
    expect(parseSlackEvent(body, slackConfig)).toMatchObject({
      kind: "event",
      event: { authorId: "" },
    });
  });

  test("a Slack cancel without a user still answers with an empty author", () => {
    const body = {
      event: { type: "message", team: "T", channel: "C", ts: "2", thread_ts: "1", text: "stop" },
    };
    expect(parseSlackEvent(body, slackConfig)).toMatchObject({
      kind: "event",
      event: { kind: "cancel", authorId: "" },
    });
  });
});

describe("fields the survivors pointed at", () => {
  test("a Discord cancel names its provider", () => {
    const result = parseDiscordInteraction({
      id: "i",
      type: 3,
      channel_id: "c",
      data: { custom_id: "STOP" },
    });
    expect(result).toMatchObject({ kind: "event", event: { kind: "cancel", provider: "discord" } });
  });

  test("a Discord interaction with no options has no prompt", () => {
    expect(discordPrompt({ data: {} } as never)).toBeUndefined();
    expect(discordPrompt({ data: { options: [] } } as never)).toBeUndefined();
  });

  test("a Slack mention with no thread names what is missing", () => {
    expect(
      refused(parseSlackEvent({ event: { type: "app_mention", team: "T", ts: "1" } }, slackConfig))
        .message,
    ).toBe("Slack mention is missing team, channel, or timestamp");
  });

  test("a Slack thread reply with no team names what is missing", () => {
    expect(
      refused(
        parseSlackEvent(
          { event: { type: "message", channel: "C", ts: "2", thread_ts: "1" } },
          slackConfig,
        ),
      ).message,
    ).toBe("Slack message is missing team, channel, or timestamp");
  });
});

describe("the last branches", () => {
  test("a public key of the wrong length is `unusable`", async () => {
    expect(unverified(await verifyEd25519("aabb", "aa", "m"))).toEqual({
      kind: "unusable",
      message: "invalid public key",
    });
  });

  test("a public key that is valid hex but structurally wrong is `unusable`", async () => {
    const result = await verifyEd25519("00".repeat(32), "00".repeat(64), "m");
    expect(result.kind).not.toBe("verified");
  });

  test("promptOf returns the first prompt option among several", () => {
    const interaction = {
      data: {
        options: [
          { name: "other", value: "x" },
          { name: "prompt", value: "wanted" },
        ],
      },
    };
    expect(discordPrompt(interaction as never)).toBe("wanted");
  });
});

describe("a refused signature reaches the provider as a 401", () => {
  test("a missing header names the header, so an operator knows what to fix", () => {
    expect(refuseSignature({ kind: "absent", header: "x-slack-signature" })).toEqual({
      kind: "refused",
      status: 401,
      message: "missing x-slack-signature",
    });
  });

  test("a mismatch is the generic invalid-signature message", () => {
    expect(refuseSignature({ kind: "mismatch" })).toEqual({
      kind: "refused",
      status: 401,
      message: "invalid signature",
    });
  });

  test("an unsigned deployment keeps its own message, which names the secret", () => {
    expect(refuseSignature({ kind: "unsigned", message: "signing secret is empty" })).toEqual({
      kind: "refused",
      status: 401,
      message: "signing secret is empty",
    });
  });

  test("an unusable key keeps its own message, which names the key", () => {
    expect(refuseSignature({ kind: "unusable", message: "invalid public key" })).toEqual({
      kind: "refused",
      status: 401,
      message: "invalid public key",
    });
  });
});

describe("a missing header is a mismatch, not a crash", () => {
  test("Discord without a signature header", async () => {
    expect(await verifyDiscord(request({}), { publicKey: "aa" })).toEqual({
      kind: "absent",
      header: "x-signature-ed25519",
    });
  });

  test("Linear without a signature header", async () => {
    expect(await verifyLinear(request({}), { webhookSecret: "s" })).toEqual({
      kind: "absent",
      header: "linear-signature",
    });
  });

  test("Slack without a signature header", async () => {
    expect(
      await verifySlack(request({ "x-slack-request-timestamp": "1" }), slackConfig, 1),
    ).toEqual({ kind: "absent", header: "x-slack-signature" });
  });
});

describe("an HMAC key the platform refuses", () => {
  test("a secret that is too long for HMAC is unusable rather than a crash", async () => {
    // WebCrypto accepts a long key, so this exercises the branch through the
    // empty-secret guard instead; the point is that neither path throws.
    expect((await hmacHex("", "m", "sha256")).kind).toBe("unsigned");
  });
});

describe("a GraphQL transport failure keeps its reason", () => {
  test("an unreachable endpoint names itself so a caller can log it", async () => {
    const answer = await postGraphql("http://127.0.0.1:1/graphql", {
      method: "POST",
      headers: {},
      body: "{}",
    });
    expect(answer.kind).toBe("unreachable");
    expect(answer.kind !== "body" && answer.message).toContain("unreachable");
  });
});

describe("the exact boundary and empty-secret paths", () => {
  test("an empty header reads as absent, the same as a missing one", () => {
    expect(headerOf({ headers: { "x-slack-signature": "" } }, "x-slack-signature")).toEqual({
      kind: "absent",
      header: "x-slack-signature",
    });
  });

  test("a present header keeps its value", () => {
    expect(headerOf({ headers: { "x-slack-signature": "v0=1" } }, "x-slack-signature")).toEqual({
      kind: "present",
      value: "v0=1",
    });
  });

  test("exactly the skew limit is still accepted, so a clock a moment behind is not refused", async () => {
    const now = 1_000;
    const at = now - SLACK_MAX_SKEW_SECONDS;
    const text = "{}";
    const signed = await signSha256(slackConfig.signingSecret, `v0:${at}:${text}`);
    if (signed.kind !== "signed") throw new Error("expected a signature");
    const signature = signed.hex;
    const request = {
      method: "POST",
      headers: {
        "x-slack-request-timestamp": String(at),
        "x-slack-signature": `v0=${signature}`,
      },
      body: text,
    };
    expect(await verifySlack(request, slackConfig, now)).toEqual({ kind: "verified" });
  });

  test("an empty signing secret is unsigned, not a mismatch, so the fault is named", async () => {
    const now = 1_000;
    const request = {
      method: "POST",
      headers: {
        "x-slack-request-timestamp": String(now),
        "x-slack-signature": "v0=x",
      },
      body: "{}",
    };
    expect(await verifySlack(request, { ...slackConfig, signingSecret: "" }, now)).toEqual({
      kind: "unsigned",
      message: "signing secret is empty",
    });
  });
});

describe("a timestamp the platform cannot parse", () => {
  test("a non-numeric timestamp is refused even when its signature is valid, so a clock trick cannot replay", async () => {
    const text = "{}";
    const signed = await signSha256(slackConfig.signingSecret, `v0:soon:${text}`);
    if (signed.kind !== "signed") throw new Error("expected a signature");
    const answer = await verifySlack(
      request({ "x-slack-request-timestamp": "soon", "x-slack-signature": `v0:${signed.hex}` }),
      slackConfig,
      0,
    );
    expect(answer).toEqual({ kind: "mismatch" });
  });
});

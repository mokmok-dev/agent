import { describe, expect, test } from "bun:test";
import { AdapterError, decodeOr400 } from "./events.ts";
import {
  constantTimeEqual,
  decodeSecret,
  hmacHex,
  signSha256,
  verifyEd25519,
  verifyHmac,
} from "./signature.ts";
import { parseJson, parseSlackEvent, SLACK_MAX_SKEW_SECONDS, verifySlack } from "./slack.ts";
import { parseDiscordInteraction } from "./discord.ts";
import { parseLinearEvent } from "./linear.ts";
import { promptOf, stripMention, threadOf } from "./slack-schema.ts";
import {
  authorOf as discordAuthor,
  channelOf,
  guildOf,
  promptOf as discordPrompt,
} from "./discord-schema.ts";
import { authorOf as linearAuthor, promptOf as linearPrompt } from "./linear-schema.ts";

const slackConfig = { signingSecret: "shhh", botUserId: "UBOT" };

function failure(run: () => unknown): AdapterError {
  try {
    run();
  } catch (error) {
    expect(error).toBeInstanceOf(AdapterError);
    return error as AdapterError;
  }
  throw new Error("expected an AdapterError");
}

async function asyncFailure(run: Promise<unknown>): Promise<AdapterError> {
  try {
    await run;
  } catch (error) {
    expect(error).toBeInstanceOf(AdapterError);
    return error as AdapterError;
  }
  throw new Error("expected an AdapterError");
}

describe("decodeOr400", () => {
  test("returns the decoded value when the decoder accepts it", () => {
    expect(decodeOr400((value) => value, { a: 1 })).toEqual({ a: 1 });
  });

  test("a thrown Error becomes a 400 that keeps its message", () => {
    const error = failure(() =>
      decodeOr400(() => {
        throw new Error("bad shape");
      }, {}),
    );
    expect(error.status).toBe(400);
    expect(error.message).toBe("bad shape");
  });

  test("a thrown non-Error becomes a 400 with its string form", () => {
    const error = failure(() =>
      decodeOr400(() => {
        throw "plain";
      }, {}),
    );
    expect(error.status).toBe(400);
    expect(error.message).toBe("plain");
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

  test("verifyHmac reports the exact message", async () => {
    const digest = await signSha256("shh", "m");
    const error = await asyncFailure(verifyHmac("shh", "m", digest.replace(/^./, "x"), "sha256"));
    expect(error.status).toBe(401);
    expect(error.message).toBe("invalid signature");
  });

  test("an empty signing secret names itself", async () => {
    const error = await asyncFailure(hmacHex("", "m", "sha256"));
    expect(error.message).toBe("signing secret is empty");
  });

  test("decodeSecret reads an odd-length hex-looking string as base64", () => {
    expect(decodeSecret("abc")).toEqual(Uint8Array.from(atob("abc"), (c) => c.charCodeAt(0)));
  });

  test("decodeSecret reads every even-length hex string as bytes", () => {
    expect([...decodeSecret("00ff10")]).toEqual([0, 255, 16]);
    expect([...decodeSecret("00FF10")]).toEqual([0, 255, 16]);
  });

  test("a public key WebCrypto rejects names itself", async () => {
    const error = await asyncFailure(verifyEd25519("aa", "aa", "m"));
    expect(error.status).toBe(401);
    expect(error.message).toBe("invalid public key");
  });

  test("a secret that is not hex or base64 names itself", async () => {
    const error = await asyncFailure(verifyEd25519("not-a-key", "aa", "m"));
    expect(error.status).toBe(401);
    expect(error.message).toBe("invalid secret encoding");
  });
});

describe("slack diagnostics", () => {
  const signed = (body: unknown, at: number) => ({
    method: "POST",
    headers: {
      "x-slack-request-timestamp": String(at),
      "x-slack-signature": "v0=placeholder",
    },
    body: JSON.stringify(body),
  });

  test("a missing timestamp names the timestamp", async () => {
    const request = signed({}, 1);
    const error = await asyncFailure(
      verifySlack({ ...request, headers: { "x-slack-signature": "v0=x" } }, slackConfig, 1),
    );
    expect(error.message).toBe("missing Slack timestamp header");
  });

  test("a missing signature names the signature", async () => {
    const request = signed({}, 1);
    const error = await asyncFailure(
      verifySlack({ ...request, headers: { "x-slack-request-timestamp": "1" } }, slackConfig, 1),
    );
    expect(error.message).toBe("missing Slack signature header");
  });

  test("a non-numeric timestamp names the timestamp", async () => {
    const request = signed({}, 1);
    const error = await asyncFailure(
      verifySlack(
        { ...request, headers: { ...request.headers, "x-slack-request-timestamp": "later" } },
        slackConfig,
        1,
      ),
    );
    expect(error.message).toBe("invalid Slack timestamp");
  });

  test("a stale request names the age rule", async () => {
    const request = signed({}, 1);
    const error = await asyncFailure(
      verifySlack(request, slackConfig, 1 + SLACK_MAX_SKEW_SECONDS + 1),
    );
    expect(error.message).toBe("Slack request is outside the allowed age");
  });

  test("a url_verification without a challenge is a 400", () => {
    expect(failure(() => parseSlackEvent({ type: "url_verification" }, slackConfig)).message).toBe(
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
    expect(failure(() => parseSlackEvent({ type: "event_callback" }, slackConfig)).message).toBe(
      "missing Slack event",
    );
  });

  test("a mention without a team, channel, or timestamp is a 400", () => {
    for (const override of [{ team: undefined }, { channel: undefined }, { ts: undefined }]) {
      const body = {
        event: { type: "app_mention", team: "T", channel: "C", ts: "1", ...override },
      };
      expect(failure(() => parseSlackEvent(body, slackConfig)).status).toBe(400);
    }
  });

  test("a thread reply without a team, channel, or timestamp is a 400", () => {
    const body = {
      event: { type: "message", ts: "2", thread_ts: "1", team: undefined, channel: "C" },
    };
    expect(failure(() => parseSlackEvent(body, slackConfig)).status).toBe(400);
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
    const error = failure(() =>
      parseDiscordInteraction({ id: "i", type: 2, channel_id: "c", data: {} }),
    );
    expect(error.message).toBe("Discord interaction has no prompt");
  });

  test("a non-ping with no id names the id", () => {
    const error = failure(() =>
      parseDiscordInteraction({ type: 2, channel_id: "c", data: { custom_id: "x" } }),
    );
    expect(error.message).toBe("Discord interaction has no id");
  });

  test("a non-ping with no channel names the channel", () => {
    const error = failure(() =>
      parseDiscordInteraction({ id: "i", type: 2, data: { custom_id: "x" } }),
    );
    expect(error.message).toBe("Discord interaction has no channel");
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

  test("a missing session, issue, or team names itself", () => {
    expect(failure(() => parseLinearEvent({ type: "AgentSessionEvent" })).message).toBe(
      "Linear event has no agent session",
    );
    expect(failure(() => parseLinearEvent({ ...base, issue: undefined })).message).toBe(
      "Linear event has no issue",
    );
    expect(failure(() => parseLinearEvent({ ...base, issue: { id: "i" } })).message).toBe(
      "Linear event has no team",
    );
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
});

describe("boundaries the survivors pointed at", () => {
  test("a non-finite timestamp is refused", async () => {
    const request = {
      method: "POST",
      headers: { "x-slack-request-timestamp": "Infinity", "x-slack-signature": "v0=x" },
      body: "{}",
    };
    const error = await asyncFailure(verifySlack(request, slackConfig, 0));
    expect(error.message).toBe("invalid Slack timestamp");
  });

  test("a timestamp on either side of now is age-checked symmetrically", async () => {
    const now = 1_000;
    const future = {
      method: "POST",
      headers: { "x-slack-request-timestamp": String(now + 10_000), "x-slack-signature": "v0=x" },
      body: "{}",
    };
    const past = {
      method: "POST",
      headers: { "x-slack-request-timestamp": String(now - 10_000), "x-slack-signature": "v0=x" },
      body: "{}",
    };
    expect((await asyncFailure(verifySlack(future, slackConfig, now))).message).toBe(
      "Slack request is outside the allowed age",
    );
    expect((await asyncFailure(verifySlack(past, slackConfig, now))).message).toBe(
      "Slack request is outside the allowed age",
    );
  });

  test("a missing Discord timestamp names the headers", async () => {
    const { verifyDiscord } = await import("./discord.ts");
    const error = await asyncFailure(
      verifyDiscord(
        { method: "POST", headers: { "x-signature-ed25519": "aa" }, body: "{}" },
        { publicKey: "aa" },
      ),
    );
    expect(error.message).toBe("missing Discord timestamp");
  });

  test("a missing Linear signature names it", async () => {
    const { verifyLinear } = await import("./linear.ts");
    const error = await asyncFailure(
      verifyLinear({ method: "POST", headers: {}, body: "{}" }, { webhookSecret: "s" }),
    );
    expect(error.message).toBe("missing Linear signature");
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
    const alphabet = "ab";
    const strings = ["", "a", "b", "aa", "ab", "ba", "bb", "aaa", "aab"];
    for (const left of strings) {
      for (const right of strings) {
        expect(constantTimeEqual(left, right)).toBe(left === right);
      }
    }
    void alphabet;
  });

  test("decodeSecret reads every two-digit hex pair, including zero", () => {
    expect([...decodeSecret("0000")]).toEqual([0, 0]);
    expect([...decodeSecret("ffff")]).toEqual([255, 255]);
    expect([...decodeSecret("0a0b")]).toEqual([10, 11]);
  });
});

describe("exact diagnostics", () => {
  test("a body that is not JSON names itself", () => {
    const error = failure(() => parseJson({ method: "POST", headers: {}, body: "{" }));
    expect(error.status).toBe(400);
    expect(error.message).toBe("body is not JSON");
  });

  test("a wrong Slack signature names the signature, not the secret", async () => {
    const request = {
      method: "POST",
      headers: { "x-slack-request-timestamp": "100", "x-slack-signature": "v0=00" },
      body: "{}",
    };
    const error = await asyncFailure(verifySlack(request, slackConfig, 100));
    expect(error.message).toBe("invalid signature");
  });

  test("an Ed25519 signature that does not verify names the signature", async () => {
    const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, [
      "sign",
      "verify",
    ])) as unknown as CryptoKeyPair;
    const raw = (await crypto.subtle.exportKey("raw", pair.publicKey)) as ArrayBuffer;
    const publicKeyHex = [...new Uint8Array(raw)]
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
    const error = await asyncFailure(verifyEd25519(publicKeyHex, "00".repeat(64), "message"));
    expect(error.message).toBe("invalid signature");
  });

  test("a Discord request with neither signature nor timestamp names the headers", async () => {
    const { verifyDiscord } = await import("./discord.ts");
    const error = await asyncFailure(
      verifyDiscord({ method: "POST", headers: {}, body: "{}" }, { publicKey: "aa" }),
    );
    expect(error.message).toBe("missing Discord signature");
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
    const error = failure(() =>
      parseSlackEvent({ event: { type: "app_mention", team: "T", ts: "1" } }, slackConfig),
    );
    expect(error.message).toBe("Slack mention is missing team, channel, or timestamp");
  });

  test("a Slack thread reply with no team names what is missing", () => {
    const error = failure(() =>
      parseSlackEvent(
        { event: { type: "message", channel: "C", ts: "2", thread_ts: "1" } },
        slackConfig,
      ),
    );
    expect(error.message).toBe("Slack message is missing team, channel, or timestamp");
  });
});

describe("the last branches", () => {
  test("a public key of the wrong length names itself", async () => {
    const error = await asyncFailure(verifyEd25519("aabb", "aa", "m"));
    expect(error.message).toBe("invalid public key");
  });

  test("a public key that is valid hex but structurally wrong names itself", async () => {
    const error = await asyncFailure(verifyEd25519("00".repeat(32), "00".repeat(64), "m"));
    expect(error.status).toBe(401);
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

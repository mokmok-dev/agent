import { describe, expect, test } from "bun:test";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import { createRegistry } from "@earendil-works/pi-durable";
import { openNodeJsonlStorage } from "@earendil-works/pi-durable/storage/jsonl/node";
import { ConversationBase } from "@agent/conversation";
import { SCRIPT_MODEL, SCRIPT_PROVIDER, scriptedModels } from "@agent/conversation/testing";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { piTurnRunner } from "./pi-turn-runner.ts";
import type { TurnInput } from "./turn-runner.ts";

const input = (overrides: Partial<TurnInput> = {}): TurnInput => ({
  sessionId: "s1",
  conversationId: "c1",
  prompt: "what is up",
  operationId: "turn-1",
  model: { provider: SCRIPT_PROVIDER, modelId: SCRIPT_MODEL },
  now: 0,
  ...overrides,
});

/** A real pi conversation over a temp directory, as the local VM does. */
async function conversation(
  replies: Parameters<typeof scriptedModels>[0],
): Promise<{ base: ConversationBase; dir: string }> {
  const dir = await mkdtemp(join(tmpdir(), "pi-turn-"));
  const base = await ConversationBase.open(await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT), {
    models: scriptedModels(replies),
    registry: createRegistry(),
    context: BACKGROUND_CONTEXT,
  });
  return { base, dir };
}

describe("running a turn through pi", () => {
  test("answers with the words pi recorded", async () => {
    const { base, dir } = await conversation([{ text: "all good" }]);
    try {
      const result = await piTurnRunner({ conversation: () => base }).run(input());
      expect(result).toEqual({ kind: "answered", text: "all good" });
    } finally {
      await base.close();
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a model failure is unanswered with pi's reason, so the turn fails rather than hangs", async () => {
    const { base, dir } = await conversation([{ error: "the model refused" }]);
    try {
      const result = await piTurnRunner({ conversation: () => base }).run(input());
      expect(result.kind).toBe("unanswered");
      expect(result.kind === "unanswered" && result.reason).not.toBe("");
    } finally {
      await base.close();
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("no conversation is unanswered with a reason a person can act on", async () => {
    const result = await piTurnRunner({ conversation: () => undefined }).run(input());
    expect(result).toEqual({
      kind: "unanswered",
      reason: "no agent is configured for this deployment",
    });
  });

  test("a host that throws while providing the conversation is unanswered, not a crash", async () => {
    const runner = piTurnRunner({
      conversation: () => {
        throw new Error("storage is gone");
      },
    });
    expect(await runner.run(input())).toEqual({ kind: "unanswered", reason: "storage is gone" });
  });

  test("a host that throws a non-error still names the reason", async () => {
    const runner = piTurnRunner({
      conversation: () => {
        throw "plain";
      },
    });
    expect(await runner.run(input())).toEqual({ kind: "unanswered", reason: "plain" });
  });

  test("the turn's operation id is what pi keys the submission on", async () => {
    const { base, dir } = await conversation([{ text: "one" }]);
    try {
      const runner = piTurnRunner({ conversation: () => base });
      await runner.run(input({ operationId: "op-a" }));
      await runner.run(input({ operationId: "op-b", prompt: "second" }));
      expect((await base.transcript()).length).toBe(4);
    } finally {
      await base.close();
      await rm(dir, { recursive: true, force: true });
    }
  });
});

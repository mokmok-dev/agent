import { describe, expect, test } from "bun:test";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import { createRegistry } from "@earendil-works/pi-durable";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openNodeJsonlStorage } from "@earendil-works/pi-durable/storage/jsonl/node";
import { assistantText, ConversationBase, type ConversationDeps } from "./conversation.ts";
import { SCRIPT_MODEL, SCRIPT_PROVIDER, scriptedModels } from "./testing/scripted.ts";

/** Opens a conversation over a fresh JSONL directory, as the local VM does. */
async function withConversation(
  replies: Parameters<typeof scriptedModels>[0],
  run: (conversation: ConversationBase, dir: string) => Promise<void>,
): Promise<void> {
  const dir = await mkdtemp(join(tmpdir(), "conversation-"));
  try {
    const conversation = await ConversationBase.open(
      await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT),
      deps(replies),
    );
    await conversation.setModel({ provider: SCRIPT_PROVIDER, modelId: SCRIPT_MODEL });
    await run(conversation, dir);
    await conversation.close();
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

function deps(replies: Parameters<typeof scriptedModels>[0]): ConversationDeps {
  return {
    models: scriptedModels(replies),
    registry: createRegistry(),
    context: BACKGROUND_CONTEXT,
    now: () => 1_700_000_000_000,
    onWake: () => undefined,
  };
}

describe("a conversation over pi-durable", () => {
  test("a prompt runs a real turn and the transcript holds both messages", async () => {
    await withConversation([{ text: "hello from the script" }], async (conversation) => {
      const result = await conversation.submit("say hi", "op-1");
      expect(result).toEqual({ kind: "done", operationId: "op-1", reason: undefined });
      const transcript = await conversation.transcript();
      expect(transcript.map((entry) => entry.kind)).toEqual(["pi.user", "pi.assistant"]);
    });
  });

  test("the last reply is the assistant's words", async () => {
    await withConversation([{ text: "the answer" }], async (conversation) => {
      await conversation.submit("question", "op-1");
      expect(await conversation.lastReply()).toBe("the answer");
    });
  });

  test("a model failure settles unanswered with pi's reason", async () => {
    await withConversation([{ error: "the model refused" }], async (conversation) => {
      const result = await conversation.submit("question", "op-1");
      expect(result.kind).toBe("unanswered");
      expect(result.kind === "unanswered" && result.reason).not.toBe("");
    });
  });

  test("a failed turn leaves no assistant entry, so the reply is empty", async () => {
    await withConversation([{ error: "the model refused" }], async (conversation) => {
      await conversation.submit("question", "op-1");
      expect(await conversation.lastReply()).toBe("");
    });
  });

  test("the reply skips a failed turn that left no text", async () => {
    await withConversation([{ text: "answer" }, { error: "failed" }], async (conversation) => {
      await conversation.submit("first", "op-1");
      await conversation.submit("second", "op-2");
      const kinds = (await conversation.transcript()).map((entry) => entry.kind);
      expect(kinds.at(-1)).toBe("pi.assistant");
      expect(await conversation.lastReply()).toBe("answer");
    });
  });

  test("the script repeats its last reply once it runs out", async () => {
    await withConversation([{ text: "only" }], async (conversation) => {
      await conversation.submit("first", "op-1");
      await conversation.submit("second", "op-2");
      expect(await conversation.lastReply()).toBe("only");
    });
  });

  test("the reply comes from the assistant entry, never from the person's words", async () => {
    await withConversation([{ text: "the agent" }], async (conversation) => {
      await conversation.submit("the person", "op-1");
      expect(await conversation.lastReply()).not.toBe("the person");
      expect(await conversation.lastReply()).toBe("the agent");
    });
  });

  test("onWake reports work, then rest after the run settles", async () => {
    const wakes: boolean[] = [];
    const dir = await mkdtemp(join(tmpdir(), "conversation-"));
    try {
      const conversation = await ConversationBase.open(
        await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT),
        {
          models: scriptedModels([{ text: "x" }]),
          registry: createRegistry(),
          context: BACKGROUND_CONTEXT,
          onWake: (wake) => wakes.push(wake.active),
        },
      );
      await conversation.setModel({ provider: SCRIPT_PROVIDER, modelId: SCRIPT_MODEL });
      await conversation.submit("question", "op-1");
      expect(wakes).toContain(true);
      // Pi offers the wake after the settling commit, so a host that waits on
      // the turn must let that report land before it drops its alarm.
      await new Promise((resolve) => setTimeout(resolve, 10));
      expect(wakes.at(-1)).toBe(false);
      await conversation.close();
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("closing releases the harness, so a later read reports the closed session", async () => {
    const dir = await mkdtemp(join(tmpdir(), "conversation-"));
    try {
      const conversation = await ConversationBase.open(
        await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT),
        deps([{ text: "x" }]),
      );
      await conversation.close();
      await expect(conversation.transcript()).rejects.toThrow("Session is closed");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a prompt is submitted as an input, not a passive write", async () => {
    await withConversation([{ text: "x" }], async (conversation) => {
      const result = await conversation.submit("question", "op-1");
      expect(result.operationId).toBe("op-1");
      const kinds = (await conversation.transcript()).map((entry) => entry.kind);
      expect(kinds).toEqual(["pi.user", "pi.assistant"]);
    });
  });

  test("a turn with no text reads as an empty reply", async () => {
    await withConversation([{ text: "" }], async (conversation) => {
      await conversation.submit("question", "op-1");
      expect(await conversation.lastReply()).toBe("");
    });
  });

  test("a second prompt adds to the same transcript", async () => {
    await withConversation([{ text: "one" }, { text: "two" }], async (conversation) => {
      await conversation.submit("first", "op-1");
      await conversation.submit("second", "op-2");
      const kinds = (await conversation.transcript()).map((entry) => entry.kind);
      expect(kinds).toEqual(["pi.user", "pi.assistant", "pi.user", "pi.assistant"]);
      expect(await conversation.lastReply()).toBe("two");
    });
  });

  test("the conversation id is pi's root id", async () => {
    await withConversation([{ text: "x" }], async (conversation) => {
      expect(conversation.id).toBe("1");
    });
  });

  test("the base exposes the harness, so a host can reach a pi feature this module does not wrap", async () => {
    await withConversation([{ text: "x" }], async (conversation) => {
      expect(typeof conversation.harness.root).toBe("function");
      expect(conversation.harness.wake()).toHaveProperty("active");
    });
  });

  test("resume after a wakeup does not disturb a settled conversation", async () => {
    await withConversation([{ text: "x" }], async (conversation) => {
      await conversation.submit("question", "op-1");
      conversation.resume();
      expect(await conversation.lastReply()).toBe("x");
    });
  });

  test("a reopen resumes queued work rather than replaying it", async () => {
    const dir = await mkdtemp(join(tmpdir(), "conversation-"));
    try {
      const first = await ConversationBase.open(
        await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT),
        deps([{ text: "settled" }]),
      );
      await first.setModel({ provider: SCRIPT_PROVIDER, modelId: SCRIPT_MODEL });
      await first.submit("question", "op-1");
      await first.close();

      const second = await ConversationBase.open(
        await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT),
        deps([{ text: "settled" }]),
      );
      second.resume();
      expect(second.wake().active).toBe(false);
      expect((await second.transcript()).length).toBe(2);
      await second.close();
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("the conversation survives a reopen", () => {
  test("a new base over the same storage sees the history a turn wrote", async () => {
    const dir = await mkdtemp(join(tmpdir(), "conversation-"));
    try {
      const first = await ConversationBase.open(
        await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT),
        deps([{ text: "before the restart" }]),
      );
      await first.setModel({ provider: SCRIPT_PROVIDER, modelId: SCRIPT_MODEL });
      await first.submit("question", "op-1");
      await first.close();

      const second = await ConversationBase.open(
        await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT),
        deps([{ text: "after the restart" }]),
      );
      expect((await second.transcript()).length).toBe(2);
      expect(await second.lastReply()).toBe("before the restart");
      await second.close();
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("the transcript reader", () => {
  test("joins every text block of an assistant entry", () => {
    const entry = {
      id: "1",
      conversationId: "1",
      kind: "pi.assistant",
      model: [
        {
          role: "assistant",
          content: [
            { type: "text", text: "one " },
            { type: "text", text: "two" },
          ],
        },
      ],
    } as never;
    expect(assistantText(entry)).toBe("one two");
  });

  test("ignores a non-text block and a non-assistant message", () => {
    const entry = {
      id: "1",
      conversationId: "1",
      kind: "pi.assistant",
      model: [
        { role: "user", content: [{ type: "text", text: "ignored" }] },
        {
          role: "assistant",
          content: [
            { type: "thinking", thinking: "hidden" },
            { type: "text", text: "kept" },
          ],
        },
      ],
    } as never;
    expect(assistantText(entry)).toBe("kept");
  });

  test("an entry with no model messages reads as empty", () => {
    expect(assistantText({ id: "1", conversationId: "1", kind: "pi.assistant" } as never)).toBe("");
  });

  test("a missing entry reads as empty, so a fresh conversation has no reply", () => {
    expect(assistantText(undefined)).toBe("");
  });

  test("a message with only non-text blocks reads as empty", () => {
    const entry = {
      id: "1",
      conversationId: "1",
      kind: "pi.assistant",
      model: [{ role: "assistant", content: [{ type: "thinking", thinking: "hidden" }] }],
    } as never;
    expect(assistantText(entry)).toBe("");
  });
});

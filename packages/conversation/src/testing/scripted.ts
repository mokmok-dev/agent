import { createModels, createProvider, type Model, type Models } from "@earendil-works/pi-ai";
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai/utils/event-stream";

/**
 * A Pi AI provider that answers from a script instead of the network, so a
 * real pi-durable turn runs in a test. It is a provider in every other way, so
 * the harness resolves auth, model, and stream exactly as it does in production.
 */

export type ScriptedReply = { text: string } | { error: string };

/** A provider whose nth call answers with the nth reply; past the end it repeats the last. */
export function scriptedModels(replies: readonly ScriptedReply[]): Models {
  const models = createModels();
  let call = 0;
  const next = (): ScriptedReply => {
    const reply = replies[Math.min(call, replies.length - 1)] ?? { text: "" };
    call += 1;
    return reply;
  };
  models.setProvider(
    createProvider({
      id: SCRIPT_PROVIDER,
      name: "Scripted",
      auth: {
        apiKey: {
          name: "Scripted key",
          resolve: async () => ({ auth: { apiKey: "script" }, source: "script" }),
        },
      },
      models: [model()],
      api: { stream: () => answer(next()), streamSimple: () => answer(next()) },
    }),
  );
  return models;
}

export const SCRIPT_PROVIDER = "script";
export const SCRIPT_MODEL = "script-1";

function model(): Model<"openai-completions"> {
  return {
    id: SCRIPT_MODEL,
    name: "Scripted",
    api: "openai-completions",
    provider: SCRIPT_PROVIDER,
    baseUrl: "http://script.invalid",
    reasoning: false,
    input: ["text"],
    contextWindow: 100_000,
    maxTokens: 1_000,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  };
}

/** The whole answer as one message, so a partial never carries a half-built block. */
function answer(reply: ScriptedReply) {
  const stream = createAssistantMessageEventStream();
  const message = complete(reply);
  if ("text" in reply) {
    stream.push({ type: "start", partial: empty() });
    stream.push({ type: "text_start", contentIndex: 0, partial: empty() });
    stream.push({ type: "text_delta", contentIndex: 0, delta: reply.text, partial: message });
    stream.push({ type: "text_end", contentIndex: 0, content: reply.text, partial: message });
    stream.push({ type: "done", reason: "stop", message });
  } else {
    stream.push({ type: "error", reason: "error", error: message });
  }
  return stream;
}

function shell() {
  return {
    role: "assistant" as const,
    api: "openai-completions" as const,
    provider: SCRIPT_PROVIDER,
    model: SCRIPT_MODEL,
    usage: {
      input: 1,
      output: 1,
      cacheRead: 0,
      cacheWrite: 0,
      totalTokens: 2,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
    },
    timestamp: Date.now(),
  };
}

function empty() {
  return { ...shell(), content: [], stopReason: "stop" as const };
}

function complete(reply: ScriptedReply) {
  return "text" in reply
    ? {
        ...shell(),
        content: [{ type: "text" as const, text: reply.text }],
        stopReason: "stop" as const,
      }
    : {
        ...shell(),
        content: [],
        stopReason: "error" as const,
        errorMessage: reply.error,
      };
}

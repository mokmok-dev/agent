import {
  Harness,
  type Conversation,
  type EntryRecord,
  type Harness as DurableHarness,
  type HarnessOptions,
  type Registry,
  type Storage,
  type Wake,
} from "@earendil-works/pi-durable";
import type { Context } from "@earendil-works/chord/context";
import type { Models } from "@earendil-works/pi-ai";

/**
 * Pi Durable owns a conversation: its transcript, its inbox of follow-ups and
 * steers, its retries, and its crash recovery. This module is the one place the
 * rest of the system touches that, so a turn has one owner and one history.
 */

/** A model reference, in the two-field shape the catalog and the router use. */
export type ModelRef = { provider: string; modelId: string };

/** What opening a conversation needs beyond its storage. */
export type ConversationDeps = {
  /** Pi AI's model collection, which resolves the model a selection names. */
  models: Models | (() => Models);
  /** Pi's tool and prompt-section registry. */
  registry: Registry;
  /** The context every call runs under. */
  context: Context;
  /** The clock pi reads deadlines against. Defaults to `Date.now`. */
  now?: () => number;
  /** Called when pi next needs a process, so a host can arm its own wakeup. */
  onWake?: (wake: Wake) => void;
};

export type SubmitResult =
  | { kind: "done"; operationId: string; reason: undefined }
  | { kind: "unanswered"; operationId: string; reason: string };

/** The most transcript entries one read returns. */
const PAGE_SIZE = 100;

/**
 * One conversation over one pi Harness. `submit` is idempotent on the
 * operation id, so a redelivered prompt is one turn rather than two.
 */
export class ConversationBase {
  readonly #harness: DurableHarness;
  readonly #root: Conversation;
  readonly #context: Context;

  private constructor(harness: DurableHarness, root: Conversation, context: Context) {
    this.#harness = harness;
    this.#root = root;
    this.#context = context;
  }

  /**
   * Open the Harness over storage and return the root conversation. Opening
   * recovers whatever a previous process committed, so this is also resume.
   */
  static async open(storage: Storage, deps: ConversationDeps): Promise<ConversationBase> {
    const harness = await Harness.open(storage, optionsOf(deps), deps.context);
    const root = await harness.root(deps.context);
    return new ConversationBase(harness, root, deps.context);
  }

  /** The pi conversation id, which is what a session record stores. */
  get id(): string {
    return String(this.#root.id);
  }

  get harness(): DurableHarness {
    return this.#harness;
  }

  /** Pin the conversation's model, by the provider and id the router chose. */
  async setModel(model: ModelRef): Promise<void> {
    await this.#root.configure({ model }, this.#context);
  }

  /**
   * Submit one prompt and wait for it to settle. The operation id is the
   * idempotency key: submitting the same id twice returns the same turn.
   */
  async submit(prompt: string, operationId: string): Promise<SubmitResult> {
    const submission = await this.#root.submit({ type: "input", content: prompt }, this.#context);
    const settled = await submission.wait(this.#context);
    return settled.status === "done"
      ? { kind: "done", operationId, reason: undefined }
      : { kind: "unanswered", operationId, reason: settled.reason };
  }

  /** Start the conversation's queued work, after a host's wakeup fired. */
  resume(): void {
    this.#harness.resume();
  }

  /** When pi next needs a process. */
  wake(): Wake {
    return this.#harness.wake();
  }

  /** The active transcript, oldest first. */
  async transcript(): Promise<EntryRecord[]> {
    const page = await this.#root.entries(
      { order: "ascending" },
      PAGE_SIZE,
      undefined,
      this.#context,
    );
    return [...page.items];
  }

  /**
   * The assistant text of the newest turn, which is what a thread replies with.
   * An empty string when the agent said nothing.
   */
  /**
   * The assistant text of the newest turn that said something, which is what a
   * thread replies with. A failed or silent turn has no text, so the reply is
   * the last real answer; an empty string when there never was one.
   */
  async lastReply(): Promise<string> {
    const newestFirst = [...(await this.transcript())].reverse();
    for (const entry of newestFirst) {
      const text = assistantText(entry);
      if (text !== "") return text;
    }
    return "";
  }

  close(): Promise<void> {
    return this.#harness.close(this.#context);
  }
}

/**
 * The text an assistant entry contributes to a reply. Pi stores the model
 * message, so the text is in its assistant content blocks. Every other entry,
 * such as the person's message, a failed turn, or a note, reads as empty.
 */
export function assistantText(entry: EntryRecord | undefined): string {
  if (entry?.kind !== "pi.assistant") return "";
  const texts = (entry.model ?? [])
    .filter((message) => message.role === "assistant")
    .flatMap((message) => message.content)
    .filter((block) => block.type === "text")
    .map((block) => block.text);
  return texts.join("");
}

function optionsOf(deps: ConversationDeps): HarnessOptions {
  return {
    models: deps.models,
    registry: deps.registry,
    now: deps.now,
    onWake: deps.onWake,
  } as HarnessOptions;
}

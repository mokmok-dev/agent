import type { ConversationBase } from "@agent/conversation";
import type { TurnRunner, TurnInput, TurnResult } from "./turn-runner.ts";

/**
 * A turn runner that drives pi, so a delivery runs the real agent loop and the
 * answer comes from the conversation pi keeps.
 *
 * The host supplies the conversation: the Durable Object opens pi over its own
 * SQLite, and a test opens it over a temp directory. This module is the one
 * place the two shapes meet.
 */

/** How a host supplies the conversation a turn runs in. */
export type PiTurnDeps = {
  /**
   * The conversation for a turn, or `undefined` when this deployment cannot run
   * the agent. `undefined` fails the turn with a reason instead of hanging it.
   */
  conversation(
    input: TurnInput,
  ): Promise<ConversationBase | undefined> | ConversationBase | undefined;
};

/**
 * Run one turn through pi. The operation id is the turn's own idempotency key,
 * so pi treats a redelivered prompt as the turn it already ran.
 */
export function piTurnRunner(deps: PiTurnDeps): TurnRunner {
  return {
    async run(input: TurnInput): Promise<TurnResult> {
      try {
        const conversation = await deps.conversation(input);
        if (conversation === undefined) {
          return { kind: "unanswered", reason: "no agent is configured for this deployment" };
        }
        await conversation.setModel(input.model);
        const result = await conversation.submit(input.prompt, input.operationId);
        if (result.kind === "unanswered") return { kind: "unanswered", reason: result.reason };
        return { kind: "answered", text: await conversation.lastReply() };
      } catch (error) {
        return { kind: "unanswered", reason: reasonOf(error) };
      }
    },
  };
}

function reasonOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

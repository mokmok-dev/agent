/**
 * The normalized event every adapter produces. The rest of the control plane
 * branches on this type, never on a vendor payload, so adding a provider means
 * adding one adapter and no changes to the routing or session code.
 *
 * An adapter never throws. A body it cannot verify or read is a `refused`
 * result carrying the status the boundary answers with, so the Worker's one
 * `catch` covers only a bug in this repository.
 */

import type { Decoded } from "@agent/protocol";
import type { SignatureRefused } from "./signature.ts";

export type Provider = "slack" | "discord" | "linear";

export type RawRequest = {
  method: string;
  /** Header names in lower case, as the runtime gives them. */
  headers: Record<string, string | undefined>;
  /** The exact bytes that were signed. */
  body: string;
};

export type ThreadId = {
  workspaceId: string;
  channelId: string;
  threadId: string;
};

export type InboundMessage = {
  kind: "message";
  provider: Provider;
  /** The provider's id for this delivery, used to ignore a replay. */
  eventId: string;
  thread: ThreadId;
  authorId: string;
  /** The text, with the mention of the agent removed. */
  text: string;
  /** Where a reply should go when it differs from the thread. */
  replyTo: { channelId: string; threadId: string };
  /**
   * What kind of delivery this is. `start` opens a session, `prompt` adds to
   * one that is open; a provider without sessions sends `message`.
   */
  stage: "start" | "prompt" | "message";
};

export type InboundCancel = {
  kind: "cancel";
  provider: Provider;
  eventId: string;
  thread: ThreadId;
  authorId: string;
};

export type InboundChallenge = {
  kind: "challenge";
  provider: Provider;
  /** The exact body to return. */
  response: string;
};

export type InboundIgnored = {
  kind: "ignored";
  provider: Provider;
  reason: string;
};

export type InboundEvent = InboundMessage | InboundCancel | InboundChallenge | InboundIgnored;

/**
 * What an adapter answers. `refused` is a body it will not act on: a bad
 * signature, a payload it cannot read, or a route it does not serve.
 */
export type AdapterResult =
  | { kind: "event"; event: InboundMessage | InboundCancel }
  | { kind: "challenge"; response: string }
  | { kind: "ignored"; reason: string }
  | { kind: "refused"; status: number; message: string };

/** A refusal at the boundary, with the status the provider receives. */
export function refuse(status: number, message: string): AdapterResult {
  return { kind: "refused", status, message };
}

/** The refusal for a signature that could not be established. */
export function refuseSignature(refusal: SignatureRefused): AdapterResult {
  return refusal.kind === "absent"
    ? refuse(401, `missing ${refusal.header}`)
    : refuse(401, refusal.kind === "mismatch" ? "invalid signature" : refusal.message);
}

/**
 * Turn a decoder's answer into an adapter result. The decoder has already said
 * the body cannot be read, so this only adds the status a provider gets.
 */
export function decoded<T>(answer: Decoded<T>, into: (value: T) => AdapterResult): AdapterResult {
  return answer.kind === "decoded" ? into(answer.value) : refuse(answer.status, answer.message);
}

/** Parse a provider body, or the refusal that says it is not JSON. */
export function jsonOf(request: RawRequest): Decoded<unknown> {
  try {
    return { kind: "decoded", value: JSON.parse(request.body) as unknown };
  } catch {
    return { kind: "rejected", status: 400, message: "body is not JSON" };
  }
}

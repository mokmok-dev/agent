/**
 * The normalized event every adapter produces. The rest of the control plane
 * branches on this type, never on a vendor payload, so adding a provider means
 * adding one adapter and no changes to the routing or session code.
 */

export type Provider = "slack" | "discord" | "linear";

/** A request as the adapter receives it: method, headers, and body bytes. */
export type RawRequest = {
  method: string;
  /** Header names in lower case, as the runtime gives them. */
  headers: Record<string, string | undefined>;
  /** The exact bytes that were signed. */
  body: string;
};

/** The external conversation an inbound event belongs to. */
export type ThreadId = {
  workspaceId: string;
  channelId: string;
  threadId: string;
};

/** A user message that should become a turn. */
export type InboundMessage = {
  kind: "message";
  provider: Provider;
  /** The provider's id for this delivery, used to ignore a replay. */
  eventId: string;
  thread: ThreadId;
  /** Who sent it, as the provider names them. */
  authorId: string;
  /** The text, with the mention of the agent removed. */
  text: string;
  /** Where a reply should go when it differs from the thread. */
  replyTo: { channelId: string; threadId: string };
};

/** A user cancelled the running turn. */
export type InboundCancel = {
  kind: "cancel";
  provider: Provider;
  eventId: string;
  thread: ThreadId;
  authorId: string;
};

/** A provider asking us to prove we own the endpoint. */
export type InboundChallenge = {
  kind: "challenge";
  provider: Provider;
  /** The exact body to return. */
  response: string;
};

/** A delivery we accept and ignore, such as a bot's own message. */
export type InboundIgnored = {
  kind: "ignored";
  provider: Provider;
  reason: string;
};

export type InboundEvent = InboundMessage | InboundCancel | InboundChallenge | InboundIgnored;

/** What an adapter returns: an event to act on, or a response to acknowledge. */
export type AdapterResult =
  | { kind: "event"; event: InboundMessage | InboundCancel }
  | { kind: "challenge"; response: string }
  | { kind: "ignored"; reason: string };

export class AdapterError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

/**
 * Run a decoder at the boundary. A payload the decoder rejects becomes a 400,
 * so a malformed provider body never surfaces as a 500.
 */
export function decodeOr400<T>(decode: (value: unknown) => T, value: unknown): T {
  try {
    return decode(value);
  } catch (error) {
    throw new AdapterError(400, error instanceof Error ? error.message : String(error));
  }
}

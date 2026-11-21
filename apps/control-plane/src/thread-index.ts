import { threadKey, type ThreadRef } from "@agent/store";

/** Where a thread's session lives. Stable, so a lookup and a store agree. */
export const indexKey = (thread: ThreadRef): string => `thread:${threadKey(thread)}`;

/**
 * The session id for a thread. A thread never forks: the first delivery names
 * the session, and every later delivery of that thread reuses it.
 */
export function sessionIdFor(thread: ThreadRef): string {
  return threadKey(thread);
}

/** The reply address a turn's output goes back to. */
export type ReplyTarget = { provider: string; channelId: string; threadId: string };

/**
 * The thread a delivery belongs to, with the fallback a provider needs when it
 * names no thread of its own. A top-level message starts the thread it lives
 * in, so replies attach to that message.
 */
export function threadFor(
  provider: ThreadRef["provider"],
  workspaceId: string,
  channelId: string,
  threadId: string,
): ThreadRef {
  return { provider, workspaceId, channelId, threadId };
}

/** Whether two deliveries name the same conversation. */
export function sameThread(left: ThreadRef, right: ThreadRef): boolean {
  return threadKey(left) === threadKey(right);
}

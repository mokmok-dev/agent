/**
 * The commands a thread understands. The match is exact apart from case and
 * surrounding whitespace, so "stop now" stays a prompt.
 */
export const THREAD_COMMANDS = ["stop", "cancel"] as const;

export type ThreadCommand = (typeof THREAD_COMMANDS)[number];

/** The command a message is, or `undefined` when the message is a prompt. */
export function commandOf(text: string): ThreadCommand | undefined {
  const command = text.trim().toLowerCase();
  for (const candidate of THREAD_COMMANDS) {
    if (candidate === command) return candidate;
  }
  return undefined;
}

/** Whether a message ends the running turn. */
export function isCancelText(text: string): boolean {
  return commandOf(text) !== undefined;
}

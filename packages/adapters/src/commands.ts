export const THREAD_COMMANDS = ["stop", "cancel"] as const;

export type ThreadCommand = (typeof THREAD_COMMANDS)[number];

export function commandOf(text: string): ThreadCommand | undefined {
  const command = text.trim().toLowerCase();
  for (const candidate of THREAD_COMMANDS) {
    if (candidate === command) return candidate;
  }
  return undefined;
}

export function isCancelText(text: string): boolean {
  return commandOf(text) !== undefined;
}

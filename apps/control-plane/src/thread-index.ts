import { threadKey, type ThreadRef } from "@agent/store";

export const indexKey = (thread: ThreadRef): string => `thread:${threadKey(thread)}`;

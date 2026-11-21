import { describe, expect, test } from "bun:test";
import {
  AgentMethod,
  ClientMethod,
  PROTOCOL_VERSION,
  STOP_REASONS,
  TOOL_KINDS,
  TOOL_STATUSES,
  PERMISSION_KINDS,
  PLAN_STATUSES,
} from "./types.ts";

describe("wire names", () => {
  test("agent methods match ACP v1", () => {
    expect(AgentMethod).toEqual({
      Initialize: "initialize",
      Authenticate: "authenticate",
      NewSession: "session/new",
      LoadSession: "session/load",
      Prompt: "session/prompt",
      SetMode: "session/set_mode",
      Cancel: "session/cancel",
    });
  });

  test("client methods match ACP v1", () => {
    expect(ClientMethod).toEqual({
      SessionUpdate: "session/update",
      RequestPermission: "session/request_permission",
      ReadTextFile: "fs/read_text_file",
      WriteTextFile: "fs/write_text_file",
      CreateTerminal: "terminal/create",
      TerminalOutput: "terminal/output",
      WaitForTerminalExit: "terminal/wait_for_exit",
      KillTerminal: "terminal/kill",
      ReleaseTerminal: "terminal/release",
    });
  });

  test("the client speaks protocol version 1", () => {
    expect(PROTOCOL_VERSION).toBe(1);
  });

  test("every enum lists exactly the spec values", () => {
    expect(STOP_REASONS).toEqual([
      "end_turn",
      "max_tokens",
      "max_turn_requests",
      "refusal",
      "cancelled",
    ]);
    expect(TOOL_KINDS).toEqual([
      "read",
      "edit",
      "delete",
      "move",
      "search",
      "execute",
      "think",
      "fetch",
      "other",
    ]);
    expect(TOOL_STATUSES).toEqual(["pending", "in_progress", "completed", "failed"]);
    expect(PERMISSION_KINDS).toEqual([
      "allow_once",
      "allow_always",
      "reject_once",
      "reject_always",
    ]);
    expect(PLAN_STATUSES).toEqual(["pending", "in_progress", "completed"]);
  });

  test("no wire name is empty and every name is unique within its group", () => {
    for (const group of [
      Object.values(AgentMethod),
      Object.values(ClientMethod),
      STOP_REASONS,
      TOOL_KINDS,
      TOOL_STATUSES,
      PERMISSION_KINDS,
      PLAN_STATUSES,
    ]) {
      expect(new Set(group).size).toBe(group.length);
      for (const name of group) expect(name.length).toBeGreaterThan(0);
    }
  });
});

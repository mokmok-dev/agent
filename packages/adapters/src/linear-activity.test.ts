import { describe, expect, test } from "bun:test";
import {
  action,
  ACTIVITY_TYPES,
  activityVariables,
  AGENT_ACTIVITY_CREATE,
  failure,
  readActivityResult,
  response,
  sendActivity,
  thought,
} from "./linear-activity.ts";

const accepted = {
  data: { agentActivityCreate: { success: true, agentActivity: { id: "a-1" } } },
};

describe("the activity types Linear accepts", () => {
  test("the list is Linear's five", () => {
    expect(ACTIVITY_TYPES).toEqual(["thought", "action", "elicitation", "response", "error"]);
  });
});

describe("building content", () => {
  test("a thought carries only a body", () => {
    expect(thought("thinking")).toEqual({ type: "thought", body: "thinking" });
  });

  test("a response carries the answer", () => {
    expect(response("done")).toEqual({ type: "response", body: "done" });
  });

  test("a failure is Linear's error type", () => {
    expect(failure("broke")).toEqual({ type: "error", body: "broke" });
  });

  test("an action without a result is a call that has started", () => {
    expect(action("Searching", "weather")).toStrictEqual({
      type: "action",
      action: "Searching",
      parameter: "weather",
    });
  });

  test("an action with a result is a call that finished", () => {
    expect(action("Searched", "weather", "12C")).toStrictEqual({
      type: "action",
      action: "Searched",
      parameter: "weather",
      result: "12C",
    });
  });

  test("an empty result is still a result, so a finished call with no output says so", () => {
    expect(action("Searched", "weather", "")).toStrictEqual({
      type: "action",
      action: "Searched",
      parameter: "weather",
      result: "",
    });
  });

  test("the mutation names the input variable Linear expects", () => {
    expect(AGENT_ACTIVITY_CREATE).toContain("$input: AgentActivityCreateInput!");
    expect(AGENT_ACTIVITY_CREATE).toContain("agentActivityCreate(input: $input)");
  });

  test("the variables wrap the input", () => {
    expect(activityVariables({ agentSessionId: "s", content: thought("t") })).toEqual({
      input: { agentSessionId: "s", content: { type: "thought", body: "t" } },
    });
  });
});

describe("reading Linear's answer", () => {
  test("a success carries the activity id", () => {
    expect(readActivityResult(accepted)).toEqual({ kind: "sent", activityId: "a-1" });
  });

  test("a success without an id is still a success", () => {
    expect(readActivityResult({ data: { agentActivityCreate: { success: true } } })).toEqual({
      kind: "sent",
      activityId: null,
    });
  });

  test("a GraphQL error carries its message", () => {
    expect(readActivityResult({ errors: [{ message: "bad scope" }] })).toEqual({
      kind: "refused",
      error: "bad scope",
    });
  });

  test("a GraphQL error with no message still names the refusal", () => {
    expect(readActivityResult({ errors: [{}] })).toEqual({
      kind: "refused",
      error: "linear rejected the request",
    });
  });

  test("an answer with no payload is a refusal", () => {
    expect(readActivityResult({ data: {} })).toEqual({
      kind: "refused",
      error: "linear answered without an activity",
    });
  });

  test("a payload that says success false is Linear declining", () => {
    expect(readActivityResult({ data: { agentActivityCreate: { success: false } } })).toEqual({
      kind: "refused",
      error: "linear declined the activity",
    });
  });

  test("an answer that is not an object is a refusal rather than a crash", () => {
    expect(readActivityResult(undefined)).toEqual({
      kind: "refused",
      error: "linear answered without an activity",
    });
  });

  test("an answer that is a bare null is a refusal rather than a crash", () => {
    expect(readActivityResult(null)).toEqual({
      kind: "refused",
      error: "linear answered without an activity",
    });
  });

  test("errors that are not an array read as no errors, so the payload decides", () => {
    expect(
      readActivityResult({ errors: "nope", data: { agentActivityCreate: { success: true } } }),
    ).toEqual({ kind: "sent", activityId: null });
  });

  test("an empty errors array is not a refusal, so a successful answer with no errors passes", () => {
    expect(
      readActivityResult({ errors: [], data: { agentActivityCreate: { success: true } } }),
    ).toEqual({
      kind: "sent",
      activityId: null,
    });
  });

  test("a null error entry is handled without a throw", () => {
    expect(readActivityResult({ errors: [null] })).toEqual({
      kind: "refused",
      error: "linear rejected the request",
    });
  });

  test("a null activity is a success with no id, without a throw", () => {
    expect(
      readActivityResult({ data: { agentActivityCreate: { success: true, agentActivity: null } } }),
    ).toEqual({ kind: "sent", activityId: null });
  });

  test("an activity without an object is a success with no id", () => {
    expect(
      readActivityResult({ data: { agentActivityCreate: { success: true, agentActivity: 5 } } }),
    ).toEqual({ kind: "sent", activityId: null });
  });
});

describe("sending", () => {
  test("posts the query and variables with the token", async () => {
    const seen: { url: string; auth: string; body: { query: string; variables: unknown } }[] = [];
    const result = await sendActivity(
      "at-1",
      { agentSessionId: "s-1", content: response("hi") },
      async (url, init) => {
        seen.push({
          url,
          auth: init.headers["authorization"] ?? "",
          body: JSON.parse(init.body),
        });
        return accepted;
      },
    );
    expect(result).toEqual({ kind: "sent", activityId: "a-1" });
    expect(seen[0]?.url).toBe("https://api.linear.app/graphql");
    expect(seen[0]?.auth).toBe("Bearer at-1");
    expect(seen[0]?.body.query).toContain("agentActivityCreate");
    expect(seen[0]?.body.variables).toEqual({
      input: { agentSessionId: "s-1", content: { type: "response", body: "hi" } },
    });
  });

  test("an unreachable Linear is a refusal, not a throw", async () => {
    const result = await sendActivity(
      "at",
      { agentSessionId: "s", content: thought("t") },
      async () => {
        throw new Error("offline");
      },
    );
    expect(result).toEqual({
      kind: "refused",
      error: expect.stringContaining("linear unreachable"),
    });
  });

  test("the request is a JSON POST, which is what the GraphQL endpoint reads", async () => {
    const seen: { method: string; type: string }[] = [];
    await sendActivity("at", { agentSessionId: "s", content: thought("t") }, async (_url, init) => {
      seen.push({ method: init.method, type: init.headers["content-type"] ?? "" });
      return accepted;
    });
    expect(seen).toEqual([{ method: "POST", type: "application/json" }]);
  });
});

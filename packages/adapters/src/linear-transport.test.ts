import { describe, expect, test } from "bun:test";
import {
  postForm,
  postGraphql,
  revokeToken,
  type GraphqlBody,
  type OAuthRefused,
  type TokenBody,
  type TokenResponse,
} from "./linear-oauth.ts";

/** Serve one scripted answer per path, so the transport is exercised over HTTP. */
async function serve(answers: Record<string, { status: number; body: string }>): Promise<{
  url: string;
  close: () => void;
}> {
  const server = Bun.serve({
    port: 0,
    fetch(request) {
      const answer = answers[new URL(request.url).pathname];
      if (answer === undefined) return new Response("no answer", { status: 599 });
      return new Response(answer.body, { status: answer.status });
    },
  });
  return { url: `http://127.0.0.1:${server.port}`, close: () => server.stop(true) };
}

/** The refusal a transport returned, or a failure naming what it returned instead. */
function refusal(answer: TokenBody | GraphqlBody | OAuthRefused): OAuthRefused {
  if (answer.kind === "body") throw new Error("expected a refusal");
  return answer;
}

/** The body a transport read, or a failure naming the refusal. */
function body(answer: TokenBody): TokenResponse {
  if (answer.kind !== "body") throw new Error(`expected a body: ${answer.message}`);
  return answer.body;
}

const answeredWith = (response: TokenResponse): TokenBody => ({ kind: "body", body: response });

describe("the token transport", () => {
  test("posts a form body and reads the JSON answer", async () => {
    const seen: string[] = [];
    const server = Bun.serve({
      port: 0,
      async fetch(request) {
        seen.push(`${request.method} ${request.headers.get("content-type")}`);
        seen.push(await request.text());
        return Response.json({ access_token: "at" });
      },
    });
    try {
      const answer = await postForm(`http://127.0.0.1:${server.port}/oauth/token`, "grant_type=x");
      expect(body(answer).access_token).toBe("at");
      expect(seen[0]).toBe("POST application/x-www-form-urlencoded");
      expect(seen[1]).toBe("grant_type=x");
    } finally {
      server.stop(true);
    }
  });

  test("a JSON error answer is returned so the caller can quote it", async () => {
    const { url, close } = await serve({
      "/token": { status: 400, body: JSON.stringify({ error: "invalid_grant" }) },
    });
    try {
      expect(body(await postForm(`${url}/token`, "x=1")).error).toBe("invalid_grant");
    } finally {
      close();
    }
  });

  test("a non-JSON answer is a refusal that names the status", async () => {
    const { url, close } = await serve({ "/token": { status: 502, body: "<html>bad gateway" } });
    try {
      const answer = refusal(await postForm(`${url}/token`, "x=1"));
      expect(answer.kind).toBe("bad-response");
      expect(answer.message).toContain("502");
    } finally {
      close();
    }
  });

  test("an unsuccessful answer with no JSON error is a refusal", async () => {
    const { url, close } = await serve({
      "/token": { status: 500, body: JSON.stringify({ unexpected: true }) },
    });
    try {
      expect(refusal(await postForm(`${url}/token`, "x=1"))).toEqual({
        kind: "bad-response",
        message: "linear token endpoint answered 500",
      });
    } finally {
      close();
    }
  });

  test("an unreachable endpoint is a refusal, not a thrown transport error", async () => {
    const answer = refusal(await postForm("http://127.0.0.1:1/oauth/token", "x=1"));
    expect(answer.kind).toBe("unreachable");
    expect(answer.message).toContain("unreachable");
  });
});

describe("revocation", () => {
  test("sends the token as the `token` form field Linear reads", async () => {
    const forms: string[] = [];
    await revokeToken("at-1", async (_url, form) => {
      forms.push(form);
      return answeredWith({});
    });
    expect(new URLSearchParams(forms[0]).get("token")).toBe("at-1");
  });

  test("reports success when Linear accepts the token", async () => {
    expect(await revokeToken("at-1", async () => answeredWith({}))).toEqual({
      kind: "revoked",
      revoked: true,
    });
  });

  test("reports failure when Linear answers an error", async () => {
    expect(await revokeToken("at-1", async () => answeredWith({ error: "invalid_token" }))).toEqual(
      {
        kind: "revoked",
        revoked: false,
      },
    );
  });

  test("an unreachable endpoint is the refusal the transport returned", async () => {
    expect(
      await revokeToken("at-1", async () => ({
        kind: "unreachable",
        message: "linear token endpoint unreachable: offline",
      })),
    ).toEqual({ kind: "unreachable", message: "linear token endpoint unreachable: offline" });
  });
});

describe("the GraphQL transport", () => {
  test("posts the body and reads the JSON answer", async () => {
    const seen: string[] = [];
    const server = Bun.serve({
      port: 0,
      async fetch(request) {
        seen.push(`${request.method} ${await request.text()}`);
        return Response.json({ data: { viewer: { id: "app-1" } } });
      },
    });
    try {
      const answer = await postGraphql(`http://127.0.0.1:${server.port}/graphql`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: '{"query":"{viewer{id}}"}',
      });
      if (answer.kind !== "body") throw new Error("expected a body");
      expect(answer.body).toEqual({ data: { viewer: { id: "app-1" } } });
      expect(seen[0]).toBe('POST {"query":"{viewer{id}}"}');
    } finally {
      server.stop(true);
    }
  });

  test("a non-JSON answer is a refusal, so a caller sees a reason rather than a crash", async () => {
    const { url, close } = await serve({ "/graphql": { status: 502, body: "<html>" } });
    try {
      expect(
        refusal(
          await postGraphql(`${url}/graphql`, {
            method: "POST",
            headers: {},
            body: "{}",
          }),
        ),
      ).toEqual({ kind: "bad-response", message: "linear graphql answered 502" });
    } finally {
      close();
    }
  });

  test("an unreachable endpoint is a refusal", async () => {
    const answer = refusal(
      await postGraphql("http://127.0.0.1:1/graphql", { method: "POST", headers: {}, body: "{}" }),
    );
    expect(answer.kind).toBe("unreachable");
  });
});

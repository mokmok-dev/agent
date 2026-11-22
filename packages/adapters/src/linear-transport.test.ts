import { describe, expect, test } from "bun:test";
import { postForm, revokeToken, type TokenResponse } from "./linear-oauth.ts";
import { AdapterError } from "./events.ts";

/** Serve one scripted answer per path, so the transport is exercised over HTTP. */
function serve(answers: Record<string, { status: number; body: string }>): Promise<{
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
  return Promise.resolve({
    url: `http://127.0.0.1:${server.port}`,
    close: () => server.stop(true),
  });
}

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
      const body = await postForm(`http://127.0.0.1:${server.port}/oauth/token`, "grant_type=x");
      expect(body.access_token).toBe("at");
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
      expect((await postForm(`${url}/token`, "x=1")).error).toBe("invalid_grant");
    } finally {
      close();
    }
  });

  test("a non-JSON answer is a refusal that names the status", async () => {
    const { url, close } = await serve({ "/token": { status: 502, body: "<html>bad gateway" } });
    try {
      const error = await refused(() => postForm(`${url}/token`, "x=1"));
      expect(error.status).toBe(502);
      expect(error.message).toContain("502");
    } finally {
      close();
    }
  });

  test("an unsuccessful answer with no JSON error is a refusal", async () => {
    const { url, close } = await serve({
      "/token": { status: 500, body: JSON.stringify({ unexpected: true }) },
    });
    try {
      const error = await refused(() => postForm(`${url}/token`, "x=1"));
      expect(error.status).toBe(502);
      expect(error.message).toBe("linear token endpoint answered 500");
    } finally {
      close();
    }
  });

  test("an unreachable endpoint is a refusal, not a thrown transport error", async () => {
    const error = await refused(() => postForm("http://127.0.0.1:1/oauth/token", "x=1"));
    expect(error.status).toBe(502);
    expect(error.message).toContain("unreachable");
  });
});

describe("revocation", () => {
  test("sends the token as the `token` form field Linear reads", async () => {
    const forms: string[] = [];
    await revokeToken("at-1", async (_url, form) => {
      forms.push(form);
      return {} as TokenResponse;
    });
    expect(new URLSearchParams(forms[0]).get("token")).toBe("at-1");
  });

  test("reports success when Linear accepts the token", async () => {
    expect(await revokeToken("at-1", async () => ({ ok: true }) as TokenResponse)).toBe(true);
  });

  test("reports failure when Linear answers an error", async () => {
    expect(
      await revokeToken("at-1", async () => ({ error: "invalid_token" }) as TokenResponse),
    ).toBe(false);
  });

  test("reports failure when the endpoint is unreachable, so disconnect still completes", async () => {
    expect(
      await revokeToken("at-1", async () => {
        throw new Error("offline");
      }),
    ).toBe(false);
  });
});

async function refused(run: () => Promise<unknown>): Promise<AdapterError> {
  try {
    await run();
  } catch (error) {
    if (error instanceof AdapterError) return error;
    throw error;
  }
  throw new Error("expected a refusal");
}

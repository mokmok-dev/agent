import { describe, expect, it } from "vitest";
import fc from "fast-check";

import { requestPath, routeOf } from "./routes.js";

const expectedRoute = (method: string, url: string): string => {
  if (method !== "GET") {
    return "NotFound";
  }
  const index = url.indexOf("?");
  const path = index === -1 ? url : url.slice(0, index);
  if (path === "/health") {
    return "Health";
  }
  if (path === "/ws") {
    return "WebSocket";
  }
  return "NotFound";
};

describe("routing", () => {
  it("drops everything from the first question mark onward in the request path", () => {
    expect(requestPath("/health?x=1")).toBe("/health");
    expect(requestPath("/health")).toBe("/health");
    expect(requestPath("/a?b?c")).toBe("/a");
    expect(requestPath("?x=1")).toBe("");
  });

  it("routes Health only for GET /health", () => {
    fc.assert(
      fc.property(fc.string(), fc.string(), (method, url) => {
        expect(routeOf(method, url)._tag).toBe(expectedRoute(method, url));
      }),
    );
  });

  it("matches Health for GET /health and ignores the query string", () => {
    expect(routeOf("GET", "/health")._tag).toBe("Health");
    expect(routeOf("GET", "/health?x=1")._tag).toBe("Health");
  });

  it("matches WebSocket for GET /ws", () => {
    expect(routeOf("GET", "/ws")._tag).toBe("WebSocket");
    expect(routeOf("GET", "/ws?token=1")._tag).toBe("WebSocket");
  });

  it("routes NotFound for any other method or path", () => {
    expect(routeOf("POST", "/health")._tag).toBe("NotFound");
    expect(routeOf("get", "/health")._tag).toBe("NotFound");
    expect(routeOf("GET", "/nope")._tag).toBe("NotFound");
    expect(routeOf("GET", "/ws/extra")._tag).toBe("NotFound");
  });
});

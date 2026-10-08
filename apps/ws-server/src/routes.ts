export type Route =
  | { readonly _tag: "Health" }
  | { readonly _tag: "WebSocket" }
  | { readonly _tag: "NotFound" };

export const requestPath = (url: string): string => {
  const index = url.indexOf("?");
  return index === -1 ? url : url.slice(0, index);
};

export const routeOf = (method: string, url: string): Route => {
  if (method !== "GET") {
    return { _tag: "NotFound" };
  }
  const path = requestPath(url);
  if (path === "/health") {
    return { _tag: "Health" };
  }
  if (path === "/ws") {
    return { _tag: "WebSocket" };
  }
  return { _tag: "NotFound" };
};

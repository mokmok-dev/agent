import { describe, expect, test } from "bun:test";
import { PendingPermission } from "./permission.ts";

describe("PendingPermission", () => {
  test("the chosen option reaches the waiter", async () => {
    const permission = new PendingPermission();
    permission.decide("allow");
    expect(await permission.chosen()).toBe("allow");
  });

  test("a declined request settles as undefined", async () => {
    const permission = new PendingPermission();
    permission.decide(undefined);
    expect(await permission.chosen()).toBeUndefined();
  });

  test("the waiter sees nothing until a decision arrives", async () => {
    const permission = new PendingPermission();
    let settled = false;
    void permission.chosen().then(() => {
      settled = true;
    });
    await Promise.resolve();
    expect(settled).toBe(false);
    permission.decide("deny");
    await Promise.resolve();
    await Promise.resolve();
    expect(settled).toBe(true);
  });
});

import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Effect, Fiber, Layer } from "effect";
import { SandboxManager } from "@anthropic-ai/sandbox-runtime";
import { describe, expect, it } from "vitest";

import {
  ApprovalDesk,
  ApprovalDeskLayer,
  type BasePolicy,
  layerReal,
  type SandboxBroker,
  withSession,
} from "./index.js";

// srt wraps commands with `/bin/bash` unless told otherwise, and a host without a
// merged `/usr` has no `/bin/bash`. The wrapped argv is spawned directly here, so
// the session has to name a shell that exists.
const shell = [
  process.env.SHELL,
  "/bin/bash",
  "/usr/bin/bash",
  "/run/current-system/sw/bin/bash",
  "/bin/sh",
].find((candidate) => candidate !== undefined && statSync(candidate, { throwIfNoEntry: false }));

const skipIfUnsupported =
  !SandboxManager.isSupportedPlatform() ||
  SandboxManager.checkDependencies().errors.length > 0 ||
  shell === undefined;

const pollPending = (desk: ApprovalDesk, maxAttempts = 100) =>
  Effect.gen(function* () {
    let attempts = 0;
    while (true) {
      const pending = yield* desk.pending();
      if (pending.length > 0) {
        return pending;
      }
      attempts += 1;
      if (attempts > maxAttempts) {
        return yield* Effect.die("no pending decision arrived in time");
      }
      yield* Effect.sleep("50 millis");
    }
  });

const waitForExit = (child: ReturnType<typeof spawn>, timeoutMs = 15000): Promise<number | null> =>
  new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error("child did not exit in time"));
    }, timeoutMs);
    child.on("exit", (code) => {
      clearTimeout(timer);
      resolve(code);
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });

const wrapAndSpawn = (session: SandboxBroker, command: string) =>
  Effect.gen(function* () {
    const wrapped = yield* session.wrap(command);
    const argv0 = wrapped.argv[0];
    if (argv0 === undefined) {
      return yield* Effect.die("wrapped argv is empty");
    }
    const child = spawn(argv0, wrapped.argv.slice(1), {
      env: { ...process.env, ...wrapped.env },
      stdio: ["ignore", "ignore", "pipe"],
    });
    return { child, wait: () => waitForExit(child) };
  });

describe("sandbox-runtime integration", () => {
  it.skipIf(skipIfUnsupported)(
    "proves live network grants, ledger denials, next-wrap filesystem grants, and teardown",
    async () => {
      const dir = mkdtempSync(join(tmpdir(), "sandbox-broker-integ-"));
      const writableDir = join(dir, "writable");
      // srt binds concrete paths on Linux, so a write grant can only name a
      // directory that is already there when the command is wrapped.
      mkdirSync(writableDir, { recursive: true });

      const base: BasePolicy = {
        allowedDomains: [],
        deniedDomains: [],
        denyRead: [],
        allowWrite: [],
        denyWrite: [],
      };

      const program = withSession(
        base,
        (session) =>
          Effect.gen(function* () {
            const desk = yield* ApprovalDesk;

            const { wait: waitNetwork } = yield* wrapAndSpawn(
              session,
              `for i in $(seq 1 40); do curl -fsSL https://example.com >/dev/null 2>/dev/null && exit 0; sleep 0.2; done; exit 1`,
            );

            const firstPending = yield* pollPending(desk);
            const firstAsk = firstPending[0];
            if (firstAsk === undefined) {
              return yield* Effect.die("first ask missing");
            }
            expect(firstAsk.origin).toBe("Attempted");
            expect(firstAsk.request).toEqual({ _tag: "AllowHost", pattern: "example.com:443" });
            yield* desk.decide({
              _tag: "Decision",
              requestId: firstAsk.requestId,
              outcome: { _tag: "Denied" },
            });

            const { wait: waitNetwork2 } = yield* wrapAndSpawn(
              session,
              `curl -fsSL https://example.com >/dev/null 2>/dev/null && exit 0; exit 3`,
            );
            expect(yield* Effect.promise(() => waitNetwork2())).toBe(3);
            expect(yield* desk.pending()).toHaveLength(0);

            const predictedHost = { _tag: "AllowHost", pattern: "example.com:443" } as const;
            const predictedFiber = yield* Effect.forkDetach(session.submit(predictedHost), {
              startImmediately: true,
            });
            const predictedPending = yield* pollPending(desk);
            const predictedEntry = predictedPending[0];
            if (predictedEntry === undefined) {
              return yield* Effect.die("predicted entry missing");
            }
            expect(predictedEntry.origin).toBe("Predicted");
            expect(predictedEntry.request).toEqual(predictedHost);
            yield* desk.decide({
              _tag: "Decision",
              requestId: predictedEntry.requestId,
              outcome: { _tag: "Approved" },
            });
            yield* Fiber.join(predictedFiber);

            const restrictions = SandboxManager.getNetworkRestrictionConfig();
            expect(restrictions.allowedHosts).toContain("example.com:443");

            const networkExit = yield* Effect.promise(() => waitNetwork());
            expect(networkExit).toBe(0);

            const { child: writeChild, wait: waitWrite } = yield* wrapAndSpawn(
              session,
              `for i in $(seq 1 40); do echo x > ${writableDir}/file 2>/dev/null && exit 0; sleep 0.2; done; exit 1`,
            );
            yield* Effect.sleep("400 millis");

            const fsGrant = { _tag: "AllowWrite", target: writableDir } as const;
            const fsFiber = yield* Effect.forkDetach(session.submit(fsGrant), {
              startImmediately: true,
            });
            const fsPending = yield* pollPending(desk);
            const fsEntry = fsPending[0];
            if (fsEntry === undefined) {
              return yield* Effect.die("fs entry missing");
            }
            expect(fsEntry.origin).toBe("Predicted");
            yield* desk.decide({
              _tag: "Decision",
              requestId: fsEntry.requestId,
              outcome: { _tag: "Approved" },
            });
            const fsGranted = yield* Fiber.join(fsFiber);
            expect(fsGranted.effect).toEqual({ _tag: "NextWrap" });

            yield* Effect.sleep("500 millis");
            expect(writeChild.exitCode).toBeNull();
            writeChild.kill("SIGKILL");
            yield* Effect.promise(() => waitWrite().catch(() => null));

            const { wait: waitWrite2 } = yield* wrapAndSpawn(
              session,
              `echo x > ${writableDir}/file2 && exit 0 || exit 1`,
            );
            const writeExit2 = yield* Effect.promise(() => waitWrite2());
            expect(writeExit2).toBe(0);

            rmSync(dir, { recursive: true, force: true });
            return;
          }),
        shell,
      ).pipe(Effect.provide(Layer.merge(layerReal, ApprovalDeskLayer)));

      await Effect.runPromise(program);

      expect(SandboxManager.getProxyPort()).toBeUndefined();
      expect(SandboxManager.getSocksProxyPort()).toBeUndefined();
    },
  );
});

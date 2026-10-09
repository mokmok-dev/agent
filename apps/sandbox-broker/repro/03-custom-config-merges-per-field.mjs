// Probe 03: customConfig merges per field, and reset() leaves the config installed.
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SandboxManager } from "@anthropic-ai/sandbox-runtime";

const assert = (ok, what) => {
  console.log(ok ? `ok: ${what}` : `FAIL: ${what}`);
  if (!ok) process.exit(1);
};

const session = mkdtempSync(join(tmpdir(), "srt-repro-session-"));
const other = mkdtempSync(join(tmpdir(), "srt-repro-other-"));

const run = async (command, customConfig) => {
  const { argv, env } = await SandboxManager.wrapWithSandboxArgv(
    command,
    process.env.SHELL,
    customConfig,
  );
  const child = spawn(argv[0], argv.slice(1), { env, stdio: ["ignore", "pipe", "pipe"] });
  let text = "";
  child.stdout.on("data", (chunk) => (text += String(chunk)));
  child.stderr.on("data", (chunk) => (text += String(chunk)));
  await new Promise((resolve) => child.on("exit", resolve));
  return text;
};

await SandboxManager.initialize({
  network: { allowedDomains: [], deniedDomains: [] },
  filesystem: { denyRead: [], allowWrite: [session], denyWrite: [] },
});
const portsBefore = [SandboxManager.getProxyPort(), SandboxManager.getSocksProxyPort()];

const withoutPatch = await run(
  `sh -c 'echo hi > ${session}/a.txt && echo session-list=ok' 2>&1 | tail -1`,
);
const withPatch = await run(
  `sh -c 'echo hi > ${session}/b.txt && echo session-list=still-ok' 2>&1 | tail -1`,
  {
    filesystem: { denyRead: [], allowWrite: [other], denyWrite: [] },
  },
);

assert(withoutPatch.includes("session-list=ok"), "the session's allowWrite applied with no patch");
assert(!withPatch.includes("still-ok"), "a patch naming another allowWrite dropped the session's");
assert(withPatch.includes("Read-only file system"), "the second write was refused");

await SandboxManager.reset();
const portsAfter = [SandboxManager.getProxyPort(), SandboxManager.getSocksProxyPort()];

assert(
  portsBefore.every((port) => port !== undefined),
  "the proxies were listening during the session",
);
assert(
  portsAfter.every((port) => port === undefined),
  "reset closed the proxies",
);
assert(
  SandboxManager.getConfig() !== undefined,
  "reset left the config installed, so it is not a teardown signal",
);

rmSync(session, { recursive: true, force: true });
rmSync(other, { recursive: true, force: true });

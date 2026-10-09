// Probe 02: a filesystem rule cannot reach a running command, only the next wrap.
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SandboxManager } from "@anthropic-ai/sandbox-runtime";

const assert = (ok, what) => {
  console.log(ok ? `ok: ${what}` : `FAIL: ${what}`);
  if (!ok) process.exit(1);
};

const dir = mkdtempSync(join(tmpdir(), "srt-repro-fs-"));
const base = {
  network: { allowedDomains: [], deniedDomains: [] },
  filesystem: { denyRead: [], allowWrite: [], denyWrite: [] },
};

const capture = (child) => {
  let text = "";
  child.stdout?.on("data", (chunk) => (text += String(chunk)));
  child.stderr?.on("data", (chunk) => (text += String(chunk)));
  return () => text;
};

const runToEnd = async (wrapped) => {
  const child = spawn(wrapped, { shell: true, stdio: ["ignore", "pipe", "pipe"] });
  const text = capture(child);
  await new Promise((resolve) => child.on("exit", resolve));
  return text();
};

await SandboxManager.initialize(base);

const wrapped = await SandboxManager.wrapWithSandbox(
  `for i in 1 2 3 4 5; do printf 'try%s ' "$i"; sh -c 'echo hi > ${dir}/from-loop && echo loop-wrote' 2>&1 | tail -1; sleep 2; done`,
);
const loop = spawn(wrapped, { shell: true, stdio: ["ignore", "pipe", "pipe"] });
const loopText = capture(loop);
await new Promise((resolve) => setTimeout(resolve, 4500));

SandboxManager.updateConfig({
  ...base,
  filesystem: { denyRead: [], allowWrite: [dir], denyWrite: [] },
});
await new Promise((resolve) => loop.on("exit", resolve));

const afterUpdate = await runToEnd(
  await SandboxManager.wrapWithSandbox(
    `sh -c 'echo hi > ${dir}/after.txt && echo after-update=ok' 2>&1 | tail -1`,
  ),
);

const custom = await SandboxManager.wrapWithSandboxArgv(
  `sh -c 'echo hi > ${dir}/custom.txt && echo custom-config=ok'`,
  process.env.SHELL,
  { filesystem: { denyRead: [], allowWrite: [dir], denyWrite: [] } },
);
const customChild = spawn(custom.argv[0], custom.argv.slice(1), {
  env: custom.env,
  stdio: ["ignore", "pipe", "pipe"],
});
const customText = capture(customChild);
await new Promise((resolve) => customChild.on("exit", resolve));

assert(
  loopText().split("Read-only file system").length - 1 >= 3,
  "the running command was refused the write before the update",
);
assert(
  !loopText().includes("loop-wrote"),
  "the running command never wrote, even after the update",
);
assert(afterUpdate.includes("after-update=ok"), "a wrap issued after the update could write");
assert(
  customText().includes("custom-config=ok"),
  "a wrap carrying its own customConfig could write",
);

await SandboxManager.reset();
rmSync(dir, { recursive: true, force: true });

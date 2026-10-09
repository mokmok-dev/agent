// Probe 04: a Linux write grant binds a concrete path, so the path must exist at wrap time.
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SandboxManager } from "@anthropic-ai/sandbox-runtime";

const assert = (ok, what) => {
  console.log(ok ? `ok: ${what}` : `FAIL: ${what}`);
  if (!ok) process.exit(1);
};

const dir = mkdtempSync(join(tmpdir(), "srt-repro-grant-"));
const ghost = join(dir, "ghost");

const run = async (command) => {
  const { argv, env } = await SandboxManager.wrapWithSandboxArgv(command, process.env.SHELL);
  const child = spawn(argv[0], argv.slice(1), { env, stdio: ["ignore", "pipe", "pipe"] });
  let text = "";
  child.stdout.on("data", (chunk) => (text += String(chunk)));
  child.stderr.on("data", (chunk) => (text += String(chunk)));
  const code = await new Promise((resolve) => child.on("exit", resolve));
  return { code, text };
};

await SandboxManager.initialize({
  network: { allowedDomains: [], deniedDomains: [] },
  filesystem: { denyRead: [], allowWrite: [ghost], denyWrite: [] },
});

const absent = await run(`sh -c 'mkdir -p ${ghost} && echo x > ${ghost}/f && echo write=ok'`);
mkdirSync(ghost, { recursive: true });
const present = await run(`sh -c 'echo x > ${ghost}/f2 && echo write=ok'`);

assert(absent.code !== 0, "a grant for a directory that does not exist was refused");
assert(absent.text.includes("Read-only file system"), "the refusal was the read-only sandbox root");
assert(
  present.code === 0 && present.text.includes("write=ok"),
  "the same grant worked once the directory existed",
);

await SandboxManager.reset();
rmSync(dir, { recursive: true, force: true });

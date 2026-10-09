// Probe 05: the wrapped argv names a shell, and srt's own default is /bin/bash.
import { spawn } from "node:child_process";
import { statSync } from "node:fs";
import { SandboxManager } from "@anthropic-ai/sandbox-runtime";

const assert = (ok, what) => {
  console.log(ok ? `ok: ${what}` : `FAIL: ${what}`);
  if (!ok) process.exit(1);
};

await SandboxManager.initialize({
  network: { allowedDomains: ["example.com"], deniedDomains: [] },
  filesystem: { denyRead: [], allowWrite: [], denyWrite: [] },
});

const command = `curl -s -o /dev/null -w 'code=%{http_code}' --max-time 5 http://example.com/; echo`;

const attempt = ({ argv, env }) =>
  new Promise((resolve) => {
    const child = spawn(argv[0], argv.slice(1), { env, stdio: ["ignore", "inherit", "inherit"] });
    child.on("error", (error) => resolve({ kind: "error", error }));
    child.on("exit", (code) => resolve({ kind: "exit", code }));
  });

const plain = await SandboxManager.wrapWithSandboxArgv(command);
console.log("default binShell, argv[0] =", JSON.stringify(plain.argv[0]));
assert(plain.argv[0] === "/bin/bash", "the default wrapper names /bin/bash");

const plainResult = await attempt(plain);
if (statSync("/bin/bash", { throwIfNoEntry: false }) === undefined) {
  assert(
    plainResult.kind === "error",
    "spawning the default argv failed where /bin/bash is absent",
  );
} else {
  assert(plainResult.kind === "exit", "spawning the default argv ran where /bin/bash exists");
}

assert(process.env.SHELL !== undefined, "the run named a shell in SHELL");
const explicit = await SandboxManager.wrapWithSandboxArgv(command, process.env.SHELL);
console.log("explicit binShell, argv[0] =", JSON.stringify(explicit.argv[0]));
assert(explicit.argv[0] === process.env.SHELL, "binShell becomes argv[0]");
assert((await attempt(explicit)).kind === "exit", "spawning the explicit argv ran");

await SandboxManager.reset();

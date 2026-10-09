// Probe 01: updateConfig changes what a RUNNING sandboxed process may reach.
import { spawn } from "node:child_process";
import { SandboxManager } from "@anthropic-ai/sandbox-runtime";

const assert = (ok, what) => {
  console.log(ok ? `ok: ${what}` : `FAIL: ${what}`);
  if (!ok) process.exit(1);
};

const base = {
  network: { allowedDomains: [], deniedDomains: [] },
  filesystem: { denyRead: [], allowWrite: [], denyWrite: [] },
};

await SandboxManager.initialize(base);

const wrapped = await SandboxManager.wrapWithSandbox(
  `for i in 1 2 3 4 5 6; do printf 'http=%s ' "$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://example.com/)"; sleep 2; done; echo`,
);
const child = spawn(wrapped, { shell: true, stdio: ["ignore", "pipe", "inherit"] });
let output = "";
child.stdout.on("data", (chunk) => {
  output += String(chunk);
  process.stdout.write(chunk);
});

await new Promise((resolve) => setTimeout(resolve, 5000));
console.log(
  "allow list before:",
  JSON.stringify(SandboxManager.getNetworkRestrictionConfig().allowedHosts),
);
SandboxManager.updateConfig({
  ...base,
  network: { allowedDomains: ["example.com"], deniedDomains: [] },
});
console.log(
  "allow list after: ",
  JSON.stringify(SandboxManager.getNetworkRestrictionConfig().allowedHosts),
);

await new Promise((resolve) => child.on("exit", resolve));

const codes = [...output.matchAll(/http=(\d+)/g)].map((match) => match[1]);
const blocked = codes.indexOf("403");
const allowed = codes.indexOf("200");
assert(blocked !== -1, "a request before the update was refused");
assert(allowed > blocked, "a request after the update was allowed, in the same process");
assert(!codes.slice(0, blocked).includes("200"), "no request before the update reached the host");

await SandboxManager.reset();

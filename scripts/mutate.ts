/**
 * Runs mizchi/mutator in every workspace package and fails when a mutant is
 * undetected. The gate is "Killed": a Survived or NoCoverage mutant exits 1.
 */
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const ROOT = join(import.meta.dir, "..");
const CLI = join(ROOT, "node_modules/@mizchi/mutator/dist/cli.js");

type Target = { name: string; dir: string };

function targets(): Target[] {
  const found: Target[] = [];
  for (const group of ["packages", "apps"]) {
    const base = join(ROOT, group);
    if (!existsSync(base)) continue;
    for (const name of readdirSync(base).sort()) {
      const dir = join(base, name);
      if (existsSync(join(dir, "mutator.config.json")))
        found.push({ name: `${group}/${name}`, dir });
    }
  }
  return found;
}

async function run(target: Target): Promise<{ name: string; ok: boolean; detail: string }> {
  const proc = Bun.spawn({
    cmd: [process.execPath, CLI, "--reporter", "json", "--concurrency", "2"],
    cwd: target.dir,
    stdout: "pipe",
    stderr: "pipe",
  });
  const exitCode = await proc.exited;
  const stderr = await new Response(proc.stderr).text();
  const reportPath = join(target.dir, ".mutator/report/mutation.json");
  if (!existsSync(reportPath)) {
    return { name: target.name, ok: false, detail: stderr.trim() || "no report written" };
  }
  const report = JSON.parse(readFileSync(reportPath, "utf8")) as {
    files: Record<string, { mutants?: Array<{ status: string }> }>;
  };
  const statuses = Object.values(report.files).flatMap((file) =>
    (file.mutants ?? []).map((mutant) => mutant.status),
  );
  const undetected = statuses.filter((status) => status === "Survived" || status === "NoCoverage");
  const killed = statuses.filter((status) => status === "Killed" || status === "Timeout");
  return {
    name: target.name,
    ok: exitCode === 0 && undetected.length === 0 && statuses.length > 0,
    detail: `${killed.length}/${statuses.length} killed`,
  };
}

const found = targets();
if (found.length === 0) {
  console.error("mutate: no package has a mutator.config.json");
  process.exit(1);
}

const results = await Promise.all(found.map(run));
let failed = false;
for (const result of results) {
  console.log(`${result.ok ? "PASS" : "FAIL"}  ${result.name}  ${result.detail}`);
  if (!result.ok) failed = true;
}
process.exit(failed ? 1 : 0);

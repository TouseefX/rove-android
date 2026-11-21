// Example adapter for a Node.js env logger. Paths are passed directly to the CLI.
import { execFile } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const run = promisify(execFile);
const defaultTool = fileURLToPath(new URL('./rove-stellar.exe', import.meta.url));

export async function buildProbe(sourcePath, modelPath, tool = defaultTool) {
  const { stdout } = await run(tool, ['pack', sourcePath, modelPath], { windowsHide: true });
  return stdout.trim();
}

// Pass the digest returned by buildProbe and the place ID of your published test copy.
// Multiple Player sessions can append reports for the same probe.
export async function readProbeReports(logPath, digest, placeId, tool = defaultTool) {
  const { stdout } = await run(tool, ['read-report', logPath], {
    encoding: 'utf8', maxBuffer: 4 * 1024 * 1024, windowsHide: true,
  });
  return stdout.trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line))
    .filter(report => report.sourceSha256 === digest && report.placeId === String(placeId));
}

/**
 * Test fixture server: runs the deterministic fake OpenAI provider as a child
 * process so tests in other languages (the Rust adapter) can exercise the real
 * helper against a real HTTP endpoint.
 *
 * Usage: node --import tsx test/serve-fixture.ts <steps.json> <captures.json>
 * Prints one line `LISTENING <base-url>` on stdout once ready. On SIGTERM or
 * after the first request to `/__shutdown`, writes captures and exits.
 */

import { writeFileSync } from "node:fs";
import { FakeProvider, type ScriptStep } from "./fake-provider.ts";

const stepsPath = process.argv[2];
const capturesPath = process.argv[3];
if (stepsPath === undefined || capturesPath === undefined) {
  process.stderr.write("usage: serve-fixture.ts <steps.json> <captures.json>\n");
  process.exit(2);
}

const steps = JSON.parse(process.env.FIXTURE_STEPS ?? "[]") as ScriptStep[];
const fixedPort = Number.parseInt(process.env.FIXTURE_PORT ?? "", 10);
const provider = new FakeProvider(steps);
await provider.start(Number.isFinite(fixedPort) ? fixedPort : 0);

const dump = () => {
  try {
    writeFileSync(
      capturesPath,
      JSON.stringify(
        provider.captures.map((capture) => ({
          body: capture.body,
          headers: capture.headers,
        })),
        null,
        2,
      ),
    );
  } catch (error) {
    process.stderr.write(`failed to write captures: ${String(error)}\n`);
  }
};

process.on("SIGTERM", () => {
  dump();
  process.exit(0);
});
process.on("SIGINT", () => {
  dump();
  process.exit(0);
});

// Cross-language tests stop the process with a hard kill, so persist captures
// eagerly after every request instead of relying on a shutdown hook.
provider.onCapture = dump;
dump();

process.stdout.write(`LISTENING ${provider.baseUrl}\n`);
// Keep the process alive; captures are written when the parent stops us.
setInterval(() => {}, 1 << 30);

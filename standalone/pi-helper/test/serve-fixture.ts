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
import { connect } from "node:net";
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

// The parent (the Rust adapter harness) reads exactly one line from our stdout
// and then drops the read end of the pipe. A later stdout write would surface
// as EPIPE, and an unhandled stream error would kill the fixture mid-request;
// keep the process alive and log instead.
process.stdout.on("error", (error) => {
  process.stderr.write(`serve-fixture: stdout error: ${String(error)}\n`);
});
process.on("uncaughtException", (error) => {
  process.stderr.write(`serve-fixture: uncaught exception: ${String(error)}\n`);
});
process.on("unhandledRejection", (reason) => {
  process.stderr.write(`serve-fixture: unhandled rejection: ${String(reason)}\n`);
});

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

/**
 * Confirm the listener accepts TCP connections before announcing readiness.
 * The `listening` event fires once the socket is bound; a client on a slower
 * platform can still race the first accept, so probe the loopback address the
 * provider reports (and only that; it must not add a captured HTTP request).
 */
async function waitUntilAccepting(baseUrl: string): Promise<void> {
  const { hostname, port } = new URL(baseUrl);
  const lastError = await new Promise<Error | undefined>((resolve) => {
    let attempts = 0;
    const attempt = () => {
      const socket = connect({ host: hostname, port: Number(port) });
      socket.once("connect", () => {
        socket.destroy();
        resolve(undefined);
      });
      socket.once("error", (error) => {
        socket.destroy();
        attempts += 1;
        if (attempts >= 100) {
          resolve(error);
        } else {
          setTimeout(attempt, 20);
        }
      });
    };
    attempt();
  });
  if (lastError !== undefined) {
    throw lastError;
  }
}

await provider.start(Number.isFinite(fixedPort) ? fixedPort : 0);

// Cross-language tests stop the process with a hard kill, so persist captures
// eagerly after every request instead of relying on a shutdown hook.
provider.onCapture = dump;
dump();

try {
  await waitUntilAccepting(provider.baseUrl);
} catch (error) {
  process.stderr.write(`serve-fixture: listener never accepted a connection: ${String(error)}\n`);
  process.exit(1);
}
process.stdout.write(`LISTENING ${provider.baseUrl}\n`);
// Keep the process alive; captures are written when the parent stops us.
setInterval(() => {}, 1 << 30);

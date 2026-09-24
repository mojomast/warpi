/**
 * Test harness: spawns the compiled helper as a child process and speaks the
 * v1 stdio protocol with it, exactly like the Rust backend does.
 */

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface, type Interface } from "node:readline";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";

const here = dirname(fileURLToPath(import.meta.url));
export const HELPER_ENTRY = join(here, "..", "dist", "main.js");

export interface HelperFrame {
  protocol: number;
  seq: number;
  kind: string;
  session_id?: string;
  turn_id?: string;
  exchange_id?: string;
  generation?: number;
  data?: Record<string, unknown>;
}

export interface HelperClientOptions {
  /** Extra environment variables; the helper is launched with a minimal env. */
  env?: Record<string, string>;
  cwd?: string;
}

export class HelperClient {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly frames: HelperFrame[] = [];
  private readonly waiters: Array<{
    predicate: (frame: HelperFrame) => boolean;
    resolve: (frame: HelperFrame) => void;
    reject: (error: Error) => void;
    timer: NodeJS.Timeout;
  }> = [];
  private seq = 0;
  readonly stderrLines: string[] = [];
  readonly dataDir: string;
  private exited = false;
  private readonly outReader: Interface;
  private readonly errReader: Interface;

  private constructor(child: ChildProcessWithoutNullStreams, dataDir: string) {
    this.child = child;
    this.dataDir = dataDir;
    const reader = createInterface({ input: child.stdout });
    this.outReader = reader;
    reader.on("line", (line) => {
      if (line.trim().length === 0) return;
      let frame: HelperFrame;
      try {
        frame = JSON.parse(line) as HelperFrame;
      } catch {
        throw new Error(`helper wrote a non-JSON line to stdout: ${line}`);
      }
      this.frames.push(frame);
      for (const waiter of [...this.waiters]) {
        if (!waiter.predicate(frame)) continue;
        this.waiters.splice(this.waiters.indexOf(waiter), 1);
        clearTimeout(waiter.timer);
        waiter.resolve(frame);
      }
    });
    const stderr = createInterface({ input: child.stderr });
    this.errReader = stderr;
    stderr.on("line", (line) => this.stderrLines.push(line));
    child.on("exit", () => {
      this.exited = true;
      for (const waiter of this.waiters.splice(0)) {
        clearTimeout(waiter.timer);
        waiter.reject(new Error(`helper exited before the expected frame; stderr:\n${this.stderrLines.join("\n")}`));
      }
    });
  }

  static async start(options: HelperClientOptions = {}): Promise<HelperClient> {
    const dataDir = await mkdtemp(join(tmpdir(), "warpi-helper-test-"));
    // Windows Node.js needs `SystemRoot` (and friends) at startup: without it
    // the OpenSSL/`ncrypto::CSPRNG` self-check aborts on Node 24. The Rust
    // supervisor passes the same variables (`apply_sandbox_env`), so the test
    // harness must too, not just on the machine that happened to have them set.
    const windowsEnv =
      process.platform === "win32"
        ? {
            SystemRoot: process.env.SystemRoot ?? "C:\\Windows",
            windir: process.env.windir ?? process.env.SystemRoot ?? "C:\\Windows",
            PATHEXT: process.env.PATHEXT ?? ".COM;.EXE;.BAT;.CMD",
            COMSPEC: process.env.COMSPEC ?? "C:\\Windows\\System32\\cmd.exe",
          }
        : {};
    const child = spawn(process.execPath, [HELPER_ENTRY], {
      cwd: options.cwd ?? dataDir,
      env: {
        ...windowsEnv,
        PATH: process.env.PATH ?? "/usr/bin:/bin",
        HOME: dataDir,
        WARPI_PI_SCRATCH_DIR: join(dataDir, "scratch"),
        ...options.env,
      },
      stdio: ["pipe", "pipe", "pipe"],
    });
    const client = new HelperClient(child, dataDir);
    return client;
  }

  send(kind: string, data: unknown, identity: Partial<HelperFrame> = {}): void {
    const frame = {
      protocol: 1,
      seq: this.seq++,
      kind,
      ...(identity.session_id !== undefined ? { session_id: identity.session_id } : {}),
      ...(identity.turn_id !== undefined ? { turn_id: identity.turn_id } : {}),
      ...(identity.exchange_id !== undefined ? { exchange_id: identity.exchange_id } : {}),
      ...(identity.generation !== undefined ? { generation: identity.generation } : {}),
      data,
    };
    this.child.stdin.write(`${JSON.stringify(frame)}\n`);
  }

  nextFrame(timeoutMs = 20_000): Promise<HelperFrame> {
    return this.waitFor(() => true, timeoutMs);
  }

  waitFor(predicate: (frame: HelperFrame) => boolean, timeoutMs = 20_000): Promise<HelperFrame> {
    const existing = this.frames.find(predicate);
    if (existing !== undefined) return Promise.resolve(existing);
    return new Promise<HelperFrame>((resolve, reject) => {
      const timer = setTimeout(() => {
        const index = this.waiters.findIndex((waiter) => waiter.timer === timer);
        if (index >= 0) this.waiters.splice(index, 1);
        reject(
          new Error(
            `timed out after ${timeoutMs}ms waiting for a frame; saw: ${this.frames
              .map((frame) => frame.kind)
              .join(", ")}\nstderr:\n${this.stderrLines.join("\n")}`,
          ),
        );
      }, timeoutMs);
      this.waiters.push({ predicate, resolve, reject, timer });
    });
  }

  /** All frames seen so far, in order. */
  get seen(): HelperFrame[] {
    return this.frames.slice();
  }

  async shutdown(): Promise<void> {
    if (!this.exited) {
      this.send("shutdown", {});
      await new Promise((resolve) => setTimeout(resolve, 200));
      this.child.kill("SIGKILL");
      await new Promise<void>((resolve) => {
        if (this.exited) return resolve();
        this.child.once("exit", () => resolve());
        setTimeout(resolve, 2000);
      });
    }
    // Close the readers so a leaked handle cannot keep the test process alive.
    this.outReader.close();
    this.errReader.close();
  }

  async dispose(): Promise<void> {
    await this.shutdown();
    // Windows can briefly hold the child's working directory open after exit;
    // retry the removal instead of failing the test with EBUSY.
    await rm(this.dataDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 50 });
  }
}

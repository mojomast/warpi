# Evidence index

Text logs and screenshots captured on the audit machine (Linux x86_64,
2026-09-23). Reproduce with the commands in each file's header and in
`../VALIDATION.md`.

| File | What it shows |
| --- | --- |
| `rust-tests.log` | `cargo test -p standalone_agent`: 22 tests, 0 failures (14 unit + 8 integration, including the M1 vertical slice and session isolation). |
| `helper-tests.log` | `node --import tsx --test test/*.test.ts` in `standalone/pi-helper`: 13 tests, 0 failures. |
| `gui-build.log` | `cargo build -p warp --bin warp-oss --features gui`: success in 4m10s, two non-behavioural warnings. |
| `gui-native-window.png` | The native Warp GUI running under Xvfb + Mesa lavapipe with the standalone config loaded (universal input, agent conversation hint). |
| `gui-typed-input.png` | Injected keyboard input reaching the native window (a shell command executed in a Warp block). |

Additional working-tree artifacts (not committed, they are bulky or noisy):

- `/home/mojo/projects/warp2/evidence/gui-shot-{1..5}.png` — onboarding flow.
- `/home/mojo/projects/warp2/evidence/gui-run/gui-verify-*.png` — final GUI run.
- `/home/mojo/projects/warp2/evidence/gui-run/captures.json` — fixture provider
  request captures (0 requests in the final run: the agent conversation was
  never started from the GUI; see `VALIDATION.md` risk 1).

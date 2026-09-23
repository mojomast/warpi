# Evidence index

Text logs and screenshots captured on the audit machine (Linux x86_64,
2026-09-23). Reproduce with the commands in each file's header and in
`../VALIDATION.md`.

| File | What it shows |
| --- | --- |
| `rust-tests.log` | `cargo test -p standalone_agent`: 24 tests, 0 failures (14 unit + 1 session isolation + 9 vertical slice) — recorded before the turn-lifecycle/queueing/`bash_output` suites. The current source has 56 test functions; those are not re-run yet (`VALIDATION.md`). |
| `helper-tests.log` | `node --import tsx --test test/*.test.ts` in `standalone/pi-helper`: 13 tests, 0 failures. The current source has 14 (`bash_output` and guardrail tests post-date the log). |
| `gui-build.log` | `cargo build -p warp --bin warpi --features gui`: success in 4m16s, 5 warnings. |
| `gui-native-window.png` | The native Warp GUI running under Xvfb + Mesa lavapipe with the standalone config loaded (universal input, agent conversation hint). |
| `gui-typed-input.png` | Injected keyboard input reaching the native window (a shell command executed in a Warp block). |
| `gui-provider-settings.png` | Settings → Agents → **Local Pi provider** with a configured DeepSeek profile. |
| `gui-test-connection.png` | **Test connection** against the configured endpoint (`GET /v1/models`). |
| `gui-tool-approval.png` | Native tool approval card for the agent's shell command. |
| `gui-round-trip-complete.png` | Completed agent round trip: command block, output, and the final assistant text. |
| `gui-model-picker-all-providers.png` | Native model picker listing every enabled model of every configured profile (`DeepSeek · deepseek-flash`, `DeepSeek · deepseek-v4-pro`, `Local fixture`). |
| `gui-model-toggle.png` | Settings page after `deepseek-v4-pro` was disabled: only `deepseek-flash` remains offered in the picker. |

Earlier GUI-run artifacts (onboarding screenshots, the model-switch sequence,
fixture provider request captures) are not committed because they are bulky or
noisy; the files above are the ones `VALIDATION.md` cites. The two
`gui-model-*` screenshots are copies of the more detailed picker captures from
that run.

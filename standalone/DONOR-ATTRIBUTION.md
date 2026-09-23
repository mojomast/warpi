# Donor attribution

Logic ported (rewritten, not copied verbatim) into this fork comes from the
OpenWarp adapter project:

- Repository: https://github.com/sasuke39/openwarp
- Commit: `5045d30a98de5432cedfd256e15e56675696d0b7` (2026-09-22)
- Component used: `integrations/pi-agent` (TypeScript) and `cmd/server` /
  `internal/agentruntime` (Go) — the NDJSON tool-brokering protocol and the
  Warp client-action event shapes.
- License: MIT (reproduced below, as required for reuse).

The donor's separate `openwarp-client` repository is AGPL-3.0 and was **not**
used. No donor source file is copied into this repository; `REUSE.md` lists the
behaviours that were reimplemented and every deliberate deviation.

```
MIT License

Copyright (c) 2026 sasuke39

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

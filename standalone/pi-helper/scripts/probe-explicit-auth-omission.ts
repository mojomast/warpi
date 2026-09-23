// Development probe: proves the OpenAI client omits Authorization when it is
// explicitly null, which is the evidence behind SECURITY.md's auth=none claim.
import { createServer } from "node:http";
import OpenAI from "../node_modules/@earendil-works/pi-coding-agent/node_modules/openai/index.mjs";

const seen: Record<string, unknown>[] = [];
const server = createServer(async (req, res) => {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  seen.push({ headers: { ...req.headers } });
  res.writeHead(200, { "content-type": "text/event-stream" });
  res.write(`data: ${JSON.stringify({ id: "x", object: "chat.completion.chunk", created: 0, model: "m", choices: [{ index: 0, delta: { content: "hi" }, finish_reason: null }] })}\n\n`);
  res.write(`data: ${JSON.stringify({ id: "x", object: "chat.completion.chunk", created: 0, model: "m", choices: [{ index: 0, delta: {}, finish_reason: "stop" }] })}\n\n`);
  res.write("data: [DONE]\n\n");
  res.end();
});
server.listen(0, "127.0.0.1");
await new Promise((r) => server.once("listening", r));
const port = (server.address() as { port: number }).port;

for (const [label, headers] of [
  ["placeholder-only", undefined],
  ["null-auth", { Authorization: null }],
  ["empty-auth", { Authorization: "" }],
] as const) {
  const client = new OpenAI({ apiKey: "unused-placeholder", baseURL: `http://127.0.0.1:${port}/v1`, defaultHeaders: headers as never });
  const stream = await client.chat.completions.create({ model: "m", messages: [{ role: "user", content: "hi" }], stream: true });
  for await (const _ of stream) { /* drain */ }
  console.log(label, JSON.stringify(seen[seen.length - 1]));
}
server.close();

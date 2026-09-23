// Development probe: shows that model/extension header values cannot carry
// nulls through the SDK config layer (why auth=none is enforced at the HTTP
// boundary instead).
import { createServer } from "node:http";
import { ModelRuntime } from "@earendil-works/pi-coding-agent";

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

for (const label of ["model-header-null", "extension-header-null"]) {
  const rt = await ModelRuntime.create({ modelsPath: null, refreshOnCreate: false, allowModelNetwork: false });
  rt.registerProvider("p", {
    name: "fixture", baseUrl: `http://127.0.0.1:${port}/v1`, api: "openai-completions", apiKey: "warposs-no-auth", authHeader: true,
    ...(label === "extension-header-null" ? { headers: { Authorization: null } as never } : {}),
    models: [{ id: "m", name: "m", api: "openai-completions", reasoning: false, input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 8192, maxTokens: 1024,
      ...(label === "model-header-null" ? { headers: { Authorization: null } as never } : {}) }],
  } as never);
  const model = rt.getModel("p", "m")!;
  const auth = await rt.getAuth(model);
  const stream = rt.streamSimple(model, { systemPrompt: "s", messages: [{ role: "user", content: [{ type: "text", text: "hi" }], timestamp: Date.now() }], tools: [] } as never, { apiKey: auth?.auth.apiKey } as never);
  try { for await (const ev of stream as AsyncIterable<unknown>) void ev; } catch (e) { console.log(label, "stream error:", String(e)); }
  console.log(label, JSON.stringify(seen[seen.length - 1]));
}
server.close();

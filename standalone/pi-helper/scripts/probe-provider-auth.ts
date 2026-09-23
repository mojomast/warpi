// Development probe: shows how the pinned SDK resolves credentials for a
// registered provider (used to design the auth=none path).
import { ModelRuntime } from "@earendil-works/pi-coding-agent";
const rt = await ModelRuntime.create({ modelsPath: null, refreshOnCreate: false, allowModelNetwork: false });
rt.registerProvider("p", {
  name: "fixture", baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", authHeader: false,
  models: [{ id: "m", name: "m", api: "openai-completions", reasoning: false, input: ["text"],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 8192, maxTokens: 1024 }],
} as never);
const model = rt.getModel("p", "m");
console.log("model:", model?.id, model?.provider);
console.log("compat:", JSON.stringify(rt.getCompatibilityRequestConfig(model!)));
console.log("hasConfiguredAuth:", rt.hasConfiguredAuth("p"));
console.log("authStatus:", JSON.stringify(rt.getProviderAuthStatus("p")));
try { console.log("getAuth:", JSON.stringify(await rt.getAuth(model!))); } catch (e) { console.log("getAuth threw:", String(e)); }

import Ajv2020 from "ajv/dist/2020.js";
import { describe, expect, it } from "vitest";
import adapterManifestJson from "../../schema/adapter-manifest.schema.json";
import componentCardJson from "../../schema/component-card.schema.json";
import loadPolicyJson from "../../schema/load-policy.schema.json";
import providerJson from "../../schema/provider.schema.json";
import routingPolicyJson from "../../schema/routing-policy.schema.json";
import taskProfileJson from "../../schema/task-profile.schema.json";
import trainingSampleJson from "../../schema/training-sample.schema.json";
import { adapterManifestSchema } from "../../src/adapter-manifest.js";
import { componentCardSchema } from "../../src/component-card.js";
import { loadPolicySchema } from "../../src/load-policy.js";
import { providerDescriptorSchema } from "../../src/provider.js";
import { routingPolicySchema } from "../../src/routing-policy.js";
import { taskProfileSchema } from "../../src/task-profile.js";
import { trainingSampleSchema } from "../../src/training-sample.js";

const ajv = new Ajv2020({ allErrors: true, strict: false });
const jsonByName = {
  "adapter-manifest": adapterManifestJson,
  provider: providerJson, "load-policy": loadPolicyJson, "component-card": componentCardJson,
  "routing-policy": routingPolicyJson, "task-profile": taskProfileJson, "training-sample": trainingSampleJson,
} as const;
type Name = keyof typeof jsonByName;
for (const schema of Object.values(jsonByName)) ajv.addSchema(schema as object);
const ajvOk = (n: Name, data: unknown) => ajv.validate(`https://idoris.ai/schema/${n}.schema.json`, data);
const zodByName: Record<Name, { safeParse: (d: unknown) => { success: boolean } }> = {
  "adapter-manifest": adapterManifestSchema,
  provider: providerDescriptorSchema, "load-policy": loadPolicySchema, "component-card": componentCardSchema,
  "routing-policy": routingPolicySchema, "task-profile": taskProfileSchema,
  "training-sample": trainingSampleSchema,
};

const validProvider = {
  id: "omlx-local", family: "local", tier: "local", capabilities: ["chat"],
  privacy_class: "local_only", cost: { input_per_m: 0, output_per_m: 0 }, locality: "loopback",
};
const validCard = {
  provider: validProvider, form: "http_service", endpoint: "http://127.0.0.1:8088/v1",
  version_pin: "omlx@0.6.4", privacy_class: "local_only", allowed_egress: ["loopback"],
  fallback_policy: "fail_closed", fail_closed: true,
  load_policy: { mode: "resident", keepalive: { pinned: true }, admission: "coexist" },
};

const digest = (c: string): string => "sha256:" + c.repeat(64);
const validManifest = {
  adapter_id: "lora-1", base_model_id: "Qwen3-4B-mlx", base_digest: digest("a"),
  tokenizer_digest: digest("b"), rank: 16, data_class: "synthetic",
};

const validSample = {
  sample_id: "s-1",
  data_class: "synthetic",
  source: { kind: "refined_from_synthetic" },
  messages: [{ role: "user", content: "hi" }, { role: "assistant", content: "hello" }],
};

const corpus: Array<[Name, unknown]> = [
  ["training-sample", validSample],
  ["training-sample", { ...validSample, data_class: "nope" }],
  ["training-sample", { ...validSample, messages: [{ role: "user", content: "hi" }] }],
  ["training-sample", { ...validSample, source: { kind: "from_the_internet" } }],
  ["training-sample", { ...validSample, sample_id: "" }],
  ["adapter-manifest", validManifest],
  ["adapter-manifest", { ...validManifest, adapter_id: "" }],
  ["adapter-manifest", { ...validManifest, base_digest: "sha256:abc" }],
  ["adapter-manifest", { ...validManifest, rank: 0 }],
  ["adapter-manifest", { ...validManifest, data_class: "nope" }],
  ["provider", validProvider],
  ["provider", { ...validProvider, capabilities: [] }],
  ["provider", { ...validProvider, tier: "cloud" }],
  ["load-policy", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" }],
  ["load-policy", { mode: "on_demand", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" }],
  ["load-policy", { mode: "resident", keepalive: {}, admission: "coexist" }],
  ["load-policy", { mode: "resident", keepalive: { pinned: true, idle_ttl_s: 5 }, admission: "coexist" }],
  ["load-policy", { mode: "sometimes", keepalive: { pinned: true }, admission: "coexist" }],
  ["component-card", validCard],
  ["component-card", { ...validCard, version_pin: "" }],
  ["component-card", { ...validCard, allowed_egress: ["internet", "typo"] }],
  ["routing-policy", { routing_policy: { version: 1, rules: [{ if: { privacy: "local_only" }, then: { tiers: ["local", "lora"], fail_closed: true } }], default: { tiers: ["local"], fail_closed: true } } }],
  ["routing-policy", { routing_policy: { version: 1, rules: [] } }],
  ["routing-policy", { routing_policy: { version: 1, rules: [], default: {} } }],
  ["routing-policy", { routing_policy: { version: 1, rules: [{ if: {}, then: {} }], default: { tiers: ["local"] } } }],
  ["routing-policy", { routing_policy: { version: 1, rules: [], default: { tiers: [] } } }],
  ["component-card", { ...validCard, extensions: { "provider.anthropic.prompt_cache": { ttl: 5 } } }],
  ["task-profile", {}],
  ["task-profile", { privacy: "nope" }],
  ["task-profile", { capabilities: [] }],
];

describe("JSON Schema <-> 生成的 zod 一致性（零漂移语义门）", () => {
  for (const [name, data] of corpus) {
    it(`${name}: ${JSON.stringify(data).slice(0, 60)}`, () => {
      expect(zodByName[name].safeParse(data).success).toBe(ajvOk(name, data));
    });
  }
  it("every schema is registered with Ajv", () => {
    for (const n of Object.keys(jsonByName) as Name[]) {
      expect(typeof ajv.getSchema(`https://idoris.ai/schema/${n}.schema.json`)).toBe("function");
    }
  });
});
import Ajv2020 from "ajv/dist/2020.js";
import { describe, expect, it } from "vitest";
import adminV0BackendsJson from "../../schema/admin-v0-backends.schema.json";
import adminV0ModelsJson from "../../schema/admin-v0-models.schema.json";
import adminV0RolesJson from "../../schema/admin-v0-roles.schema.json";
import adminV0RuntimesJson from "../../schema/admin-v0-runtimes.schema.json";
import adminV0StatusJson from "../../schema/admin-v0-status.schema.json";
import adminV0UpstreamJson from "../../schema/admin-v0-upstream.schema.json";
import adminV0UpstreamsJson from "../../schema/admin-v0-upstreams.schema.json";
import adapterManifestJson from "../../schema/adapter-manifest.schema.json";
import componentCardJson from "../../schema/component-card.schema.json";
import inspectRequestJson from "../../schema/inspect-request.schema.json";
import inspectResponseJson from "../../schema/inspect-response.schema.json";
import loadPolicyJson from "../../schema/load-policy.schema.json";
import providerJson from "../../schema/provider.schema.json";
import roleJson from "../../schema/role.schema.json";
import routingPolicyJson from "../../schema/routing-policy.schema.json";
import taskProfileJson from "../../schema/task-profile.schema.json";
import trainingSampleJson from "../../schema/training-sample.schema.json";
import { adminV0BackendsResponseSchema } from "../../src/generated/admin-v0-backends.js";
import { adminV0ModelsResponseSchema } from "../../src/generated/admin-v0-models.js";
import { adminV0RolesResponseSchema } from "../../src/generated/admin-v0-roles.js";
import { adminV0RuntimesResponseSchema } from "../../src/generated/admin-v0-runtimes.js";
import { adminV0StatusResponseSchema } from "../../src/generated/admin-v0-status.js";
import { adminV0UpstreamSchema } from "../../src/generated/admin-v0-upstream.js";
import { adminV0UpstreamsResponseSchema } from "../../src/generated/admin-v0-upstreams.js";
import { adapterManifestSchema } from "../../src/adapter-manifest.js";
import { componentCardSchema } from "../../src/component-card.js";
import { inspectRequestSchema } from "../../src/generated/inspect-request.js";
import { inspectResponseSchema } from "../../src/generated/inspect-response.js";
import {
  adminV0UpstreamSchema as publicAdminV0UpstreamSchema,
  adminV0UpstreamsResponseSchema as publicAdminV0UpstreamsResponseSchema,
  inspectResponseSchema as publicInspectResponseSchema,
} from "../../src/index.js";
import { loadPolicySchema } from "../../src/load-policy.js";
import { providerDescriptorSchema } from "../../src/provider.js";
import { roleSchema } from "../../src/role.js";
import { routingPolicySchema } from "../../src/routing-policy.js";
import { taskProfileSchema } from "../../src/task-profile.js";
import { trainingSampleSchema } from "../../src/training-sample.js";

const ajv = new Ajv2020({ allErrors: true, strict: false });
const jsonByName = {
  "admin-v0-backends": adminV0BackendsJson,
  "admin-v0-models": adminV0ModelsJson,
  "admin-v0-roles": adminV0RolesJson,
  "admin-v0-runtimes": adminV0RuntimesJson,
  "admin-v0-status": adminV0StatusJson,
  "admin-v0-upstream": adminV0UpstreamJson,
  "admin-v0-upstreams": adminV0UpstreamsJson,
  "adapter-manifest": adapterManifestJson,
  "inspect-request": inspectRequestJson,
  "inspect-response": inspectResponseJson,
  provider: providerJson, "load-policy": loadPolicyJson, "component-card": componentCardJson,
  "routing-policy": routingPolicyJson, "task-profile": taskProfileJson, "training-sample": trainingSampleJson,
  role: roleJson,
} as const;
type Name = keyof typeof jsonByName;
for (const schema of Object.values(jsonByName)) ajv.addSchema(schema as object);
const ajvOk = (n: Name, data: unknown) => ajv.validate(`https://idoris.ai/schema/${n}.schema.json`, data);
const zodByName: Record<Name, { safeParse: (d: unknown) => { success: boolean } }> = {
  "admin-v0-backends": adminV0BackendsResponseSchema,
  "admin-v0-models": adminV0ModelsResponseSchema,
  "admin-v0-roles": adminV0RolesResponseSchema,
  "admin-v0-runtimes": adminV0RuntimesResponseSchema,
  "admin-v0-status": adminV0StatusResponseSchema,
  "admin-v0-upstream": adminV0UpstreamSchema,
  "admin-v0-upstreams": adminV0UpstreamsResponseSchema,
  "adapter-manifest": adapterManifestSchema,
  "inspect-request": inspectRequestSchema,
  "inspect-response": inspectResponseSchema,
  provider: providerDescriptorSchema, "load-policy": loadPolicySchema, "component-card": componentCardSchema,
  "routing-policy": routingPolicySchema, "task-profile": taskProfileSchema,
  "training-sample": trainingSampleSchema,
  role: roleSchema,
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

const validInspect = {
  sensitivity: "S3",
  findings: [{ kind: "cn_resident_id", span: { start: 4, length: 18 } }, { kind: "input_limit_exceeded" }],
  action: "BLOCK",
};

// Admin `upstreams` (规范 §2.11.4、勘误七): the element is the `/health.upstreams[]` fields plus Admin-only read facts.
const managedUpstream = {
  id: "omlx", engine_kind: "omlx", state: "ready", managed: true, owned: true,
  port: 19731, pid: 51234, restart_required: false, failure_count: 0,
};
const residentUpstream = {
  id: "lmstudio", engine_kind: "openai_compat", state: "ready", managed: false, owned: false,
  port: 1234, restart_required: false,
};
const without = (item: Record<string, unknown>, ...keys: string[]): Record<string, unknown> =>
  Object.fromEntries(Object.entries(item).filter(([key]) => !keys.includes(key)));
function adminV0UpstreamCorpus(): Array<[Name, unknown]> {
  const entries: Array<[Name, unknown]> = [
    ["admin-v0-upstreams", { upstreams: [managedUpstream, residentUpstream] }],
    ["admin-v0-upstreams", { upstreams: [] }],
    ["admin-v0-upstreams", { upstreams: [{ ...without(managedUpstream, "pid"), state: "failed", reason: "crash_loop", owned: false, restart_required: true, failure_count: 6, last_failure_at_ms: 1763625330123 }] }],
    ["admin-v0-upstreams", {}],
    ["admin-v0-upstreams", { upstreams: {} }],
    ["admin-v0-upstreams", { upstreams: [managedUpstream], default: "omlx" }],
    ["admin-v0-upstreams", { upstreams: [without(managedUpstream, "restart_required")] }],
  ];
  const element: Array<unknown> = [
    managedUpstream,
    residentUpstream,
    { ...managedUpstream, engine_kind: "unknown" },
    { ...without(managedUpstream, "pid"), state: "stopped", owned: false, restart_required: true },
    { ...without(managedUpstream, "pid"), state: "absent", reason: "engine_not_found", owned: false, restart_required: true },
    { ...managedUpstream, state: "stopping" },
    { ...managedUpstream, state: "Ready" },
    { ...managedUpstream, reason: "nope" },
    { ...managedUpstream, reason: null },
    { ...managedUpstream, id: "" },
    { ...managedUpstream, engine_kind: "" },
    { ...managedUpstream, engine_kind: "OMLX" },
    { ...managedUpstream, engine_kind: "1omlx" },
    { ...managedUpstream, engine_kind: "a".repeat(33) },
    { ...managedUpstream, engine_kind: "a".repeat(32) },
    { ...managedUpstream, managed: "yes" },
    { ...managedUpstream, owned: 1 },
    without(managedUpstream, "restart_required"),
    without(managedUpstream, "id"),
    without(managedUpstream, "state"),
    { ...managedUpstream, port: 0 },
    { ...managedUpstream, port: 65535 },
    { ...managedUpstream, port: 65536 },
    { ...managedUpstream, port: 1.5 },
    { ...managedUpstream, pid: 0 },
    { ...managedUpstream, pid: -3 },
    { ...managedUpstream, pid: 4294967295 },
    { ...managedUpstream, pid: 4294967296 },
    { ...managedUpstream, failure_count: -1 },
    { ...managedUpstream, failure_count: 2.5 },
    { ...managedUpstream, failure_count: 4294967295 },
    { ...managedUpstream, failure_count: 4294967296 },
    { ...managedUpstream, last_failure_at_ms: -1 },
    { ...managedUpstream, last_failure_at_ms: 1763625330123 },
    { ...managedUpstream, last_failure_at_ms: 9007199254740991 },
    { ...managedUpstream, last_failure_at_ms: 9007199254740992 },
    // 响应里不会有任何 key、token、nonce：多出来的字段一律被拒（严格模式）。
    { ...managedUpstream, api_key: "secret" },
    { ...managedUpstream, instance_nonce: "secret" },
    { ...managedUpstream, endpoint: "http://127.0.0.1:19731" },
  ];
  return [...entries, ...element.map((item): [Name, unknown] => ["admin-v0-upstream", item])];
}

const corpus: Array<[Name, unknown]> = [
  ["inspect-request", { input: "身份证 110105194912310021" }],
  ["inspect-request", { input: "" }],
  ["inspect-request", {}],
  ["inspect-request", { input: null }],
  ["inspect-request", { input: "x", privacy: "any" }],
  ["inspect-response", validInspect],
  ["inspect-response", { sensitivity: "S0", findings: [], action: "ALLOW" }],
  ["inspect-response", { ...validInspect, sensitivity: "S4" }],
  ["inspect-response", { ...validInspect, action: "block" }],
  ["inspect-response", { ...validInspect, findings: [{ kind: "phone" }] }],
  ["inspect-response", { ...validInspect, findings: [{ kind: "cn_mobile", span: { start: 0, length: 0 } }] }],
  ["inspect-response", { ...validInspect, findings: [{ kind: "cn_mobile", span: { start: -1, length: 11 } }] }],
  ["inspect-response", { ...validInspect, findings: [{ kind: "cn_mobile", span: { start: 1.5, length: 11 } }] }],
  ["inspect-response", { ...validInspect, findings: [{ kind: "cn_mobile", span: null }] }],
  ["inspect-response", { ...validInspect, findings: [{ kind: "cn_mobile", span: { start: 0, end: 11 } }] }],
  ["inspect-response", { ...validInspect, findings: [{ kind: "api_key", match: "sk-live-1" }] }],
  ["inspect-response", { ...validInspect, reason: "matched 110105194912310021" }],
  ["admin-v0-backends", [{ provider_id: "omlx-local", locality: "loopback", form: "http_service", lifecycle_runtime_bound: true }]],
  ["admin-v0-backends", []],
  ["admin-v0-backends", [{ provider_id: "", locality: "loopback", form: "http_service", lifecycle_runtime_bound: false }]],
  ["admin-v0-backends", [{ provider_id: "omlx-local", locality: "loopback", form: "http_service", lifecycle_runtime_bound: true, endpoint: "http://127.0.0.1:8088/v1" }]],
  ["admin-v0-models", { sources: [{ provider_id: "omlx-local", source: "http_models_endpoint", state: "observed", models: ["Qwen3-8B"] }] }],
  ["admin-v0-models", { sources: [{ provider_id: "claude-subscription", source: "subscription_registration", state: "configured", models: ["claude-sonnet"] }] }],
  ["admin-v0-models", { sources: [{ provider_id: "omlx-local", source: "http_models_endpoint", state: "error", models: [], error: "unavailable" }] }],
  ["admin-v0-models", { sources: [{ provider_id: "omlx-local", source: "http_models_endpoint", state: "observed", models: ["Qwen3-8B"], error: "unavailable" }] }],
  ["admin-v0-models", { sources: [{ provider_id: "claude-subscription", source: "subscription_registration", state: "configured", models: [] }] }],
  ["admin-v0-models", { sources: [{ provider_id: "omlx-local", source: "http_models_endpoint", state: "error", models: ["should-not-leak"], error: "authentication_failed" }] }],
  ["admin-v0-models", { sources: [{ provider_id: "omlx-local", source: "subscription_registration", state: "observed", models: ["Qwen3-8B"] }] }],
  ["admin-v0-roles", [{ role: "daily", aliases: ["idoris/daily"], catalog_role: true }]],
  ["admin-v0-roles", []],
  ["admin-v0-roles", [{ role: "daily", aliases: [], catalog_role: true }]],
  ["admin-v0-roles", [{ role: "daily", aliases: ["idoris/daily"], catalog_role: true, model: "Qwen3-8B" }]],
  ["admin-v0-runtimes", { runtimes: [{ provider_id: "omlx-local", state: "observed", pressure: "soft", used_gb: 8.5, model_memory_max_gb: 32, loaded: ["Qwen3-8B"] }] }],
  ["admin-v0-runtimes", { runtimes: [] }],
  ["admin-v0-runtimes", { runtimes: [{ provider_id: "omlx-local", state: "error" }] }],
  ["admin-v0-runtimes", { runtimes: [{ provider_id: "omlx-local", state: "observed", used_gb: 8.5, model_memory_max_gb: 32, loaded: ["Qwen3-8B"] }] }],
  ["admin-v0-runtimes", { runtimes: [{ provider_id: "omlx-local", state: "observed", pressure: "soft", used_gb: -1, model_memory_max_gb: 32, loaded: ["Qwen3-8B"] }] }],
  ["admin-v0-runtimes", { runtimes: [{ provider_id: "omlx-local", state: "error", pressure: "ok" }] }],
  ["admin-v0-runtimes", { runtimes: [{ provider_id: "omlx-local", state: "observed", pressure: "soft", used_gb: 8.5, model_memory_max_gb: 32, loaded: ["Qwen3-8B"], endpoint: "http://127.0.0.1:8088" }] }],
  ["admin-v0-status", {
    status: "ok", service: "idoris", version: "0.2.0", contract_version: "1.0.1",
    instance_id: "instance-1", components: 2, runtimes: 1, subscriptions: 0,
    budget_configured: true, audit_configured: true,
    capacity: { state: "observed", entries: [{ id: "model-a", capability: "reasoning", resident: true, estimated_memory_gb: 12.5, ctx_limit: 131072, queue_depth: 1, admission_status: "ready" }] },
  }],
  ["admin-v0-status", {
    status: "ok", service: "idoris", version: "0.2.0", contract_version: "1.0.1",
    instance_id: "instance-1", components: 0, runtimes: 0, subscriptions: 0,
    budget_configured: false, audit_configured: false, capacity: { state: "error" },
  }],
  ...adminV0UpstreamCorpus(),
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
  ["role", "fast"],
  ["role", "daily"],
  ["role", "auto"],
  ["role", "core"],
  ["role", "temp"],
  ["role", "nope"],
  ["role", ""],
  ["role", null],
  ["role", 1],
  ["role", "FAST"],
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

const special = { kind: "input_limit_exceeded" };
const spannedSpecial = { ...special, span: { start: 0, length: 1 } };
const mobile = { kind: "cn_mobile", span: { start: 0, length: 11 } };
const inspectResponseCases = [
  ["special span with BLOCK", [spannedSpecial], "BLOCK", false],
  ["spanless special with ALLOW", [special], "ALLOW", false],
  ["spanless special with REDACT", [special], "REDACT", false],
  ["special span with ALLOW", [spannedSpecial], "ALLOW", false],
  ["special after ordinary finding with ALLOW", [mobile, special], "ALLOW", false],
  ["spanned special after ordinary finding with BLOCK", [mobile, spannedSpecial], "BLOCK", false],
  ["spanless special with BLOCK", [special], "BLOCK", true],
  ["mixed findings with BLOCK", [mobile, special], "BLOCK", true],
  ["ordinary finding with ALLOW", [mobile], "ALLOW", true],
  ["clean ALLOW", [], "ALLOW", true],
] as const;

describe("inspect response independent expected verdicts", () => {
  for (const [name, findings, action, expected] of inspectResponseCases) {
    const data = { sensitivity: "S0", findings, action };
    it(`${name}: raw schema`, () => {
      expect(ajvOk("inspect-response", data)).toBe(expected);
    });
    it(`${name}: generated Zod`, () => {
      expect(inspectResponseSchema.safeParse(data).success).toBe(expected);
    });
    it(`${name}: public Zod`, () => {
      expect(publicInspectResponseSchema.safeParse(data).success).toBe(expected);
    });
  }
});

// 独立的期望判定（不靠「两边一致」）：Admin `upstreams` 的元素没有任何 key、token、nonce 的位置，也不接受 `/health` 之外随手加的字段。
const adminUpstreamCases: Array<[string, unknown, boolean]> = [
  ["managed, owned, ready", managedUpstream, true],
  ["resident, not managed", residentUpstream, true],
  ["engine kinds are an open set", { ...managedUpstream, engine_kind: "lmstudio" }, true],
  ["a failed managed engine with a reason and its failure facts", { ...without(managedUpstream, "pid"), state: "failed", reason: "crash_loop", owned: false, restart_required: true, failure_count: 6, last_failure_at_ms: 1763625330123 }, true],
  ["restart_required is mandatory", without(managedUpstream, "restart_required"), false],
  ["a state outside the frozen seven", { ...managedUpstream, state: "stopping" }, false],
  ["a reason outside the frozen ten", { ...managedUpstream, reason: "disk_full" }, false],
  ["no secret-bearing field is allowed: api_key", { ...managedUpstream, api_key: "k" }, false],
  ["no secret-bearing field is allowed: token", { ...managedUpstream, token: "t" }, false],
  ["no secret-bearing field is allowed: instance_nonce", { ...managedUpstream, instance_nonce: "n" }, false],
  ["port 0 is not a port", { ...managedUpstream, port: 0 }, false],
  ["a pid is a positive integer", { ...managedUpstream, pid: 0 }, false],
  // The integer ranges are the Rust types' ranges (u32, and a timestamp pinned to 2^53 - 1): both sides agree on every edge.
  ["the largest u32 pid", { ...managedUpstream, pid: 4294967295 }, true],
  ["a pid beyond u32", { ...managedUpstream, pid: 4294967296 }, false],
  ["the largest u32 failure_count", { ...managedUpstream, failure_count: 4294967295 }, true],
  ["a failure_count beyond u32", { ...managedUpstream, failure_count: 4294967296 }, false],
  ["the largest safe-integer timestamp", { ...managedUpstream, last_failure_at_ms: 9007199254740991 }, true],
  ["a timestamp past the JS safe-integer range", { ...managedUpstream, last_failure_at_ms: 9007199254740992 }, false],
];

describe("admin upstream independent expected verdicts", () => {
  for (const [name, data, expected] of adminUpstreamCases) {
    it(`${name}: raw schema`, () => {
      expect(ajvOk("admin-v0-upstream", data)).toBe(expected);
    });
    it(`${name}: generated Zod`, () => {
      expect(adminV0UpstreamSchema.safeParse(data).success).toBe(expected);
    });
    it(`${name}: public Zod`, () => {
      expect(publicAdminV0UpstreamSchema.safeParse(data).success).toBe(expected);
    });
    it(`${name}: inside the collection`, () => {
      expect(publicAdminV0UpstreamsResponseSchema.safeParse({ upstreams: [data] }).success).toBe(expected);
    });
  }
});

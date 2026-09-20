import Ajv2020 from "ajv/dist/2020.js";
import { describe, expect, it } from "vitest";
import componentCardJson from "../../schema/component-card.schema.json";
import loadPolicyJson from "../../schema/load-policy.schema.json";
import providerJson from "../../schema/provider.schema.json";
import routingPolicyJson from "../../schema/routing-policy.schema.json";
import taskProfileJson from "../../schema/task-profile.schema.json";
import { componentCardSchema } from "../../src/component-card.js";
import { loadPolicySchema } from "../../src/load-policy.js";
import { providerDescriptorSchema } from "../../src/provider.js";
import { routingPolicySchema } from "../../src/routing-policy.js";
import { taskProfileSchema } from "../../src/task-profile.js";

const ajv = new Ajv2020({ allErrors: true, strict: false });
const jsonByName = {
  provider: providerJson, "load-policy": loadPolicyJson, "component-card": componentCardJson,
  "routing-policy": routingPolicyJson, "task-profile": taskProfileJson,
} as const;
type Name = keyof typeof jsonByName;
for (const schema of Object.values(jsonByName)) ajv.addSchema(schema as object);
const ajvOk = (n: Name, data: unknown) => ajv.validate(`https://idoris.ai/schema/${n}.schema.json`, data);
const zodByName: Record<Name, { safeParse: (d: unknown) => { success: boolean } }> = {
  provider: providerDescriptorSchema, "load-policy": loadPolicySchema, "component-card": componentCardSchema,
  "routing-policy": routingPolicySchema, "task-profile": taskProfileSchema,
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

const corpus: Array<[Name, unknown]> = [
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

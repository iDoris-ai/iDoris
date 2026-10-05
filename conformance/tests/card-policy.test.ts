import { readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { localComponent, makeComponentsDir, remoteComponent, type ComponentCardSpec } from "../src/fixtures.js";
import { ConformanceStartupError, routingPolicyFixturePath, spawnConformanceServer } from "../src/harness.js";

const endpoint = "http://127.0.0.1:12345";
const local = localComponent(endpoint);
const loadPolicy = (mode: string, keepalive: Record<string, unknown>) => ({ mode, keepalive, admission: "coexist" });
const demand = localComponent(endpoint, { loadPolicy: loadPolicy("on_demand", { pinned: false }) });
const evict = localComponent(endpoint, { loadPolicy: loadPolicy("evict_to_load", { idle_ttl_s: 7 }) });
const dirs: string[] = [];
afterEach(() => { for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true }); });

function components(card: ComponentCardSpec, change?: [string, string]): string {
  const dir = makeComponentsDir([card]);
  dirs.push(dir);
  if (change !== undefined) {
    const path = join(dir, readdirSync(dir)[0] as string);
    const text = readFileSync(path, "utf8");
    expect(text.split(change[0])).toHaveLength(2); // 精确变更一个字段，避免未命中/改到 provider。
    writeFileSync(path, text.replace(change[0], change[1]), "utf8");
  }
  return dir;
}

async function accepts(componentsDir: string): Promise<void> {
  const server = await spawnConformanceServer({ componentsDir, routingPolicyPath: routingPolicyFixturePath });
  try {
    expect(await (await fetch(server.baseUrl + "/health")).json()).toMatchObject({ components: 1 });
  } finally {
    await server.stop();
  }
}

const cases: { name: string; card: ComponentCardSpec; change: [string, string] }[] = [
  { name: "local_only 必须 fail_closed", card: local, change: ["fail_closed: true", "fail_closed: false"] },
  { name: "provider 隐私下限", card: local, change: ["\nprivacy_class: local_only", "\nprivacy_class: any"] },
  { name: "local tier 不能 remote locality", card: local, change: ["  locality: loopback", "  locality: remote"] },
  { name: "local tier 必须 load_policy", card: local, change: ["load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }\n", ""] },
  { name: "resident 不能 pinned=false", card: local, change: ["pinned: true", "pinned: false"] },
  { name: "resident 不能 idle TTL", card: local, change: ["keepalive: { pinned: true }", "keepalive: { idle_ttl_s: 7 }"] },
  { name: "on_demand 不能 pinned=true", card: demand, change: ['"pinned":false', '"pinned":true'] },
  { name: "evict_to_load 不能 pinned=true", card: evict, change: ['"keepalive":{"idle_ttl_s":7}', '"keepalive":{"pinned":true}'] },
  { name: "local none 必须独占", card: localComponent(endpoint, { allowedEgress: ["none"] }), change: ["[none]", "[none, loopback]"] },
  { name: "any none 必须独占", card: remoteComponent(endpoint, { allowedEgress: ["none"] }), change: ["[none]", "[none, internet]"] },
  { name: "重复 none 也拒绝", card: localComponent(endpoint, { allowedEgress: ["none"] }), change: ["[none]", "[none, none]"] },
  { name: "local_only 禁 internet", card: local, change: ["allowed_egress: [loopback]", "allowed_egress: [internet]"] },
  { name: "local_only 禁 lan", card: local, change: ["allowed_egress: [loopback]", "allowed_egress: [lan]"] },
];

it.each(cases)("$name：合法卡正控；只改一个字段即非零提前退出", async ({ card, change }) => {
  await accepts(components(card));
  const start = Date.now();
  const failure = await spawnConformanceServer({
    componentsDir: components(card, change), routingPolicyPath: routingPolicyFixturePath, healthTimeoutMs: 10_000,
  }).then(async (unexpected) => { await unexpected.stop(); return undefined; }, (err: unknown) => err);
  expect(failure).toBeInstanceOf(ConformanceStartupError);
  expect(failure).toMatchObject({ kind: "exited_early" });
  expect((failure as ConformanceStartupError).exitCode).not.toBeNull();
  expect((failure as ConformanceStartupError).exitCode).not.toBe(0);
  expect(Date.now() - start).toBeLessThan(2_000);
});

it.each([
  remoteComponent(endpoint, { loadPolicy: null }),
  remoteComponent(endpoint, { tier: "lora", loadPolicy: null }),
  localComponent(endpoint, { locality: "lan" }),
  localComponent(endpoint, { loadPolicy: loadPolicy("on_demand", { idle_ttl_s: 7 }) }),
  localComponent(endpoint, { loadPolicy: loadPolicy("evict_to_load", { pinned: false }) }),
  localComponent(endpoint, { fallbackPolicy: "next_in_chain" }),
  remoteComponent(endpoint, { allowedEgress: ["lan", "internet"] }),
])("额外合法卡：%j", async (card) => { await accepts(components(card)); });

it("provider=any 允许卡收紧到 local_only", async () => {
  await accepts(components(local, ["  privacy_class: local_only", "  privacy_class: any"]));
});

import { validateComponentCard, type ComponentCard } from "@idoris/contracts";
import { describe, expect, it } from "vitest";
import { effectiveServedLocality } from "../src/locality.js";

function card(overrides: {
  id: string;
  form: "http_service" | "spawn_cli";
  locality: string;
  tier?: "local" | "remote" | "lora";
  privacy_class?: "local_only" | "any";
}): ComponentCard {
  const tier = overrides.tier ?? "local";
  return validateComponentCard({
    provider: {
      id: overrides.id,
      family: "other",
      tier,
      capabilities: ["chat"],
      privacy_class: overrides.privacy_class ?? "any",
      cost: { input_per_m: 0, output_per_m: 0 },
      locality: overrides.locality,
    },
    form: overrides.form,
    endpoint: overrides.form === "spawn_cli" ? "spawn://fake" : "http://127.0.0.1:9999",
    version_pin: overrides.id + "@1",
    privacy_class: overrides.privacy_class ?? "any",
    allowed_egress: ["loopback"],
    fallback_policy: "fail_closed",
    fail_closed: true,
    ...(tier === "local" ? { load_policy: { mode: "resident", keepalive: { pinned: true }, admission: "coexist" } } : {}),
  }) as ComponentCard;
}

/**
 * H1（PR #46 复审）：`effectiveServedLocality` 是路由门禁/缓存/响应头三处
 * 共用的唯一判定，这里直接测它自己的行为，不经过 dispatch/proxy/server 三层。
 */
describe("effectiveServedLocality", () => {
  it("http_service 卡：locality 是三值之一时原样返回", () => {
    expect(effectiveServedLocality(card({ id: "a", form: "http_service", locality: "loopback" }))).toBe("loopback");
    // schema 层面 tier=local 不能配 locality=remote/lan（LOCAL_TIER_CANNOT_BE_REMOTE_LOCALITY），
    // 所以这两个用例得配 tier: remote 才能过校验——跟 effectiveServedLocality 本身无关。
    expect(effectiveServedLocality(card({ id: "b", form: "http_service", locality: "lan", tier: "remote" }))).toBe("lan");
    expect(effectiveServedLocality(card({ id: "c", form: "http_service", locality: "remote", tier: "remote" }))).toBe("remote");
  });

  it("form: spawn_cli 的卡一律判成 remote——不管它自己声明的 locality 是什么", () => {
    // schema 层面 locality: loopback 在这里是"合法但会说谎"的取值，
    // effectiveServedLocality 就是专门用来戳穿它的。
    expect(effectiveServedLocality(card({ id: "relay", form: "spawn_cli", locality: "loopback" }))).toBe("remote");
  });

  it("provider.id 命中 isSubscriptionProviderId 时一律判成 remote，即便 form 是 http_service", () => {
    // H1 真实复现路径：不需要 form 是 spawn_cli，isSubscriptionProviderId
    // 只认 id，"subscription" 这个 id 配 http_service 一样会被判成 remote。
    expect(effectiveServedLocality(card({ id: "subscription", form: "http_service", locality: "loopback" }))).toBe("remote");
  });

  it("负对照：普通 provider id 配 http_service + loopback 不会被误判成 remote", () => {
    expect(effectiveServedLocality(card({ id: "totally-ordinary", form: "http_service", locality: "loopback" }))).toBe("loopback");
  });
});

import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  UtteranceIntentDetector,
  cosine,
  defaultIntentDetector,
  hashingEmbedder,
  resolveProfile,
  type Embedder,
  type IntentDetector,
} from "../src/intent.js";
import { parseProfile } from "../src/profile.js";
import { startRouter, type Router } from "../src/server.js";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..", "..", "..");
const fixtures = join(here, "fixtures", "good");
const user = (content: string) => ({ messages: [{ role: "user", content }] });
const l2 = (v: readonly number[]): number => Math.sqrt(v.reduce((s, x) => s + x * x, 0));

describe("resolveProfile：显式声明优先于识别", () => {
  it("显式 X-iDoris-Intent 存在时识别器根本不被调用", async () => {
    const detect = vi.fn(async () => ({ intent: "web_search", score: 1 }));
    const detector: IntentDetector = { locality: "loopback", detect };
    const r = await resolveProfile({ "x-idoris-intent": "banner" }, user("帮我上网查一下"), detector, "personal");
    expect(r.profile.intent).toBe("banner");
    expect(r.intentSource).toBe("header");
    expect(detect).not.toHaveBeenCalled();
  });
  it("没有 detector 时回落静态默认 chat（D3=B）", async () => {
    const r = await resolveProfile({}, user("帮我上网查一下"), undefined, "personal");
    expect(r.profile.intent).toBe("chat");
    expect(r.intentSource).toBe("default");
  });
});

describe("privacy 永不被自动推断", () => {
  it("识别成功也不改 privacy：缺省仍是保守的 local_only", async () => {
    const detector: IntentDetector = { locality: "loopback", detect: async () => ({ intent: "web_search", score: 1 }) };
    const r = await resolveProfile({}, user("帮我上网查一下"), detector, "personal");
    expect(r.intentSource).toBe("detected");
    expect(r.profile.privacy).toBe("local_only");
  });
  it("显式 privacy=any 不被识别结果改写，且除 intent 外逐字段与 parseProfile 一致", async () => {
    const detector: IntentDetector = { locality: "loopback", detect: async () => ({ intent: "coding", score: 1 }) };
    const r = await resolveProfile({ "x-idoris-privacy": "any" }, user("写一个函数"), detector, "personal");
    const raw = parseProfile({ "x-idoris-privacy": "any" }, "personal").profile;
    expect(r.profile.privacy).toBe("any");
    expect(r.intentSource).toBe("detected");
    expect({ ...r.profile, intent: raw.intent }).toEqual(raw);
  });
  it("敌意识别器夹带 privacy/tenantId 也无效（接口没有那条通道）", async () => {
    const hostile = {
      locality: "loopback",
      detect: async () => ({ intent: "coding", score: 1, privacy: "any", tenantId: "evil" }),
    } as unknown as IntentDetector;
    const r = await resolveProfile({}, user("写一个函数"), hostile, "personal");
    expect(r.profile.privacy).toBe("local_only");
    expect(r.tenantId).toBeUndefined();
  });
  it("local_only + 远端嵌入端点：不识别（不给自动识别留外泄后门）", async () => {
    const detect = vi.fn(async () => ({ intent: "web_search", score: 1 }));
    const remote: IntentDetector = { locality: "remote", detect };
    const blocked = await resolveProfile({}, user("帮我上网查一下"), remote, "personal");
    expect(detect).not.toHaveBeenCalled();
    expect(blocked.intentSource).toBe("default");
    const allowed = await resolveProfile({ "x-idoris-privacy": "any" }, user("帮我上网查一下"), remote, "personal");
    expect(detect).toHaveBeenCalledTimes(1);
    expect(allowed.intentSource).toBe("detected");
  });
});

describe("兜底而非猜：识别不出/出故障/非法值都回落默认", () => {
  it("识别器自行认输（返回 undefined）时回落默认，不做二次阈值猜测", async () => {
    const detector: IntentDetector = { locality: "loopback", detect: async () => undefined };
    const r = await resolveProfile({}, user("随便说点什么"), detector, "personal");
    expect(r.intentSource).toBe("default");
    expect(r.profile.intent).toBe("chat");
  });
  it("识别器抛错不 500，回落默认", async () => {
    const detector: IntentDetector = {
      locality: "loopback",
      detect: async () => {
        throw new Error("embedding endpoint down");
      },
    };
    const r = await resolveProfile({}, user("写一个函数"), detector, "personal");
    expect(r.intentSource).toBe("default");
  });
  it("识别器给出非法 intent（空串）被拒绝，不把脏数据带进路由", async () => {
    const detector: IntentDetector = { locality: "loopback", detect: async () => ({ intent: "", score: 1 }) };
    const r = await resolveProfile({}, user("写一个函数"), detector, "personal");
    expect(r.intentSource).toBe("default");
    expect(r.profile.intent).toBe("chat");
  });
  it("空 prompt 不触发识别", async () => {
    const detect = vi.fn(async () => ({ intent: "coding", score: 1 }));
    const detector: IntentDetector = { locality: "loopback", detect };
    const r = await resolveProfile({}, { messages: [] }, detector, "personal");
    expect(detect).not.toHaveBeenCalled();
    expect(r.intentSource).toBe("default");
  });
});

describe("UtteranceIntentDetector 算法", () => {
  const vec = (t: string): number[] =>
    t === "QUERY" ? [1, 0] : t === "A" ? [0.9, 0.1] : t === "B" ? [0, 1] : [0, 0];
  const embed: Embedder = async (texts) => texts.map(vec);

  it("取每条 route 的最大相似度，选最高的 route", async () => {
    const d = new UtteranceIntentDetector({
      routes: [
        { intent: "alpha", utterances: ["B", "A"] },
        { intent: "beta", utterances: ["B", "C"] },
      ],
      embed,
      minScore: 0.5,
    });
    const hit = await d.detect(user("QUERY"));
    expect(hit?.intent).toBe("alpha");
    expect(hit?.score).toBeCloseTo(cosine([1, 0], [0.9, 0.1]), 10);
  });
  it("全部低于 minScore 时返回 undefined（不硬猜）", async () => {
    const d = new UtteranceIntentDetector({ routes: [{ intent: "alpha", utterances: ["B"] }], embed, minScore: 0.5 });
    expect(await d.detect(user("QUERY"))).toBeUndefined();
  });
  it("话术只编码一次（缓存）", async () => {
    const spy = vi.fn(embed);
    const d = new UtteranceIntentDetector({ routes: [{ intent: "alpha", utterances: ["A"] }], embed: spy, minScore: 0.5 });
    await d.detect(user("QUERY"));
    await d.detect(user("QUERY"));
    expect(spy).toHaveBeenCalledTimes(3);
  });
});

describe("hashingEmbedder（零依赖兜底）", () => {
  it("确定性且 L2 归一", async () => {
    const e = hashingEmbedder(64);
    const [a1] = await e(["帮我写一个函数"]);
    const [a2] = await e(["帮我写一个函数"]);
    expect(a1).toEqual(a2);
    expect(l2(a1 ?? [])).toBeCloseTo(1, 10);
    const [empty] = await e([""]);
    expect(l2(empty ?? [])).toBe(0);
  });
  it("内置路由能认出示例话术，空输入认输", async () => {
    const d = defaultIntentDetector();
    expect((await d.detect(user("写一个函数")))?.intent).toBe("coding");
    expect((await d.detect(user("帮我发一封邮件")))?.intent).toBe("email");
    expect(await d.detect({ messages: [] })).toBeUndefined();
  });
});

describe("startRouter 接线", () => {
  let running: Router | undefined;
  afterEach(async () => {
    if (running !== undefined) {
      await running.close();
      running = undefined;
    }
  });
  it("仅在缺 intent header 时调用注入的识别器", async () => {
    const seen: string[] = [];
    const detector: IntentDetector = {
      locality: "loopback",
      detect: async (ctx) => {
        seen.push(ctx.messages.map((m) => m.content).join("|"));
        return { intent: "coding", score: 1 };
      },
    };
    running = await startRouter({
      componentsDir: fixtures,
      routingPolicyPath: join(root, "config", "routing-policy.yaml"),
      intentDetector: detector,
    });
    const active = running;
    const post = (headers: Record<string, string>) =>
      fetch("http://127.0.0.1:" + active.port + "/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json", ...headers },
        body: JSON.stringify({ model: "mock-1", messages: [{ role: "user", content: "hi there" }] }),
      });
    await post({});
    expect(seen).toEqual(["hi there"]);
    await post({ "x-idoris-intent": "banner" });
    expect(seen).toEqual(["hi there"]);
  });
});

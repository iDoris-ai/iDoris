import { type DeployMode, taskProfileSchema, type TaskProfile } from "@idoris/contracts";
import { currentDeployMode, parseProfile, type HeaderBag } from "./profile.js";

/** 意图来源：显式 header / 自动识别 / 静态默认值（D3=B：缺省不猜，回落 chat）。 */
export type IntentSource = "header" | "detected" | "default";

/** 识别器的输入：只有请求体里已被过滤为 role/content 字符串的消息。 */
export interface IntentContext {
  messages: ReadonlyArray<{ role: string; content: string }>;
}

export interface IntentHit {
  intent: string;
  /** 0..1 的置信度；低于识别器阈值即视为「没识别出来」。 */
  score: number;
}

/**
 * 意图识别器（T2.4.1，docs/10 首选方案 semantic-router 的形状）。
 *
 * - **只能返回 intent**：接口里没有 privacy 通道，隐私字段在结构上无法被识别结果改写。
 * - 返回 undefined = 识别不出；调用方回落静态默认值，绝不猜。
 * - locality 声明嵌入端点位置：remote 的识别器不会为 local_only 请求触发，
 *   否则「自动识别」会变成一条把 prompt 送出本机的后门。
 */
export interface IntentDetector {
  readonly locality: "loopback" | "remote";
  detect(ctx: IntentContext): Promise<IntentHit | undefined>;
}

/** 文本 → 向量。生产接 iDoris/oMLX 的 /v1/embeddings，测试注入假实现。 */
export type Embedder = (texts: readonly string[]) => Promise<number[][]>;

export interface UtteranceRoute {
  intent: string;
  /** 该意图的示例话术集（semantic-router 的 route=examples）。 */
  utterances: readonly string[];
}

/** 余弦相似度；长度不等时取较短一侧，零向量记 0 分。 */
export function cosine(a: readonly number[], b: readonly number[]): number {
  const n = Math.min(a.length, b.length);
  let dot = 0;
  let na = 0;
  let nb = 0;
  for (let i = 0; i < n; i += 1) {
    const x = a[i] ?? 0;
    const y = b[i] ?? 0;
    dot += x * y;
    na += x * x;
    nb += y * y;
  }
  if (na === 0 || nb === 0) return 0;
  return dot / (Math.sqrt(na) * Math.sqrt(nb));
}

export interface UtteranceDetectorOptions {
  routes: readonly UtteranceRoute[];
  embed: Embedder;
  locality?: "loopback" | "remote";
  /** 低于该相似度就认输，防止「最像的一条」在毫不相干时也胜出。 */
  minScore?: number;
}

/**
 * 示例话术 + 嵌入余弦的意图识别（aurelio-labs/semantic-router 的核心算法）。
 * 路由话术只编码一次并缓存；每条 route 的得分取其实例话术的最大值。
 */
export class UtteranceIntentDetector implements IntentDetector {
  readonly locality: "loopback" | "remote";
  private readonly routes: readonly UtteranceRoute[];
  private readonly embed: Embedder;
  private readonly minScore: number;
  private encoded: Promise<Array<{ intent: string; vectors: number[][] }>> | undefined;

  constructor(opts: UtteranceDetectorOptions) {
    this.routes = opts.routes;
    this.embed = opts.embed;
    this.locality = opts.locality ?? "loopback";
    this.minScore = opts.minScore ?? 0.6;
  }

  private encode(): Promise<Array<{ intent: string; vectors: number[][] }>> {
    this.encoded ??= (async () => {
      const flat: string[] = [];
      for (const route of this.routes) for (const u of route.utterances) flat.push(u);
      const vectors = await this.embed(flat);
      if (vectors.length !== flat.length) {
        throw new Error("embedder returned " + vectors.length + " vectors for " + flat.length + " texts");
      }
      const out: Array<{ intent: string; vectors: number[][] }> = [];
      let i = 0;
      for (const route of this.routes) {
        const own: number[][] = [];
        for (let k = 0; k < route.utterances.length; k += 1) {
          const v = vectors[i];
          i += 1;
          if (v !== undefined) own.push(v);
        }
        out.push({ intent: route.intent, vectors: own });
      }
      return out;
    })();
    return this.encoded;
  }

  async detect(ctx: IntentContext): Promise<IntentHit | undefined> {
    const query = queryText(ctx);
    if (query === "") return undefined;
    const [queryVector] = await this.embed([query]);
    if (queryVector === undefined) return undefined;
    const routes = await this.encode();
    let best: IntentHit | undefined;
    for (const route of routes) {
      for (const vector of route.vectors) {
        const score = cosine(queryVector, vector);
        if (best === undefined || score > best.score) best = { intent: route.intent, score };
      }
    }
    if (best === undefined || best.score < this.minScore) return undefined;
    return best;
  }
}

/** 取最后一条非空 user 消息；没有 user 消息就取最后一条。 */
export function queryText(ctx: IntentContext): string {
  for (let i = ctx.messages.length - 1; i >= 0; i -= 1) {
    const m = ctx.messages[i];
    if (m?.role === "user" && m.content.trim() !== "") return m.content;
  }
  const last = ctx.messages[ctx.messages.length - 1];
  return last === undefined ? "" : last.content.trim();
}

function fnv1a(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i += 1) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

function bigrams(token: string): string[] {
  const out: string[] = [];
  for (let i = 0; i + 1 < token.length; i += 1) out.push(token.slice(i, i + 2));
  return out;
}

/**
 * 零依赖的确定性兜底嵌入：词 + 字符 bigram 的 signed hashing，L2 归一。
 * 它能离线工作（无嵌入端点时也有兜底），但只是兜底 —— 生产要换成
 * iDoris/oMLX 的 /v1/embeddings（oMLX 已暴露该端点）。
 */
export function hashingEmbedder(dim = 256): Embedder {
  return async (texts) =>
    texts.map((text) => {
      const v = new Float64Array(dim);
      const normalized = text.toLowerCase().replace(/\s+/g, " ").trim();
      for (const token of normalized.split(" ").filter((t) => t !== "")) {
        for (const feature of [token, ...bigrams(token)]) {
          const h = fnv1a(feature);
          const idx = h % dim;
          v[idx] = (v[idx] ?? 0) + ((h & 0x80000000) !== 0 ? -1 : 1);
        }
      }
      let norm = 0;
      for (let i = 0; i < dim; i += 1) {
        const x = v[i] ?? 0;
        norm += x * x;
      }
      norm = Math.sqrt(norm);
      const out: number[] = new Array<number>(dim);
      for (let i = 0; i < dim; i += 1) out[i] = norm === 0 ? 0 : (v[i] ?? 0) / norm;
      return out;
    });
}

export interface ResolvedProfile {
  profile: TaskProfile;
  tenantId?: string;
  intentSource: IntentSource;
}

/**
 * 控制面 header → TaskProfile，并在未显式声明 intent 时跑一次兜底识别（T2.4.1）。
 *
 * 三条不可协商的次序：
 * 1. **显式 header 永远优先** —— 此时识别器根本不会被调用；
 * 2. **隐私绝不推断** —— 识别结果只被允许替换 intent，privacy/tenant/capabilities
 *    全部来自已校验的 header 结果；识别器返回非法值即回落默认，不 500；
 * 3. **local_only + 远端嵌入端点不识别** —— 不给「自动识别」留一条外泄后门。
 */
export async function resolveProfile(
  headers: HeaderBag,
  context: IntentContext,
  detector?: IntentDetector,
  deployMode: DeployMode = currentDeployMode(),
): Promise<ResolvedProfile> {
  const parsed = parseProfile(headers, deployMode);
  const base: ResolvedProfile = {
    profile: parsed.profile,
    intentSource: parsed.intentSource,
    ...(parsed.tenantId === undefined ? {} : { tenantId: parsed.tenantId }),
  };
  if (parsed.intentSource === "header" || detector === undefined) return base;
  if (parsed.profile.privacy === "local_only" && detector.locality === "remote") return base;
  // 没有任何文本可识别：连识别器都不必调用（省掉一次无意义的嵌入请求/出网机会）。
  if (queryText(context) === "") return base;
  let hit: IntentHit | undefined;
  try {
    hit = await detector.detect(context);
  } catch {
    return base;
  }
  if (hit === undefined) return base;
  const candidate = taskProfileSchema.safeParse({ ...parsed.profile, intent: hit.intent });
  if (!candidate.success) return base;
  return { ...base, profile: candidate.data, intentSource: "detected" };
}

/** 内置兜底路由：docs/10 的「路径/意图」维度，刻意不含任何 policy 规则键上的 intent。 */
export const DEFAULT_INTENT_ROUTES: readonly UtteranceRoute[] = [
  {
    intent: "web_search",
    utterances: ["搜索一下最新的资料", "帮我上网查一下", "search the web for this", "查一下网上的说法"],
  },
  {
    intent: "coding",
    utterances: ["写一个函数", "帮我实现这个功能", "fix this bug", "重构这段代码", "implement this in Rust"],
  },
  {
    intent: "email",
    utterances: ["帮我发一封邮件", "send an email to the team", "回复这封邮件", "draft a reply to this message"],
  },
  {
    intent: "agent_task",
    utterances: ["帮我规划并执行这个任务", "plan and execute this step by step", "一步一步帮我做完这件事"],
  },
];

let shared: IntentDetector | undefined;

/** 进程内共享的默认识别器（只有本地哈希兜底嵌入，不产生任何出网）。 */
export function defaultIntentDetector(): IntentDetector {
  shared ??= new UtteranceIntentDetector({
    routes: DEFAULT_INTENT_ROUTES,
    embed: hashingEmbedder(),
    locality: "loopback",
    minScore: 0.65,
  });
  return shared;
}

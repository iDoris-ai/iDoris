import type { UsageEvent } from "./datalake.js";

export interface SyntheticSeed {
  intent: string;
  prompt: string;
  response: string;
}

/**
 * 合成语料种子（T3.1.1）。**全部为合成**：F3.1/F3.3 在 F3.4 隐私层就位前只允许
 * 用合成/脱敏数据（03 §5 硬门禁）。
 */
export const SYNTHETIC_SEEDS: readonly SyntheticSeed[] = [
  { intent: "coding", prompt: "用 Rust 写一个把秒数格式化成 hh:mm:ss 的函数", response: "把输入取模拆成时分秒，再按两位零填充拼接。" },
  { intent: "reasoning", prompt: "一个水池有两个进水管，单开甲 6 小时注满、乙 4 小时注满，同开几小时注满？", response: "合并速率 1/6+1/4=5/12，故需 12/5=2.4 小时。" },
  { intent: "chat", prompt: "用一句话解释模型量化是什么", response: "量化是把权重从高精度浮点压到更低位宽，用少量精度换内存与速度。" },
  { intent: "web_search", prompt: "帮我查一下 oMLX 的最新版本", response: "这需要先做一次联网检索再回答。" },
];

export interface SynthesizeOptions {
  repeats?: number;
  /** 事件时间戳基线。给定后产物完全确定（便于测试与复现）。 */
  now?: number;
}

/**
 * 「使用」段：把合成种子展开成确定性的使用痕迹。
 * 最后一个 repeat 的第 0 条刻意标为 rejected —— 用来证明下游提炼真的在过滤而不是全收。
 */
export function synthesizeUsage(
  seeds: readonly SyntheticSeed[] = SYNTHETIC_SEEDS,
  opts: SynthesizeOptions = {},
): UsageEvent[] {
  const repeats = opts.repeats ?? 2;
  const now = opts.now ?? 0;
  const out: UsageEvent[] = [];
  for (let r = 0; r < repeats; r += 1) {
    for (let i = 0; i < seeds.length; i += 1) {
      const seed = seeds[i];
      if (seed === undefined) continue;
      out.push({
        event_id: "syn-" + r + "-" + i,
        ts_utc: now + r * 1000 + i,
        intent: seed.intent,
        privacy: "local_only",
        data_class: "synthetic",
        outcome: r === repeats - 1 && i === 0 ? "rejected" : "accepted",
        prompt: seed.prompt,
        response: seed.response,
      });
    }
  }
  return out;
}

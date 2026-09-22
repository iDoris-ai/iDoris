import { trainingSampleSchema, type TrainingSample } from "@idoris/contracts";
import type { UsageEvent } from "./datalake.js";

export type RefineErrorCode = "SAMPLE_SCHEMA_INVALID" | "REAL_DATA_NOT_REFINABLE";

export class RefineError extends Error {
  constructor(
    readonly code: RefineErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "RefineError";
  }
}

export interface RefineResult {
  samples: TrainingSample[];
  /** 被过滤掉的事件数（未接受 / 缺 prompt / 缺 response）。 */
  skipped: number;
}

/**
 * 「提炼」段：使用痕迹 → 训练样本（T3.1.1）。
 *
 * - 只提炼 outcome=accepted 且 prompt/response 齐全的事件，其余计数跳过；
 * - `data_class=real` 一律**报错**而不是跳过（F3.4 硬门禁在提炼侧的落点）；
 * - 每条产物都过 `trainingSampleSchema`，schema 不合法即报错，不把脏样本交给训练。
 */
export function refineToTrainingSamples(events: readonly UsageEvent[]): RefineResult {
  const samples: TrainingSample[] = [];
  let skipped = 0;
  for (const event of events) {
    if (event.data_class === "real") {
      throw new RefineError(
        "REAL_DATA_NOT_REFINABLE",
        "usage event " + event.event_id + " is marked data_class=real; refusing to refine real data before the F3.4 privacy layer is enabled",
      );
    }
    if (event.outcome !== "accepted" || event.prompt === undefined || event.response === undefined) {
      skipped += 1;
      continue;
    }
    const candidate = {
      sample_id: "s-" + event.event_id,
      group_id: event.intent,
      data_class: event.data_class,
      source: {
        kind: event.data_class === "synthetic" ? "refined_from_synthetic" : "human_reviewed",
        generator: "idoris-growth/synthesize",
        intent: event.intent,
      },
      messages: [
        { role: "user", content: event.prompt },
        { role: "assistant", content: event.response },
      ],
    };
    const parsed = trainingSampleSchema.safeParse(candidate);
    if (!parsed.success) {
      throw new RefineError(
        "SAMPLE_SCHEMA_INVALID",
        "refined sample for event " + event.event_id + " is not schema-valid: " +
          parsed.error.issues.map((i) => i.path.join(".") + ": " + i.message).join("; "),
      );
    }
    samples.push(parsed.data);
  }
  return { samples, skipped };
}

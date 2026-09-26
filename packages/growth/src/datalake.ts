import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

/** 数据来源分级。真实数据在隐私层（F3.4）就位前不得进入任何管线。 */
export type DataClass = "synthetic" | "anonymized" | "real";

export type UsageOutcome = "accepted" | "rejected" | "aborted";

/** 一次「使用」的痕迹：由路由/执行侧产生，数据湖侧消费。 */
export interface UsageEvent {
  event_id: string;
  ts_utc: number;
  intent: string;
  privacy: "local_only" | "any";
  data_class: DataClass;
  outcome: UsageOutcome;
  prompt?: string;
  response?: string;
}

export type DataLakeErrorCode = "INVALID_EVENT" | "REAL_DATA_NOT_ADMISSIBLE";

export class DataLakeError extends Error {
  constructor(
    readonly code: DataLakeErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "DataLakeError";
  }
}

export interface DataLakeOptions {
  /** 真实数据准入门（F3.4.2）。隐私层未就位时必须保持 false。 */
  privacyLayerEnabled?: boolean;
}

/**
 * 真实数据准入判定（F3.4 硬门禁的可执行形式）。
 * 隐私层未启用时 `data_class=real` 一律**报错**——不是静默丢弃：静默丢弃会让人
 * 以为「没进管线」，而实际上代码路径已经把它读进来了。
 */
export function assertEventAdmissible(event: UsageEvent, privacyLayerEnabled: boolean): void {
  if (typeof event.event_id !== "string" || event.event_id === "") {
    throw new DataLakeError("INVALID_EVENT", "usage event must carry a non-empty event_id");
  }
  if (event.data_class !== "synthetic" && event.data_class !== "anonymized" && event.data_class !== "real") {
    throw new DataLakeError("INVALID_EVENT", "usage event must declare data_class=synthetic|anonymized|real");
  }
  if (event.data_class === "real" && !privacyLayerEnabled) {
    throw new DataLakeError(
      "REAL_DATA_NOT_ADMISSIBLE",
      "data_class=real cannot enter the local data lake before the F3.4 privacy layer is enabled; refused, not skipped",
    );
  }
}

/**
 * 本地数据湖（T3.1.1）：append-only JSONL，落在本机目录。
 * 准入在写之前跑，失败时湖里不留任何痕迹。
 */
export class DataLake {
  private readonly dir: string;
  private readonly file: string;
  private readonly privacyLayerEnabled: boolean;
  private cache: UsageEvent[] | undefined;

  constructor(dir: string, opts: DataLakeOptions = {}) {
    this.dir = dir;
    this.file = join(dir, "events.jsonl");
    this.privacyLayerEnabled = opts.privacyLayerEnabled ?? false;
  }

  append(event: UsageEvent): void {
    assertEventAdmissible(event, this.privacyLayerEnabled);
    mkdirSync(this.dir, { recursive: true });
    appendFileSync(this.file, JSON.stringify(event) + "\n", "utf8");
    this.cache = undefined;
  }

  size(): number {
    return this.load().length;
  }

  all(): readonly UsageEvent[] {
    return this.load();
  }

  byIntent(intent: string): UsageEvent[] {
    return this.load().filter((e) => e.intent === intent);
  }

  private load(): UsageEvent[] {
    if (this.cache !== undefined) return this.cache;
    if (!existsSync(this.file)) {
      this.cache = [];
      return this.cache;
    }
    const out: UsageEvent[] = [];
    for (const line of readFileSync(this.file, "utf8").split("\n")) {
      if (line.trim() === "") continue;
      out.push(JSON.parse(line) as UsageEvent);
    }
    this.cache = out;
    return out;
  }
}

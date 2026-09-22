/**
 * T2.1.2 / T2.1.3 — HardwareAwareModelRecommender。
 *
 * 输入：硬件 facts + 模型目录 + 策略；输出：常驻 / 临时组合 + 警告 + sysctl 建议 +
 * 可读 tradeoff（docs/07 §5.3 伪代码，门槛按 docs/13 §9.2 修正）。
 *
 * 关键规则：
 *  - `min_ram_gb` 是**硬门槛**：ram_gb < min_ram_gb 的组合一律 BLOCKED（docs/13 §9.2 ①）。
 *  - 常驻 = `capability.reasoning × quant.quality` 最高、且 footprint 放得下常驻预算者。
 *  - 临时按需求能力逐个 admission；放不下并存的标 `requires_eviction`。
 *  - `IDORIS_CORE_MODEL` 强制时推荐模块**让路**（yields），但仍输出警告。
 */

import { readFileSync } from "node:fs";
import { parse } from "yaml";
import type { TaskProfile } from "@idoris/contracts";
import {
  appleReserveGb,
  appleUsableGb,
  kvCacheGb,
  recommendedWiredLimitMb,
  weightsGb,
  type KvQuant,
  type ModelArch,
  type WiredMode,
} from "./memory.js";
import type { HostFacts } from "./probe.js";

export type Capability = TaskProfile["capabilities"][number];
export type AdmissionStatus = "ready" | "requires_eviction" | "BLOCKED";

export interface CatalogQuant {
  label: string;
  bpp?: number;
  weights_gb?: number;
  quality: number;
}

export interface CatalogModel {
  id: string;
  family?: string;
  params_total_b: number;
  params_active_b?: number;
  arch: ModelArch;
  modality?: string[];
  roles?: string[];
  capability?: Partial<Record<Capability, number>>;
  quant_options: CatalogQuant[];
  license?: string;
  min_ram_gb: number;
  status?: string;
  note?: string;
  scenarios?: Record<string, number>;
}

export interface CatalogExcluded {
  id: string;
  reason: string;
}

export interface Catalog {
  version: number;
  catalog: CatalogModel[];
  excluded?: CatalogExcluded[];
}

export class CatalogError extends Error {
  readonly path: string;
  constructor(message: string, path: string) {
    super(`${path}: ${message}`);
    this.name = "CatalogError";
    this.path = path;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function reqString(value: unknown, path: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new CatalogError("必须是非空字符串", path);
  }
  return value;
}

function reqNumber(value: unknown, path: string): number {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new CatalogError("必须是有限数字", path);
  }
  return value;
}

function optNumber(value: unknown, path: string): number | undefined {
  if (value === undefined || value === null) return undefined;
  return reqNumber(value, path);
}

function parseArch(value: unknown, path: string): ModelArch {
  if (!isRecord(value)) throw new CatalogError("arch 必须是对象", path);
  return {
    n_layers: reqNumber(value.n_layers, `${path}.n_layers`),
    n_kv_heads: reqNumber(value.n_kv_heads, `${path}.n_kv_heads`),
    head_dim: reqNumber(value.head_dim, `${path}.head_dim`),
  };
}

function parseQuant(value: unknown, path: string): CatalogQuant {
  if (!isRecord(value)) throw new CatalogError("quant option 必须是对象", path);
  const label = reqString(value.label, `${path}.label`);
  const quality = reqNumber(value.quality, `${path}.quality`);
  const bpp = optNumber(value.bpp, `${path}.bpp`);
  const weightsGbValue = optNumber(value.weights_gb, `${path}.weights_gb`);
  if (bpp === undefined && weightsGbValue === undefined) {
    throw new CatalogError("quant option 必须有 bpp 或 weights_gb", path);
  }
  return {
    label,
    quality,
    ...(bpp === undefined ? {} : { bpp }),
    ...(weightsGbValue === undefined ? {} : { weights_gb: weightsGbValue }),
  };
}

function parseCapability(value: unknown, path: string): Partial<Record<Capability, number>> | undefined {
  if (value === undefined) return undefined;
  if (!isRecord(value)) throw new CatalogError("capability 必须是对象", path);
  const out: Partial<Record<Capability, number>> = {};
  for (const [key, raw] of Object.entries(value)) {
    out[key as Capability] = reqNumber(raw, `${path}.${key}`);
  }
  return out;
}

function parseModel(value: unknown, path: string): CatalogModel {
  if (!isRecord(value)) throw new CatalogError("catalog 条目必须是对象", path);
  const id = reqString(value.id, `${path}.id`);
  const quantRaw = value.quant_options;
  if (!Array.isArray(quantRaw) || quantRaw.length === 0) {
    throw new CatalogError("quant_options 不能为空", `${path}.quant_options`);
  }
  const rolesRaw = value.roles;
  if (rolesRaw !== undefined && !Array.isArray(rolesRaw)) {
    throw new CatalogError("roles 必须是数组", `${path}.roles`);
  }
  return {
    id,
    params_total_b: reqNumber(value.params_total_b, `${path}.params_total_b`),
    arch: parseArch(value.arch, `${path}.arch`),
    quant_options: quantRaw.map((q, i) => parseQuant(q, `${path}.quant_options[${i}]`)),
    min_ram_gb: reqNumber(value.min_ram_gb, `${path}.min_ram_gb`),
    ...(typeof value.family === "string" ? { family: value.family } : {}),
    ...(optNumber(value.params_active_b, `${path}.params_active_b`) === undefined
      ? {}
      : { params_active_b: optNumber(value.params_active_b, `${path}.params_active_b`) as number }),
    ...(rolesRaw === undefined ? {} : { roles: rolesRaw.map((r, i) => reqString(r, `${path}.roles[${i}]`)) }),
    ...(value.modality === undefined
      ? {}
      : { modality: (value.modality as unknown[]).map((m, i) => reqString(m, `${path}.modality[${i}]`)) }),
    ...(parseCapability(value.capability, `${path}.capability`) === undefined
      ? {}
      : { capability: parseCapability(value.capability, `${path}.capability`) as Partial<Record<Capability, number>> }),
    ...(typeof value.license === "string" ? { license: value.license } : {}),
    ...(typeof value.status === "string" ? { status: value.status } : {}),
    ...(typeof value.note === "string" ? { note: value.note } : {}),
  };
}

export function parseCatalog(raw: unknown): Catalog {
  if (!isRecord(raw)) throw new CatalogError("根节点必须是对象", "$");
  const version = reqNumber(raw.version, "$.version");
  const models = raw.catalog;
  if (!Array.isArray(models)) throw new CatalogError("catalog 必须是数组", "$.catalog");
  const parsed = models.map((m, i) => parseModel(m, `$.catalog[${i}]`));
  const seen = new Set<string>();
  for (const m of parsed) {
    if (seen.has(m.id)) throw new CatalogError(`重复的模型 id: ${m.id}`, "$.catalog");
    seen.add(m.id);
  }
  const excluded = Array.isArray(raw.excluded)
    ? raw.excluded.map((e, i): CatalogExcluded => {
        if (!isRecord(e)) throw new CatalogError("excluded 条目必须是对象", `$.excluded[${i}]`);
        return { id: reqString(e.id, `$.excluded[${i}].id`), reason: reqString(e.reason, `$.excluded[${i}].reason`) };
      })
    : undefined;
  return { version, catalog: parsed, ...(excluded === undefined ? {} : { excluded }) };
}

export function loadCatalog(path: string): Catalog {
  return parseCatalog(parse(readFileSync(path, "utf8")) as unknown);
}

// ---------------------------------------------------------------------------
// 策略
// ---------------------------------------------------------------------------

export interface RecommenderPolicy {
  wired_mode: WiredMode;
  context_target: number;
  kv_quant: KvQuant;
  temp_slots: number;
  quality_threshold: number;
  needed_capabilities: Capability[];
}

/** 一个共存临时槽的预算预留（docs/07 §4.3：VL-3B ~2GB + ASR ~1GB + TTS ~0.4GB）。 */
export const TEMP_SLOT_RESERVE_GB = 3.5;
/** 常驻之外的 headroom，避免踩到 wired 上限。 */
export const HEADROOM_GB = 1.0;

export const DEFAULT_POLICY: RecommenderPolicy = {
  wired_mode: "conservative",
  context_target: 32768,
  kv_quant: "q8",
  temp_slots: 1,
  quality_threshold: 0.98,
  needed_capabilities: ["vision", "coding"],
};

// ---------------------------------------------------------------------------
// 结果
// ---------------------------------------------------------------------------

export interface QuantPick {
  label: string;
  quality: number;
  weights_gb: number;
  kv_gb: number;
  footprint_gb: number;
}

export interface ResidentChoice extends QuantPick {
  id: string;
  ctx: number;
  score: number;
}

export interface TempChoice {
  id: string;
  capability: Capability;
  status: AdmissionStatus;
  min_ram_gb: number;
  quant: QuantPick | null;
  reason: string;
}

export interface BlockedChoice {
  id: string;
  status: "BLOCKED";
  min_ram_gb: number;
  reason: string;
}

export interface Recommendation {
  hardware: HostFacts;
  policy: RecommenderPolicy;
  usable_gb: number;
  reserve_gb: number;
  temp_reserve_gb: number;
  resident_budget_gb: number;
  resident: ResidentChoice | null;
  resident_label: string | null;
  temp: TempChoice[];
  blocked: BlockedChoice[];
  warnings: string[];
  recommended_sysctl: { iogpu_wired_limit_mb: number };
  tradeoff: string;
  override: { id: string; active: boolean } | null;
}

export interface RecommendInput {
  hardware: HostFacts;
  catalog: Catalog;
  policy?: Partial<RecommenderPolicy>;
  /** 默认读 process.env；测试可注入。 */
  env?: Record<string, string | undefined>;
}

function mergePolicy(partial: Partial<RecommenderPolicy> | undefined): RecommenderPolicy {
  return { ...DEFAULT_POLICY, ...(partial ?? {}) };
}

function isCore(model: CatalogModel): boolean {
  return model.roles === undefined || model.roles.includes("core");
}

function pickQuant(
  model: CatalogModel,
  budgetGb: number,
  policy: RecommenderPolicy,
  minQuality: number,
): QuantPick | undefined {
  let best: QuantPick | undefined;
  for (const quant of model.quant_options) {
    if (quant.quality < minQuality) continue;
    const pick = toPick(quant, model, policy);
    if (pick.footprint_gb > budgetGb) continue;
    if (
      best === undefined ||
      pick.quality > best.quality ||
      (pick.quality === best.quality && pick.footprint_gb < best.footprint_gb)
    ) {
      best = pick;
    }
  }
  return best;
}

/** footprint 公式里的运行时开销（docs/07 §1：0.5–1.5GB）。 */
const HEADROOM_OVERHEAD_GB = 1.0;

function toPick(quant: CatalogQuant, model: CatalogModel, policy: RecommenderPolicy): QuantPick {
  const weights = weightsGb(model.params_total_b, quant);
  const kv = kvCacheGb(model.arch, policy.context_target, policy.kv_quant);
  return {
    label: quant.label,
    quality: quant.quality,
    weights_gb: weights,
    kv_gb: kv,
    footprint_gb: weights + kv + HEADROOM_OVERHEAD_GB,
  };
}

function lowestFootprintPick(model: CatalogModel, policy: RecommenderPolicy): QuantPick {
  let best: QuantPick | undefined;
  for (const quant of model.quant_options) {
    const pick = toPick(quant, model, policy);
    if (best === undefined || pick.footprint_gb < best.footprint_gb) best = pick;
  }
  if (best === undefined) throw new CatalogError("quant_options 为空", model.id);
  return best;
}

function quantByLabel(model: CatalogModel, label: string, policy: RecommenderPolicy): QuantPick | undefined {
  const quant = model.quant_options.find((q) => q.label === label);
  return quant === undefined ? undefined : toPick(quant, model, policy);
}

function fmt(value: number): string {
  return value.toFixed(2);
}

export function recommend(input: RecommendInput): Recommendation {
  const { hardware, catalog } = input;
  const policy = mergePolicy(input.policy);
  const env = input.env ?? process.env;
  const warnings: string[] = [];

  const reserve = appleReserveGb(hardware.ram_gb);
  const usable = appleUsableGb(hardware.ram_gb, policy.wired_mode);
  const tempReserve = policy.temp_slots * TEMP_SLOT_RESERVE_GB;
  const residentBudget = usable - tempReserve - HEADROOM_GB;

  warnings.push(
    `预算: ${hardware.ram_gb}GB ${policy.wired_mode} → usable ${fmt(usable)}GB（reserve ${fmt(reserve)}GB）；` +
      `常驻预算 ${fmt(residentBudget)}GB（预留临时 ${fmt(tempReserve)}GB + headroom ${fmt(HEADROOM_GB)}GB）`,
  );

  // 全局硬门槛：docs/13 §9.2 ① —— ram_gb < min_ram_gb 一律 BLOCKED。
  const blocked: BlockedChoice[] = [];
  for (const model of catalog.catalog) {
    if (hardware.ram_gb < model.min_ram_gb) {
      blocked.push({
        id: model.id,
        status: "BLOCKED",
        min_ram_gb: model.min_ram_gb,
        reason: `min_ram_gb=${model.min_ram_gb} > ram_gb=${hardware.ram_gb}`,
      });
    }
  }
  for (const b of blocked) {
    warnings.push(`${b.id}: BLOCKED（${b.reason}）`);
  }

  const eligible = catalog.catalog.filter((m) => m.status !== "experiment" && hardware.ram_gb >= m.min_ram_gb);

  // ---- 常驻：reasoning × quality 最高（docs/07 §5.3）----------------------
  let resident: ResidentChoice | null = null;
  let residentModel: CatalogModel | null = null;
  for (const model of eligible) {
    if (!isCore(model)) continue;
    const pick = pickQuant(model, residentBudget, policy, policy.quality_threshold);
    if (pick === undefined) continue;
    const score = (model.capability?.reasoning ?? 0) * pick.quality;
    if (
      resident === null ||
      residentModel === null ||
      score > resident.score ||
      (score === resident.score && pick.footprint_gb < resident.footprint_gb)
    ) {
      resident = { id: model.id, ctx: policy.context_target, score, ...pick };
      residentModel = model;
    }
  }

  // ---- IDORIS_CORE_MODEL override：让路但仍警告 --------------------------
  let override: { id: string; active: boolean } | null = null;
  const forced = env["IDORIS_CORE_MODEL"]?.trim();
  if (forced !== undefined && forced.length > 0) {
    const [forcedId, forcedQuant] = forced.split("@");
    const model = forcedId === undefined ? undefined : catalog.catalog.find((m) => m.id === forcedId);
    if (model === undefined) {
      warnings.push(`IDORIS_CORE_MODEL=${forced} 不在目录中：忽略 override，回落自动选择`);
    } else {
      override = { id: model.id, active: true };
      let pick = forcedQuant === undefined ? undefined : quantByLabel(model, forcedQuant, policy);
      if (forcedQuant !== undefined && pick === undefined) {
        warnings.push(`IDORIS_CORE_MODEL 指定的量化 ${forcedQuant} 不存在：改用自动量化`);
      }
      if (pick === undefined) {
        pick = pickQuant(model, residentBudget, policy, 0);
      }
      if (pick === undefined) {
        pick = lowestFootprintPick(model, policy);
        warnings.push(`IDORIS_CORE_MODEL=${model.id} 放不下常驻预算（${fmt(residentBudget)}GB）：已强制放行，请自行承担 OOM 风险`);
      } else if (pick.footprint_gb > residentBudget) {
        warnings.push(`IDORIS_CORE_MODEL=${model.id} footprint ${fmt(pick.footprint_gb)}GB > 常驻预算 ${fmt(residentBudget)}GB`);
      }
      if (hardware.ram_gb < model.min_ram_gb) {
        warnings.push(`IDORIS_CORE_MODEL=${model.id} 绕过 min_ram_gb 硬门槛（需 ${model.min_ram_gb}GB，实际 ${hardware.ram_gb}GB）`);
      }
      warnings.push(`IDORIS_CORE_MODEL=${forced} 生效：推荐模块让路（yields），不覆盖用户强制`);
      resident = { id: model.id, ctx: policy.context_target, score: (model.capability?.reasoning ?? 0) * pick.quality, ...pick };
      residentModel = model;
    }
  }

  // ---- 临时：按需求能力逐个 admission ------------------------------------
  const residentFootprint = resident?.footprint_gb ?? 0;
  const remaining = usable - residentFootprint;
  const temp: TempChoice[] = [];
  for (const capability of policy.needed_capabilities) {
    let bestModel: CatalogModel | undefined;
    let bestValue = -1;
    for (const model of eligible) {
      if (model.id === residentModel?.id) continue;
      const value = model.capability?.[capability] ?? 0;
      if (value <= 0) continue;
      if (value > bestValue) {
        bestValue = value;
        bestModel = model;
      }
    }
    if (bestModel === undefined) continue;
    const coexist = pickQuant(bestModel, remaining, policy, policy.quality_threshold);
    if (coexist !== undefined) {
      temp.push({
        id: bestModel.id,
        capability,
        status: "ready",
        min_ram_gb: bestModel.min_ram_gb,
        quant: coexist,
        reason: `footprint ${fmt(coexist.footprint_gb)}GB ≤ 剩余 ${fmt(remaining)}GB，可与常驻共存`,
      });
    } else {
      const alone = pickQuant(bestModel, usable, policy, policy.quality_threshold) ?? lowestFootprintPick(bestModel, policy);
      temp.push({
        id: bestModel.id,
        capability,
        status: "requires_eviction",
        min_ram_gb: bestModel.min_ram_gb,
        quant: alone,
        reason: `footprint ${fmt(alone.footprint_gb)}GB > 剩余 ${fmt(remaining)}GB，需驱逐常驻后加载`,
      });
      warnings.push(`${capability}: ${bestModel.id} 需驱逐常驻（${fmt(alone.footprint_gb)}GB > 剩余 ${fmt(remaining)}GB）`);
    }
  }

  if (resident === null) {
    warnings.push("没有模型能放进常驻预算：请提高 wired_mode 或降低 context_target");
  }

  const residentLabel = resident === null ? null : `${resident.id}@${resident.label}`;
  const tradeoff = buildTradeoff({ hardware, policy, usable, reserve, tempReserve, residentBudget, resident, residentLabel, temp, blocked });

  return {
    hardware,
    policy,
    usable_gb: usable,
    reserve_gb: reserve,
    temp_reserve_gb: tempReserve,
    resident_budget_gb: residentBudget,
    resident,
    resident_label: residentLabel,
    temp,
    blocked,
    warnings,
    recommended_sysctl: { iogpu_wired_limit_mb: recommendedWiredLimitMb(usable) },
    tradeoff,
    override,
  };
}

interface TradeoffInput {
  hardware: HostFacts;
  policy: RecommenderPolicy;
  usable: number;
  reserve: number;
  tempReserve: number;
  residentBudget: number;
  resident: ResidentChoice | null;
  residentLabel: string | null;
  temp: TempChoice[];
  blocked: BlockedChoice[];
}

function buildTradeoff(input: TradeoffInput): string {
  const lines: string[] = [];
  lines.push(
    `硬件 ${input.hardware.chip} / ${input.hardware.ram_gb}GB（${input.policy.wired_mode}，ctx ${input.policy.context_target}，KV ${input.policy.kv_quant}）`,
  );
  if (input.resident !== null) {
    lines.push(
      `常驻 ${input.residentLabel}：权重 ${fmt(input.resident.weights_gb)}GB + KV ${fmt(input.resident.kv_gb)}GB + 开销 = ` +
        `${fmt(input.resident.footprint_gb)}GB，预算 ${fmt(input.residentBudget)}GB；能力×质量=${input.resident.score.toFixed(4)}`,
    );
  } else {
    lines.push(`常驻：无（预算 ${fmt(input.residentBudget)}GB 放不下任何 core 模型）`);
  }
  const ready = input.temp.filter((t) => t.status === "ready");
  const evict = input.temp.filter((t) => t.status === "requires_eviction");
  if (ready.length > 0) {
    lines.push(`共存临时：${ready.map((t) => `${t.id}@${t.quant?.label ?? "?"}`).join("、")}`);
  }
  if (evict.length > 0) {
    lines.push(`需驱逐：${evict.map((t) => `${t.id}@${t.quant?.label ?? "?"}`).join("、")}`);
  }
  lines.push(
    input.blocked.length === 0
      ? "BLOCKED：无"
      : `BLOCKED：${input.blocked.map((b) => `${b.id}(需${b.min_ram_gb}GB)`).join("、")}`,
  );
  lines.push(`预算：usable ${fmt(input.usable)}GB（reserve ${fmt(input.reserve)}GB，临时预留 ${fmt(input.tempReserve)}GB）`);
  return lines.join("\n");
}

/** 便捷入口：从 catalog 文件路径 + 硬件 facts 直接给推荐。 */
export function recommendFromFile(path: string, hardware: HostFacts, input?: Omit<RecommendInput, "hardware" | "catalog">): Recommendation {
  return recommend({ hardware, catalog: loadCatalog(path), ...(input ?? {}) });
}

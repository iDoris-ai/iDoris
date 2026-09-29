import type { LoadPolicy } from "@idoris/contracts";
import type {
  Admission,
  BackendStatus,
  ChatRequest,
  ChatResponse,
  ModelBackend,
  ModelInfo,
  Pressure,
} from "../src/backend.js";

/** 只依赖最小的 fetch 形状，避免为库引入 DOM/@types/node 依赖。 */
export type FetchLike = (
  url: string,
  init?: { method?: string; headers?: Record<string, string>; body?: string },
) => Promise<{ ok: boolean; status: number; json(): Promise<unknown>; text(): Promise<string> }>;

export interface OmlxOptions {
  /** 默认 http://127.0.0.1:8088（本机自定义端口，见设计文档 D7）。 */
  baseUrl?: string;
  apiKey?: string;
  fetchImpl?: FetchLike;
}

const DEFAULT_URL = "http://127.0.0.1:8088";
const BYTES_PER_GIB = 1024 ** 3;
const KNOWN_PRESSURE_VALUES = ["ok", "soft", "hard", "ceiling"] as const;

/**
 * HTTP 失败的结构化错误：**只**携带 method、path（含 model id，是调用方自己传进来的，
 * 不是后端内容）和 HTTP 状态码，从不携带响应体内容。这是本文件里"安全的结构化元数据"
 * 的来源之一——其它错误类型需要知道"是不是一次 HTTP 失败/状态码是多少"时，从这个类型的
 * 实例上取字段，而不是解析某个错误的 `.message` 文本（H1）。
 */
class OmlxHttpError extends Error {
  readonly status: number;
  constructor(method: string, path: string, status: number) {
    super(`oMLX ${method} ${path} failed: HTTP ${status}`);
    this.name = "OmlxHttpError";
    this.status = status;
  }
}

/**
 * oMLX 在 0.6.4 上把设置 `is_pinned` 的能力搬到了要求 admin 会话认证的路由下
 * （见类头版本注记），本适配器没有 admin 会话能力，所以 `load(id, {mode:"resident"})`
 * 会在"模型已经装入成功"之后才发现 pin 不了。用专门的错误类型把这种
 * **「装入成功、pin 失败」的部分成功状态**和"装入本身就失败"区分开，
 * 调用方 catch 到它时应该知道：模型已经在跑（unpinned），只是没有被保护、
 * 压力大时可能被 LRU 驱逐。
 *
 * **只在"已经确认 pin 失败"时抛出**（M2）：要么是 `PUT .../settings` 本身被拒绝
 * （例如 401），要么是复核 `GET /v1/models/status` 之后读到 `pinned===false`。
 * 如果连"复核"这一步本身都没能完成（响应畸形、模型被并发卸载等），那是**状态未知**，
 * 不是"确认失败"，用 {@link OmlxPinStateUnverifiedError} 表达，不要混进这里。
 *
 * H1：message 是固定文案，**不拼接 `cause.message` 或 `String(cause)`**——上游
 * （包括注入的 `fetchImpl`）抛出的错误可能携带不该出现在日志里的内容。需要知道
 * "为什么"时，读 `causeErrorName`（错误类名/`typeof`）和 `causeHttpStatus`
 * （仅当 cause 是 {@link OmlxHttpError} 时才有值）这两个安全的结构化字段，
 * 不要试图从 `cause` 本身（或它的 `.message`）拼字符串。
 */
export class OmlxPinUnavailableError extends Error {
  /** 稳定错误码，供调用方判断，不依赖 `instanceof` 跨包比较。 */
  readonly code = "OMLX_PIN_UNAVAILABLE" as const;
  readonly modelId: string;
  /** 安全的结构化元数据：cause 的错误类名（或非 Error 时的 `typeof`）。不含 cause.message。 */
  readonly causeErrorName: string | undefined;
  /** 安全的结构化元数据：cause 是 HTTP 失败时的状态码；不是 HTTP 失败（例如"复核后确认 unpinned"）时是 undefined。 */
  readonly causeHttpStatus: number | undefined;

  constructor(modelId: string, cause?: unknown) {
    super(
      `oMLX 模型 "${modelId}" 未能确认设置为常驻（is_pinned=true）：0.6.4 上 ` +
        `PUT /admin/api/models/{id}/settings 需要独立的 admin 会话认证，仅推理 API key 不可用；` +
        `或者该请求返回成功，但复核 GET /v1/models/status 后确认仍是 unpinned` +
        `（见 spike/u0/U0-LOG.md「0.6.4 复测」/ FU-17）。模型目前以 unpinned 状态运行，` +
        `压力大时可能被驱逐。具体原因见 causeErrorName / causeHttpStatus 字段（不含后端原始内容）。`,
    );
    this.name = "OmlxPinUnavailableError";
    this.modelId = modelId;
    this.causeErrorName = cause === undefined ? undefined : cause instanceof Error ? cause.constructor.name : typeof cause;
    this.causeHttpStatus = cause instanceof OmlxHttpError ? cause.status : undefined;
  }
}

/**
 * 非 resident（`on_demand` / `evict_to_load` / 未传 policy）加载完成后，实测发现该模型
 * 其实处于 `pinned` 状态——这不是本适配器造成的（本适配器只有 `mode:"resident"` 才会
 * 尝试 pin，而那条路径在 0.6.4 上必然因 401 抛 `OmlxPinUnavailableError`，不会"悄悄成功"），
 * 而是**外部状态漂移**：用户在 oMLX 管理页手动 pin 过、pin 状态跨重启持久化保留、或者
 * 模型之前被切换成过 `resident`（H-a）。
 *
 * 0.6.4 上取消 pin 同样需要 admin 会话认证（见 `OmlxPinUnavailableError`），本适配器没有
 * 该能力，**无法自动纠正**，只能如实抛错，把这个 LoadPolicy 与实际状态不一致的事实报告给调用方。
 */
export class OmlxUnexpectedlyPinnedError extends Error {
  readonly code = "OMLX_UNEXPECTEDLY_PINNED" as const;
  readonly modelId: string;

  constructor(modelId: string) {
    super(
      `oMLX 模型 "${modelId}" 已加载，但处于 pinned 状态，与所请求的 LoadPolicy（非 resident）不符：` +
        `可能是被本适配器管辖范围之外的操作 pin 住的（如 oMLX 管理页手动 pin、pin 状态跨重启持久化、` +
        `或曾经切换成过 resident）。0.6.4 上 unpin 需要 admin 会话认证（见 FU-17），本适配器暂无该能力，` +
        `无法自动纠正，只能如实报告这个不一致。`,
    );
    this.name = "OmlxUnexpectedlyPinnedError";
    this.modelId = modelId;
  }
}

/**
 * `GET /v1/models/status` 的响应没能让我们**放心确认**某个模型的 `loaded`/`pinned` 状态
 * （H1）：可能是 `models` 字段整个缺失、找不到/找到多个 id 匹配的条目、`loaded`/`pinned`
 * 字段类型不对，或者最关键的一种——**`loaded===false`**（模型在我们 `POST .../load` 成功
 * 之后、读这次 status 之前，被并发 unload 掉了）。
 *
 * 这些情况下**一律 fail-closed 抛错，不猜测、不当成"未 pin"处理**：一个读不懂/读不到干净
 * 状态的校验，不能被静默折叠成"看起来没问题"。`reason` 是稳定的枚举值，`message` 只描述
 * 字段名和期望/实际的类型，**不包含后端返回的任何原始值**（H2：日志与错误不带载荷）。
 */
export type OmlxVerificationFailureReason =
  | "response_invalid"
  | "models_missing"
  | "model_not_found"
  | "duplicate_model_entries"
  | "loaded_field_invalid"
  | "not_loaded"
  | "pinned_field_invalid";

const VERIFICATION_FAILURE_DETAIL: Record<OmlxVerificationFailureReason, string> = {
  response_invalid: "GET /v1/models/status 的顶层响应不是一个 JSON 对象（例如 null/数字/字符串/数组）",
  models_missing: "GET /v1/models/status 响应的 models 字段缺失或不是数组",
  model_not_found: "models 数组中没有找到 id 匹配的条目",
  duplicate_model_entries: "models 数组中有多个 id 匹配的条目（期望恰好一个）",
  loaded_field_invalid: "匹配到的条目的 loaded 字段不是 boolean 类型",
  not_loaded: "匹配到的条目 loaded=false（可能是在校验窗口内被并发卸载）",
  pinned_field_invalid: "匹配到的条目的 pinned 字段不是 boolean 类型",
};

export class OmlxVerificationError extends Error {
  readonly code = "OMLX_VERIFICATION_FAILED" as const;
  readonly modelId: string;
  readonly reason: OmlxVerificationFailureReason;

  constructor(modelId: string, reason: OmlxVerificationFailureReason) {
    super(
      `oMLX 模型 "${modelId}" 的状态校验失败（reason=${reason}）：${VERIFICATION_FAILURE_DETAIL[reason]}。` +
        `这是部分成功/校验失败，不是"确认未被 pin"，调用方不应把它当成正常的 on_demand 结果处理。`,
    );
    this.name = "OmlxVerificationError";
    this.modelId = modelId;
    this.reason = reason;
  }
}

/**
 * `PUT /admin/api/models/{id}/settings`（resident 分支的 pin 请求）返回成功之后，用来
 * 复核"是否真的 pin 上了"的 `GET /v1/models/status`（{@link OmlxBackend.verifyModelState}）
 * 本身失败了——响应畸形、找不到条目、模型在复核窗口内被并发卸载，等等。
 *
 * 这种情况下我们**不知道**模型最终是不是 pinned：既不能算"确认成功"，也不能算
 * {@link OmlxPinUnavailableError} 那种"确认失败"（那需要读到明确的 `pinned===false`
 * 或者 PUT 请求本身被拒绝）——只能如实报告"状态未知"（M2，修复前是把这种情况也包成
 * `OmlxPinUnavailableError`，等于把"不知道"错误地说成了"确认失败/被 admin 认证拒绝"）。
 *
 * `causeReason` 只在 cause 是 {@link OmlxVerificationError} 时才有值（复用它稳定的
 * `reason` 枚举，不是后端原始内容）；`causeErrorName`/`causeHttpStatus` 同 H1。
 */
export class OmlxPinStateUnverifiedError extends Error {
  readonly code = "OMLX_PIN_STATE_UNVERIFIED" as const;
  readonly modelId: string;
  readonly causeErrorName: string;
  readonly causeReason: OmlxVerificationFailureReason | undefined;
  readonly causeHttpStatus: number | undefined;

  constructor(modelId: string, cause: unknown) {
    super(
      `oMLX 模型 "${modelId}" 的 pin 设置请求已返回成功，但复核 GET /v1/models/status 时失败，` +
        `无法确认模型最终是否真的处于 pinned 状态——这不是"确认成功"也不是"确认失败"，是未知。` +
        `具体原因见 causeErrorName / causeReason / causeHttpStatus 字段（不含后端原始内容）。`,
    );
    this.name = "OmlxPinStateUnverifiedError";
    this.modelId = modelId;
    this.causeErrorName = cause instanceof Error ? cause.constructor.name : typeof cause;
    this.causeReason = cause instanceof OmlxVerificationError ? cause.reason : undefined;
    this.causeHttpStatus = cause instanceof OmlxHttpError ? cause.status : undefined;
  }
}

/** 判断 `x` 是不是一个"普通对象"——排除 `null` 和数组，两者 `typeof` 都会误报成 `"object"`。 */
function isPlainRecord(x: unknown): x is Record<string, unknown> {
  return typeof x === "object" && x !== null && !Array.isArray(x);
}

/**
 * 报告"这是什么类型"，专给错误信息/日志用（H2/M1）：区分 `null`、数组和普通 object
 * （`typeof` 三者都报 `"object"`，会把 `null`/数组这种关键信息抹掉），但**不**输出值本身。
 */
function describeType(x: unknown): string {
  if (x === null) return "null";
  if (Array.isArray(x)) return "array";
  return typeof x;
}

/**
 * oMLX 适配器（T1.2.2）：把 LoadPolicy 抽象映射到 oMLX 实测端点（spike/u0/U0-LOG.md）。
 * - resident → is_pinned=true。PUT 成功后还会用 `verifyModelState()` 复核 `pinned===true`
 *   才算数：PUT 被拒绝、或复核后确认 `pinned===false`，两者都是**已确认失败**，抛
 *   `OmlxPinUnavailableError`；但如果复核这一步本身失败（响应畸形、并发卸载等），那是
 *   **状态未知**，不是"确认失败"，抛 `OmlxPinStateUnverifiedError`（这两种不能混用）
 * - on_demand/evict_to_load/未传 policy → 不主动调用 pin 端点，但会用 `verifyModelState()`
 *   严格核对该模型实际的 loaded/pinned 状态，确认被 pin 住时抛 `OmlxUnexpectedlyPinnedError`
 *   （见 `load()` 注释，H-a）；`verifyModelState()` 本身解析失败（响应畸形、找不到条目、
 *   或模型被并发卸载）会抛 `OmlxVerificationError`，**一律 fail-closed，不当成"未被 pin"**
 * - 显式 POST /v1/models/{id}/load | /unload
 * - admission / status 读 GET /api/status（model_memory_max + loaded_models）
 *
 * ⚠️ 本适配器是本仓内唯一允许出现 "omlx" 字样的实现层；Router 核心不得引用它。
 *
 * ⚠️ **日志/错误不带载荷**：本文件所有抛出的错误信息和 `console.warn` 只写字段名、期望的
 * 类型、实际收到的类型（`describeType()`，能区分 `null`/数组/其它 object，不是笼统的
 * `typeof`）或下标，**从不把后端返回的原始值字符串化写进去，也不拼接下游抛出的
 * `err.message`/`String(err)`**——上游（包括注入的 `fetchImpl`）抛出的错误本身可能就携带
 * 不该出现在日志里的内容。需要表达"为什么失败"时，只使用安全的结构化元数据（HTTP 状态码、
 * 错误类名、`OmlxVerificationError.reason` 这类固定枚举值），见 `OmlxPinUnavailableError`/
 * `OmlxPinStateUnverifiedError` 的 `causeErrorName`/`causeHttpStatus`/`causeReason` 字段。
 * 改这个文件时新增校验分支/错误类型要延续这条规则。
 *
 * 版本注记（FU-16，0.6.4 复测，见 spike/u0/U0-LOG.md「0.6.4 复测」一节，2026-09-27）：
 * - `/api/status` 的已加载模型字段在 0.6.4 上是 `loaded_models`（不是 v0.4.3 假设的 `loaded`）；
 *   两个字段名都兼容解析，但**缺失/类型不对时会抛错，元素不是字符串也会抛错**，不会静默
 *   退化成空数组或悄悄丢元素（见 `parseLoaded`）。顶层响应不是普通对象（`null`/数字/字符串/
 *   数组）时也会先被 `status()` 挡下来抛出可读错误，不会让后续字段访问直接抛 `TypeError`。
 * - `/api/status` 的 `model_memory_max` / `model_memory_used` 实测单位是**字节**，已换算成
 *   `ModelBackend` 契约要求的 GiB 口径（÷1024³，见 `status()`/`parseMemoryGb`）——修复前是
 *   直接把字节数塞进 `*Gb` 字段，数值被夸大了 2^30 倍，**这个字段之前不能被当成"已验证可用"**；
 *   要求严格是 `number` 类型、有限、`>=0`，不做 `Number(value)` 那种会接受字符串/布尔/数组的
 *   宽松转换（fail-closed）。
 * - `pressure` 字段缺失时返回显式的 `"unknown"`（不是 fail-open 地当成 `"ok"`）；不在
 *   `ok/soft/hard/ceiling` 白名单里的值会 warn 一行（固定脱敏文案，只报告类型）后按
 *   `"unknown"` 处理，不抛错、不影响同一次 `status()` 里 `loaded` 字段的解析，也不会被无脑
 *   `as Pressure`（见 `parsePressure`）。
 * - 设置 `is_pinned` 的端点在 0.6.4 上从 `POST /admin/settings`（已 404）搬到了
 *   `PUT /admin/api/models/{id}/settings`（body 从 `{model_settings:{id:{is_pinned}}}` 拍平成 `{is_pinned}`）。
 *   **该 body 形状只是照 `GET /openapi.json` 的 `ModelSettingsRequest` schema 编的，从未实测跑通过**
 *   （见下一条 401，请求从未真正到达 handler 逻辑）。
 *   **已知缺口**：`/admin/api/*` 在 0.6.4 要求独立的 admin 会话认证，仅凭 `/v1/*` 用的推理 API key
 *   会被拒绝（`401 Admin authentication required`，已实测）。本适配器没有 admin 会话能力，
 *   因此 `load(id, { mode: "resident" })` 目前在 0.6.4 上会抛出 `OmlxPinUnavailableError`，而不是
 *   真的把模型 pin 住；pin 语义（包括反向的 unpin）在拿到 admin 会话支持前对 0.6.4 是未打通的
 *   （跟进见 FU-17）。`GET /v1/models/status` 则**已实测确认**推理 API key 可以正常读（200，
 *   只读），本适配器用它严格核对"非 resident 加载的模型是否其实被外部 pin 住"（H-a），
 *   以及 resident 分支 pin 是否真的生效。
 *   `pressure` 在 `--memory-guard` 模式下的字段名是否不变也未验证（跟进见 FU-18）。
 */
export class OmlxBackend implements ModelBackend {
  private readonly baseUrl: string;
  private readonly apiKey: string | undefined;
  private readonly fetchImpl: FetchLike;

  constructor(opts: OmlxOptions = {}) {
    this.baseUrl = (opts.baseUrl ?? DEFAULT_URL).replace(/\/$/, "");
    this.apiKey = opts.apiKey;
    this.fetchImpl = opts.fetchImpl ?? ((globalThis as unknown as { fetch: FetchLike }).fetch);
  }

  async list(): Promise<ModelInfo[]> {
    const body = (await this.json("GET", "/v1/models")) as { data?: Array<{ id?: string }> };
    return (body.data ?? [])
      .filter((m): m is { id: string } => typeof m.id === "string")
      .map((m) => ({ id: m.id, memoryGb: 0 }));
  }

  async load(id: string, policy?: LoadPolicy): Promise<void> {
    await this.post(`/v1/models/${encodeURIComponent(id)}/load`);
    if (policy?.mode === "resident") {
      // resident 需要显式 pin。0.6.4 上这个端点要求 admin 会话（见类头版本注记），
      // 用推理 API key 调用当前必然 401——但模型此时已经装入成功，只是没被 pin 住，
      // 这是"部分成功"而不是"整体失败"，所以包成专门的 OmlxPinUnavailableError，
      // 让调用方能分辨"装不进去"和"装进去了但没保护住"这两种不同的失败模式（H1）。
      try {
        await this.setPinned(id, true);
      } catch (err) {
        // 已确认失败：PUT 本身被拒绝（0.6.4 上必然是 401）。
        throw new OmlxPinUnavailableError(id, err);
      }
      // PUT 返回 2xx 不等于真的 pin 上了——用同一个严格状态解析器复核，
      // 要求 loaded===true 且 pinned===true 才算数。
      let verified: { loaded: boolean; pinned: boolean };
      try {
        verified = await this.verifyModelState(id);
      } catch (err) {
        // M2：复核这一步本身失败（响应畸形、模型被并发卸载等）= **状态未知**，
        // 不是"确认 pin 失败"——不能包成 OmlxPinUnavailableError（那意味着"已经
        // 确认是 admin 认证被拒或读到 unpinned"），用专门的 OmlxPinStateUnverifiedError
        // 如实表达"不知道"，让调用方能把"未知"和"已确认失败"分开处理。
        throw new OmlxPinStateUnverifiedError(id, err);
      }
      if (!verified.pinned) {
        // 已确认失败：复核读到了明确的 pinned===false。
        throw new OmlxPinUnavailableError(id);
      }
      return;
    }
    // 其余分支（on_demand / evict_to_load，以及未传 policy）刻意不调用 setPinned(id, false)：
    // - 已实测 `POST /v1/models/{id}/load` 本身不会把模型隐式 pin 住
    //   （2026-09-27 复测：刚 load 的 Qwen3-0.6B-4bit 在 `/v1/models/status` 里 `pinned:false`，
    //   记录见 spike/u0/U0-LOG.md）；
    // - 本适配器是这套状态里唯一会把 `is_pinned` 设成 true 的地方，而那条路径在 0.6.4 上
    //   会直接抛 `OmlxPinUnavailableError`、不会"悄悄成功"——所以只要调用方没见过这个错误，
    //   就说明本适配器从来没有真正把这个模型 pin 住过；
    // - 这样也避免了 on_demand 这个最常见的路径在 0.6.4 上每次 load 都白打一次必 401 的 admin 端点。
    //
    // 但"本适配器没 pin 过"不等于"这个模型现在真的是 unpinned"——它可能早就被外部 pin 住
    // （用户在 oMLX 管理页手动 pin、pin 状态跨重启持久化、或者曾经被切换成过 resident）。
    // 对这种情况保持沉默会让调用方以为自己拿到的是 on_demand 语义的模型，实际上却是个不会
    // 被驱逐的 pinned 模型（H-a）。所以 load 完成后主动查一次 `GET /v1/models/status`（已实测
    // 2026-09-27：用推理 API key 就能读，200，只读，不改变任何状态，见 U0-LOG），核对该模型的
    // `pinned` 字段；不一致就抛 `OmlxUnexpectedlyPinnedError`，而不是悄悄放行。
    // H1：这里**不吞掉**校验本身的失败——`verifyModelState` 在响应畸形、找不到条目，或者
    // 最关键的"模型在校验窗口内被并发卸载"（loaded===false）时会抛 `OmlxVerificationError`，
    // 直接向上传播，不当成"未被 pin"处理（fail-closed；修复前是 fail-open：解析不出 pinned
    // 字段就默认 false，等于假装"没问题"）。
    // 已知限制（记入 FU-17）：发现被外部 pin 住后，本适配器**无法自动 unpin**（同样需要 admin
    // 会话），只能检测并报告，不能纠正。
    const { pinned } = await this.verifyModelState(id);
    if (pinned) {
      throw new OmlxUnexpectedlyPinnedError(id);
    }
  }

  async unload(id: string): Promise<void> {
    await this.post(`/v1/models/${encodeURIComponent(id)}/unload`);
  }

  async admission(id: string): Promise<Admission> {
    const st = await this.status();
    if (st.loaded.includes(id)) return "coexist";
    // 未知模型内存时保守判 requires_eviction（宁可先驱逐也不超 guard）
    return "requires_eviction";
  }

  async status(): Promise<BackendStatus> {
    const raw = await this.json("GET", "/api/status");
    // 顶层响应必须是一个普通对象——null/数字/字符串/数组都不是合法的 /api/status 形状。
    // 不校验这一步，`raw.loaded_models` 这类访问在 raw 为 null 时会直接抛 TypeError，
    // 而不是这里统一、可读的错误（同 verifyModelState 的顶层校验，见下）。
    if (!isPlainRecord(raw)) {
      throw new Error(`oMLX /api/status 的响应不是一个 JSON 对象（实际类型：${describeType(raw)}）`);
    }
    const body = raw;
    const loaded = this.parseLoaded(body);
    // 0.6.4 实测 model_memory_max/model_memory_used 单位是字节（例如 55662788608 = 51.84GB，
    // 与响应里自带的 model_memory_max_formatted 对得上）；ModelBackend 契约的 *Gb 字段是 GiB
    // 口径，这里必须除以 1024^3 换算，不能把字节数直接塞进去（修复前会夸大 2^30 倍）。
    const max = this.parseMemoryGb(body.model_memory_max, "model_memory_max");
    const used = this.parseMemoryGb(body.model_memory_used, "model_memory_used");
    const pressure = this.parsePressure(body.pressure);
    return { pressure, usedGb: used, modelMemoryMaxGb: max, loaded };
  }

  /**
   * 查一次 `GET /v1/models/status`，**严格**核对某个模型当前的 `loaded`/`pinned` 状态（H-a/H1）。
   * 已实测（2026-09-27）：这个端点用推理 API key 就能读（200），是只读操作，不会
   * 改变任何状态——不像 `PUT /admin/api/models/{id}/settings` 那样需要 admin 会话。
   *
   * **fail-closed，不 fail-open**：要求顶层响应是一个普通对象（不是 `null`/数字/字符串/
   * 数组——这些访问 `.models` 要么直接抛 `TypeError` 崩掉，要么静默拿到 `undefined` 从而
   * 被误判成"models 缺失"，两种都不如显式报告"整个响应就不是期望的形状"，M1）、`models`
   * 是数组、里面恰好有一条 `id` 匹配的条目、`loaded` 是 boolean 且为 `true`
   * （`loaded===false` 说明模型在我们 `POST .../load` 成功之后、读这次 status 之前被并发
   * unload 掉了，这本身就是一种校验失败，不能被吞掉）、`pinned` 是 boolean。任何一步不
   * 满足都抛 `OmlxVerificationError`（H1，修复前是 fail-open：找不到条目/字段缺失就默认
   * `false`=="未被 pin"，等于假装状态正常）。
   */
  private async verifyModelState(id: string): Promise<{ loaded: boolean; pinned: boolean }> {
    const raw = await this.json("GET", "/v1/models/status");
    if (!isPlainRecord(raw)) {
      throw new OmlxVerificationError(id, "response_invalid");
    }
    const body = raw;
    const models = body.models;
    if (!Array.isArray(models)) {
      throw new OmlxVerificationError(id, "models_missing");
    }
    const matches = models.filter((m): m is Record<string, unknown> => isPlainRecord(m) && m.id === id);
    if (matches.length === 0) {
      throw new OmlxVerificationError(id, "model_not_found");
    }
    if (matches.length > 1) {
      throw new OmlxVerificationError(id, "duplicate_model_entries");
    }
    const entry = matches[0]!;
    if (typeof entry.loaded !== "boolean") {
      throw new OmlxVerificationError(id, "loaded_field_invalid");
    }
    if (!entry.loaded) {
      throw new OmlxVerificationError(id, "not_loaded");
    }
    if (typeof entry.pinned !== "boolean") {
      throw new OmlxVerificationError(id, "pinned_field_invalid");
    }
    return { loaded: entry.loaded, pinned: entry.pinned };
  }

  /**
   * 解析 `/api/status` 的已加载模型列表（M2）。
   * - 0.6.4 实测字段名是 `loaded_models`；`loaded` 是更早版本可能用过的旧名。
   * - `loaded_models` 存在且非 null 时**始终优先**使用它（即便 `loaded` 也存在）。
   * - `loaded_models` 缺失或为 `null` 时才回退到 `loaded`。
   * - 两个键都缺失/为 `null`，或者选中的值不是数组，都**必须抛错**——
   *   不能像修复前那样静默退化成 `[]`（那会让 `admission()` 把"看不懂响应"
   *   和"这个模型确实没加载"混为一谈，进而一律误判成 `requires_eviction`）。
   *
   * H2：错误信息只写字段名和实际类型/下标，**不把收到的原始值字符串化写进去**——
   * 后端返回的内容可能包含不该出现在日志/错误里的东西，这里只报告"长什么样"，不报告"是什么"。
   */
  private parseLoaded(body: Record<string, unknown>): string[] {
    const loadedModels = body.loaded_models;
    const raw = loadedModels === undefined || loadedModels === null ? body.loaded : loadedModels;
    if (raw === undefined || raw === null) {
      throw new Error("oMLX /api/status 缺少 loaded_models 与 loaded 字段，无法确定已加载模型列表");
    }
    if (!Array.isArray(raw)) {
      throw new Error(`oMLX /api/status 的已加载模型字段不是数组（实际类型：${describeType(raw)}）`);
    }
    // M-a：元素不是字符串时直接抛错，不能用 filter 静默丢弃——丢弃掉的那个模型 id
    // 就从"已加载列表"里凭空消失了，会让 admission()/驱逐决策看到一个偏小的已加载集合。
    return raw.map((x, i) => {
      if (typeof x !== "string") {
        throw new Error(`oMLX /api/status 的已加载模型列表第 ${i} 项不是字符串（实际类型：${describeType(x)}）`);
      }
      return x;
    });
  }

  /**
   * 解析 `/api/status` 的 `model_memory_max` / `model_memory_used`（L-a/M3，字节，见 `status()`）。
   * 要求**必须已经是** `number` 类型、是有限数字、且 `>= 0`（M3：不做 `Number(value)` 那种宽松转换，
   * 否则会接受 `"17179869184"` 这样的字符串、`true`/`false`、`[]` 甚至负数）；不满足就抛错
   * （fail-closed，不当成 0）。错误信息只写实际类型，不把原始值字符串化（H2）。
   */
  private parseMemoryGb(value: unknown, fieldName: string): number {
    if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
      throw new Error(`oMLX /api/status 的 ${fieldName} 必须是 >=0 的有限 number（实际类型：${describeType(value)}）`);
    }
    return value / BYTES_PER_GIB;
  }

  /**
   * 解析 `/api/status` 的 `pressure` 字段（H3，M-c）。
   * - 缺失（undefined/null，例如本次复测用的实例没带 `--memory-guard` 启动）时返回
   *   显式的 `"unknown"`——**不是** fail-open 地当成 `"ok"`；`"unknown"` 的消费方必须按
   *   保守方向处理（见 `Pressure` 类型定义 / FU-18）。
   * - 值存在但不在 `ok/soft/hard/ceiling` 白名单里（类型不对或拼写变了）时**不抛错**，
   *   warn 一行后按 `"unknown"` 处理——`pressure` 只是 `status()` 里的一个字段，不该让它
   *   的解析失败连带炸掉同一次调用里已经解析好的 `loaded` 列表（M-c）。
   *   H2：warn 文案是固定的脱敏文案，只报告实际类型，**不把收到的原始值写进日志**。
   */
  private parsePressure(value: unknown): Pressure {
    if (value === undefined || value === null) return "unknown";
    if (typeof value === "string" && (KNOWN_PRESSURE_VALUES as readonly string[]).includes(value)) {
      return value as Pressure;
    }
    console.warn(`[idoris] oMLX /api/status 返回了不认识的 pressure 值（实际类型：${describeType(value)}），已按 "unknown" 处理`);
    return "unknown";
  }

  async chat(req: ChatRequest): Promise<ChatResponse> {
    const body = (await this.json("POST", "/v1/chat/completions", { model: req.model, messages: req.messages })) as {
      choices?: Array<{ message?: { content?: string } }>;
    };
    const content = body.choices?.[0]?.message?.content ?? "";
    return { model: req.model, content };
  }

  private async setPinned(id: string, pinned: boolean): Promise<void> {
    // 0.6.4：POST /admin/settings 已 404；换成 PUT /admin/api/models/{id}/settings，
    // body 拍平为 {is_pinned}——**这个 body 形状只按 GET /openapi.json 的
    // ModelSettingsRequest schema 编的，未实测**：本机调用这个端点会先被 admin 会话
    // 认证挡在 401，请求从未真正到达 handler，无法确认这个 body 真的会让
    // is_pinned 生效（M5，见头部版本注记 / FU-17）。
    await this.json("PUT", `/admin/api/models/${encodeURIComponent(id)}/settings`, { is_pinned: pinned });
  }

  private headers(): Record<string, string> {
    const h: Record<string, string> = { "content-type": "application/json" };
    if (this.apiKey) h.authorization = `Bearer ${this.apiKey}`;
    return h;
  }

  private async post(path: string): Promise<void> {
    const res = await this.fetchImpl(this.baseUrl + path, { method: "POST", headers: this.headers() });
    // H1：抛结构化的 OmlxHttpError（只带 method/path/status），不是拼了一堆内容的裸 Error——
    // 这样上层需要"安全的失败原因"时，能从 .status 读结构化字段，不用去解析/转发 message 文本。
    if (!res.ok) throw new OmlxHttpError("POST", path, res.status);
  }

  private async json(method: string, path: string, payload?: unknown): Promise<unknown> {
    const res = await this.fetchImpl(this.baseUrl + path, {
      method,
      headers: this.headers(),
      ...(payload === undefined ? {} : { body: JSON.stringify(payload) }),
    });
    if (!res.ok) throw new OmlxHttpError(method, path, res.status);
    return res.json();
  }
}

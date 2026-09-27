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
 * oMLX 在 0.6.4 上把设置 `is_pinned` 的能力搬到了要求 admin 会话认证的路由下
 * （见类头版本注记），本适配器没有 admin 会话能力，所以 `load(id, {mode:"resident"})`
 * 会在"模型已经装入成功"之后才发现 pin 不了。用专门的错误类型把这种
 * **「装入成功、pin 失败」的部分成功状态**和"装入本身就失败"区分开，
 * 调用方 catch 到它时应该知道：模型已经在跑（unpinned），只是没有被保护、
 * 压力大时可能被 LRU 驱逐。
 */
export class OmlxPinUnavailableError extends Error {
  readonly modelId: string;

  constructor(modelId: string, cause: unknown) {
    const causeMsg = cause instanceof Error ? cause.message : String(cause);
    super(
      `oMLX 模型 "${modelId}" 已加载，但设置常驻（is_pinned=true）失败：0.6.4 上 ` +
        `PUT /admin/api/models/{id}/settings 需要独立的 admin 会话认证，仅推理 API key 不可用` +
        `（见 spike/u0/U0-LOG.md「0.6.4 复测」/ FU-17）。模型目前以 unpinned 状态运行，` +
        `压力大时可能被驱逐。原始错误：${causeMsg}`,
      { cause }, // Error.cause（ES2022）：保留原始异常，不用自定义同名字段覆盖内置属性
    );
    this.name = "OmlxPinUnavailableError";
    this.modelId = modelId;
  }
}

/**
 * oMLX 适配器（T1.2.2）：把 LoadPolicy 抽象映射到 oMLX 实测端点（spike/u0/U0-LOG.md）。
 * - resident → is_pinned=true；on_demand/evict_to_load → 不主动调用 pin 端点（见 load() 注释）
 * - 显式 POST /v1/models/{id}/load | /unload
 * - admission / status 读 GET /api/status（model_memory_max + loaded_models）
 *
 * ⚠️ 本适配器是本仓内唯一允许出现 "omlx" 字样的实现层；Router 核心不得引用它。
 *
 * 版本注记（FU-16，0.6.4 复测，见 spike/u0/U0-LOG.md「0.6.4 复测」一节，2026-09-27）：
 * - `/api/status` 的已加载模型字段在 0.6.4 上是 `loaded_models`（不是 v0.4.3 假设的 `loaded`）；
 *   两个字段名都兼容解析，但**缺失或类型不对时会抛错，不会静默退化成空数组**（见 `parseLoaded`，M2）。
 * - `/api/status` 的 `model_memory_max` / `model_memory_used` 实测单位是**字节**，已换算成
 *   `ModelBackend` 契约要求的 GiB 口径（÷1024³，见 `status()`，M1）——修复前是直接把字节数
 *   塞进 `*Gb` 字段，数值被夸大了 2^30 倍，**这个字段之前不能被当成"已验证可用"**。
 * - `pressure` 字段缺失时返回显式的 `"unknown"`（不是 fail-open 地当成 `"ok"`）；不在
 *   `ok/soft/hard/ceiling` 白名单里的非法值会直接抛错，不会被无脑 `as Pressure`（见 `parsePressure`，H3）。
 * - 设置 `is_pinned` 的端点在 0.6.4 上从 `POST /admin/settings`（已 404）搬到了
 *   `PUT /admin/api/models/{id}/settings`（body 从 `{model_settings:{id:{is_pinned}}}` 拍平成 `{is_pinned}`）。
 *   **该 body 形状只是照 `GET /openapi.json` 的 `ModelSettingsRequest` schema 编的，从未实测跑通过**
 *   （见下一条 401，请求从未真正到达 handler 逻辑）。
 *   **已知缺口**：`/admin/api/*` 在 0.6.4 要求独立的 admin 会话认证，仅凭 `/v1/*` 用的推理 API key
 *   会被拒绝（`401 Admin authentication required`，已实测）。本适配器没有 admin 会话能力，
 *   因此 `load(id, { mode: "resident" })` 目前在 0.6.4 上会抛出 `OmlxPinUnavailableError`，而不是
 *   真的把模型 pin 住；pin 语义在拿到 admin 会话支持前对 0.6.4 是未打通的（跟进见 FU-17）。
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
        throw new OmlxPinUnavailableError(id, err);
      }
    }
    // 其余分支（on_demand / evict_to_load，以及未传 policy）刻意不调用 setPinned(id, false)：
    // - 已实测 `POST /v1/models/{id}/load` 本身不会把模型隐式 pin 住
    //   （2026-09-27 复测：刚 load 的 Qwen3-0.6B-4bit 在 `/v1/models/status` 里 `pinned:false`）；
    // - 本适配器是这套状态里唯一会把 `is_pinned` 设成 true 的地方，而那条路径在 0.6.4 上
    //   会直接抛 `OmlxPinUnavailableError`、不会"悄悄成功"——所以只要调用方没见过这个错误，
    //   就说明本适配器从来没有真正把这个模型 pin 住过，也就不存在需要"反悔取消 pin"的状态；
    // - 这样也避免了 on_demand 这个最常见的路径在 0.6.4 上每次 load 都白打一次必 401 的 admin 端点。
    // 已知限制（记入 FU-17）：如果模型是被本适配器管辖范围之外的东西（例如 oMLX 自带管理页）
    // 手动 pin 过的，我们不会主动替它 unpin——那属于跨面板状态漂移，不在本次修复范围内。
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
    const body = (await this.json("GET", "/api/status")) as Record<string, unknown>;
    const loaded = this.parseLoaded(body);
    // 0.6.4 实测 model_memory_max/model_memory_used 单位是字节（例如 55662788608 = 51.84GB，
    // 与响应里自带的 model_memory_max_formatted 对得上）；ModelBackend 契约的 *Gb 字段是 GiB
    // 口径，这里必须除以 1024^3 换算，不能把字节数直接塞进去（M1——修复前会夸大 2^30 倍）。
    const max = Number(body.model_memory_max ?? 0) / BYTES_PER_GIB;
    const used = Number(body.model_memory_used ?? 0) / BYTES_PER_GIB;
    const pressure = this.parsePressure(body.pressure);
    return { pressure, usedGb: used, modelMemoryMaxGb: max, loaded };
  }

  /**
   * 解析 `/api/status` 的已加载模型列表（M2）。
   * - 0.6.4 实测字段名是 `loaded_models`；`loaded` 是更早版本可能用过的旧名。
   * - `loaded_models` 存在且非 null 时**始终优先**使用它（即便 `loaded` 也存在）。
   * - `loaded_models` 缺失或为 `null` 时才回退到 `loaded`。
   * - 两个键都缺失/为 `null`，或者选中的值不是数组，都**必须抛错**——
   *   不能像修复前那样静默退化成 `[]`（那会让 `admission()` 把"看不懂响应"
   *   和"这个模型确实没加载"混为一谈，进而一律误判成 `requires_eviction`）。
   */
  private parseLoaded(body: Record<string, unknown>): string[] {
    const loadedModels = body.loaded_models;
    const raw = loadedModels === undefined || loadedModels === null ? body.loaded : loadedModels;
    if (raw === undefined || raw === null) {
      throw new Error("oMLX /api/status 缺少 loaded_models 与 loaded 字段，无法确定已加载模型列表");
    }
    if (!Array.isArray(raw)) {
      throw new Error(`oMLX /api/status 的已加载模型字段不是数组：${JSON.stringify(raw)}`);
    }
    return raw.filter((x): x is string => typeof x === "string");
  }

  /**
   * 解析 `/api/status` 的 `pressure` 字段（H3）。
   * - 缺失（undefined/null，例如本次复测用的实例没带 `--memory-guard` 启动）时返回
   *   显式的 `"unknown"`——**不是** fail-open 地当成 `"ok"`；`"unknown"` 的消费方必须按
   *   保守方向处理（见 `Pressure` 类型定义）。
   * - 值存在但不在 `ok/soft/hard/ceiling` 白名单里（类型不对或拼写变了）时**直接抛错**，
   *   不会被无脑 `as Pressure` 静默接受一个我们不认识的压力等级。
   */
  private parsePressure(value: unknown): Pressure {
    if (value === undefined || value === null) return "unknown";
    if (typeof value === "string" && (KNOWN_PRESSURE_VALUES as readonly string[]).includes(value)) {
      return value as Pressure;
    }
    throw new Error(`oMLX /api/status 返回了不认识的 pressure 值：${JSON.stringify(value)}`);
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
    if (!res.ok) throw new Error(`oMLX POST ${path} failed: HTTP ${res.status}`);
  }

  private async json(method: string, path: string, payload?: unknown): Promise<unknown> {
    const res = await this.fetchImpl(this.baseUrl + path, {
      method,
      headers: this.headers(),
      ...(payload === undefined ? {} : { body: JSON.stringify(payload) }),
    });
    if (!res.ok) throw new Error(`oMLX ${method} ${path} failed: HTTP ${res.status}`);
    return res.json();
  }
}

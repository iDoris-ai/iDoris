import { assertAdapterMountable, type AdapterManifest, type BaseIdentity } from "@idoris/contracts";

export interface ChatRequestLike {
  model: string;
  messages: ReadonlyArray<{ role: string; content: string }>;
}
export interface ChatResponseLike {
  model: string;
  content: string;
}
/** 真正干活的那一层：生产里是 Router 的后端 chat。 */
export type InferFn = (req: ChatRequestLike) => Promise<ChatResponseLike>;

export type MountErrorCode = "ADAPTER_ALREADY_MOUNTED" | "ADAPTER_NOT_MOUNTED";

export class MountError extends Error {
  constructor(
    readonly code: MountErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "MountError";
  }
}

interface Mounted {
  manifest: AdapterManifest;
  base: BaseIdentity;
}

/**
 * 热挂载表（T3.1.2）：Router 侧 adapter 的挂载点。
 *
 * 挂载时过 T3.2.1 门禁（base/tokenizer 指纹必须与当前加载的 base 一致），
 * 所以「换了个同名底座再挂老 adapter」在这里就被挡下；
 * 推理时**再校验一次**——挂载之后底座仍可能被换掉。
 */
export class AdapterMountTable {
  private readonly mounted = new Map<string, Mounted>();

  mount(input: unknown, base: BaseIdentity): AdapterManifest {
    const manifest = assertAdapterMountable(input, base);
    if (this.mounted.has(manifest.adapter_id)) {
      throw new MountError("ADAPTER_ALREADY_MOUNTED", "adapter " + manifest.adapter_id + " is already mounted");
    }
    this.mounted.set(manifest.adapter_id, { manifest, base: { ...base } });
    return manifest;
  }

  unmount(adapterId: string): boolean {
    return this.mounted.delete(adapterId);
  }

  has(adapterId: string): boolean {
    return this.mounted.has(adapterId);
  }

  list(): AdapterManifest[] {
    return [...this.mounted.values()].map((e) => e.manifest);
  }

  /** 用已挂载的 adapter 推理。adapter 不存在或底座已变 → 拒绝。 */
  async infer(
    adapterId: string,
    base: BaseIdentity,
    req: ChatRequestLike,
    infer: InferFn,
  ): Promise<ChatResponseLike> {
    const entry = this.mounted.get(adapterId);
    if (entry === undefined) {
      throw new MountError("ADAPTER_NOT_MOUNTED", "adapter " + adapterId + " is not mounted");
    }
    assertAdapterMountable(entry.manifest, base);
    return infer(req);
  }
}

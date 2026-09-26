import type { ComponentCard } from "@idoris/contracts";
import { MockBackend } from "../mock/mock-backend.js";
import { OmlxBackend } from "../omlx/omlx-backend.js";
import type { ModelBackend, ModelInfo } from "./backend.js";

/**
 * 组件卡 → 具体后端。**这是本仓唯一把 provider 名映射到引擎的层**；
 * Router 核心只调用本函数，因此 packages/router/src 里不出现任何引擎名。
 */
export function createBackend(card: ComponentCard): ModelBackend {
  const ext = (card.extensions ?? {}) as Record<string, unknown>;
  if (card.provider.id === "mock") {
    const mock = (ext.mock ?? {}) as { memoryMaxGb?: number; models?: ModelInfo[] };
    return new MockBackend({
      memoryMaxGb: mock.memoryMaxGb ?? 16,
      models: mock.models ?? [{ id: "mock-small", memoryGb: 2 }],
    });
  }
  if (card.provider.id === "omlx") {
    return new OmlxBackend({ baseUrl: card.endpoint });
  }
  throw new Error("no adapter for provider " + card.provider.id + " (form=" + card.form + ")");
}

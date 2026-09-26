import type { ComponentCard } from "@idoris/contracts";
import { MockBackend } from "../mock/mock-backend.js";
import { OmlxBackend } from "../omlx/omlx-backend.js";
import {
  createSubscriptionBackend,
  isSubscriptionProviderId,
} from "../subscription/registration.js";
import type { ModelBackend, ModelInfo } from "./backend.js";

/**
 * 组件卡 → 具体后端。**这是本仓唯一把 provider 名映射到引擎的层**；
 * Router 核心只调用本函数，因此 packages/router/src 里不出现任何引擎名。
 *
 * 订阅 provider 走 createSubscriptionBackend：没有 personal 部署 + 显式沙箱档
 * 就抛错，绝不静默无沙箱硬跑（T1.4.1 fail-closed）。
 */
export function createBackend(card: ComponentCard, env: NodeJS.ProcessEnv = process.env): ModelBackend {
  const ext = (card.extensions ?? {}) as Record<string, unknown>;
  if (isSubscriptionProviderId(card.provider.id)) {
    return createSubscriptionBackend(card, { env });
  }
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

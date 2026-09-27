import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { createBackend, isSubscriptionProviderId, type ModelBackend } from "@idoris/adapters";
import { validateComponentCard, type ComponentCard } from "@idoris/contracts";
import { parse } from "yaml";
import { subscriptionStartupGate } from "./egress-guard.js";

export interface Registered {
  card: ComponentCard;
  backend: ModelBackend;
}

export class ComponentRegistrationError extends Error {
  constructor(
    readonly code: "SUBSCRIPTION_FORBIDDEN",
    message: string,
  ) {
    super(message);
    this.name = "ComponentRegistrationError";
  }
}

export interface LoadComponentsOptions {
  env?: NodeJS.ProcessEnv;
}

/**
 * 从 config/components/*.yaml 加载组件卡；校验不过则抛错 → 启动失败。
 *
 * 订阅 provider 先过分层门禁（T1.4.2）：
 *   - 非 personal 部署 → 抛错（启动退出非 0）；
 *   - disabled / 未 enable → 跳过注册（fail-closed 默认）；
 *   - enable 但缺沙箱档 → createBackend 抛错（绝不无沙箱硬跑）。
 */
export function loadComponents(dir: string, opts: LoadComponentsOptions = {}): Registered[] {
  const env = opts.env ?? process.env;
  if (!statSync(dir).isDirectory()) throw new Error("components dir is not a directory: " + dir);
  const files = readdirSync(dir).filter((f) => f.endsWith(".yaml") || f.endsWith(".yml")).sort();
  const registered: Registered[] = [];
  for (const f of files) {
    const raw = parse(readFileSync(join(dir, f), "utf8")) as unknown;
    const card = validateComponentCard(raw);
    if (isSubscriptionProviderId(card.provider.id)) {
      const gate = subscriptionStartupGate(card.provider.id, env);
      if (gate.action === "refuse") {
        throw new ComponentRegistrationError("SUBSCRIPTION_FORBIDDEN", gate.reason);
      }
      if (gate.action === "skip") {
        console.warn("[idoris] subscription provider is not registered: " + gate.reason);
        continue;
      }
    }
    registered.push({ card, backend: createBackend(card, env) });
  }
  return registered;
}

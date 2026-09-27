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

/** M6 允许声明 locality: loopback 的 endpoint host（含 URL.hostname 对 IPv6 的加括号形式）。 */
const LOOPBACK_HOSTS: ReadonlySet<string> = new Set(["127.0.0.1", "::1", "[::1]", "localhost"]);

/**
 * M6：http_service 卡的 endpoint host 与 provider.locality 必须一致，否则拒绝注册（fail-closed）。
 *
 * 只管「声称 loopback 却不是」这个方向——这正是会让 X-iDoris-Served-Locality 谎报成
 * loopback、被 Agent24 的 idoris-local 误当本地放行的那类错误配置。
 *
 * 只校验 `http:`/`https:` 这两种真实网络地址；`mock://` 这类不代表真实网络端点的
 * 私有 scheme（纯内存 mock 后端，进程内计算，locality: loopback 本身没有说谎）
 * 以及 `form !== "http_service"`（如 spawn_cli 走子进程，不是网络地址）不在范围内。
 */
function assertEndpointLocalityConsistent(card: ComponentCard, file: string): void {
  if (card.form !== "http_service" || card.provider.locality !== "loopback") return;
  let parsed: URL;
  try {
    parsed = new URL(card.endpoint);
  } catch {
    return;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return;
  if (LOOPBACK_HOSTS.has(parsed.hostname)) return;
  throw new Error(
    `组件卡 "${card.provider.id}"（${file}）声明 provider.locality: loopback，` +
      `但 endpoint host 是 "${parsed.hostname}"，不是 127.0.0.1/::1/localhost。` +
      "两者不一致会导致 X-iDoris-Served-Locality 谎报——请把 endpoint 改成真实的 loopback 地址，或把 locality 改成 lan/remote。",
  );
}

/**
 * H1（PR #46 复审，真实复现）：`spawn_cli` 或订阅类 provider 不允许声明
 * `privacy_class: local_only`，也不允许 `tier: local`。
 *
 * 这类卡的推理实际发生在它背后的外部 CLI/服务里（`effectiveServedLocality`
 * 对它们一律判成 `remote`），如果同时声明 `local_only`/`tier: local`，
 * `dispatch.ts` 的 `isLocalCapable` 早先只看 `card.provider.locality` 时会把它
 * 当"可信本地"放行过 local_only 门禁——不需要 form 是 spawn_cli，
 * `provider.id === "subscription"` 的 `form: http_service` 卡一样会触发，因为
 * `isSubscriptionProviderId` 只认 id 不认 form。`isLocalCapable` 现在已经改
 * 用 `effectiveServedLocality`（运行时第二道防线），但从注册源头直接拒绝这种
 * 自相矛盾的卡更好——语义矛盾的配置不应该有机会启动。
 */
function assertNoContradictoryRelayClaim(card: ComponentCard, file: string): void {
  const isRelayLike = card.form === "spawn_cli" || isSubscriptionProviderId(card.provider.id);
  if (!isRelayLike) return;
  if (card.privacy_class !== "local_only" && card.provider.tier !== "local") return;
  throw new Error(
    `组件卡 "${card.provider.id}"（${file}）语义矛盾：` +
      (card.form === "spawn_cli" ? "form: spawn_cli" : "订阅类 provider（isSubscriptionProviderId）") +
      " 的推理实际发生在它背后的外部 CLI/服务里，不允许同时声明 privacy_class: local_only 或 tier: local——" +
      "那会让路由的 local_only 门禁把它当「可信本地」放行，实际却会把请求转发到外部。" +
      "如果这张卡确实需要本地语义，请改用真正本地的 http_service 后端。",
  );
}

/**
 * 从 config/components/*.yaml 加载组件卡；校验不过则抛错 → 启动失败。
 *
 * 订阅 provider 先过分层门禁（T1.4.2）：
 *   - 非 personal 部署 → 抛错（启动退出非 0）；
 *   - disabled / 未 enable → 跳过注册（fail-closed 默认）；
 *   - enable 但缺沙箱档 → createBackend 抛错（绝不无沙箱硬跑）。
 *
 * L5：同一个 provider.id 出现在两张卡里 → 拒绝启动（谁赢是未定义行为，不如直接报错）。
 */
export function loadComponents(dir: string, opts: LoadComponentsOptions = {}): Registered[] {
  const env = opts.env ?? process.env;
  if (!statSync(dir).isDirectory()) throw new Error("components dir is not a directory: " + dir);
  const files = readdirSync(dir).filter((f) => f.endsWith(".yaml") || f.endsWith(".yml")).sort();
  const registered: Registered[] = [];
  const seenIds = new Set<string>();
  for (const f of files) {
    const raw = parse(readFileSync(join(dir, f), "utf8")) as unknown;
    const card = validateComponentCard(raw);
    if (seenIds.has(card.provider.id)) {
      throw new Error(
        `组件卡 provider.id "${card.provider.id}" 重复声明（${f}）：每个 provider.id 只能有一张卡，` +
          "请检查 config/components 下是否有两个文件写了同一个 id。",
      );
    }
    seenIds.add(card.provider.id);
    assertEndpointLocalityConsistent(card, f);
    assertNoContradictoryRelayClaim(card, f);
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

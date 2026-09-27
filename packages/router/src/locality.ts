import { isSubscriptionProviderId } from "@idoris/adapters";
import type { ComponentCard } from "@idoris/contracts";

export type ServedLocality = "loopback" | "lan" | "remote";
export const SERVED_LOCALITY_VALUES: ReadonlySet<string> = new Set(["loopback", "lan", "remote"]);

/**
 * 唯一的"这张卡的推理实际会落在哪"判定。**路由门禁**（dispatch.ts 的
 * `isLocalCapable`/egress 计数）、**幂等缓存**（proxy.ts 的隔离与 fail-closed
 * 判据）、**响应头**（server.ts 的 `X-iDoris-Served-Locality`）三处必须用
 * 同一个函数——PR #46 复审 H1 真实复现的洞就是这三处曾经各算各的：
 *
 * dispatch.ts 的 `isLocalCapable` 只看 `card.provider.locality === "loopback"`，
 * server.ts 的响应头计算却对 `form === "spawn_cli"` / 订阅类 provider 一律
 * 判成 `remote`。结果：一张 schema 合法、`provider.id: subscription`（或任意
 * `form: spawn_cli`）却声明 `privacy_class: local_only` + `locality: loopback`
 * 的卡，会被 `isLocalCapable` 当成"可信本地"放行过 local_only 门禁，实际推理
 * 却在它背后的外部 CLI/服务里完成——**不需要 form 是 spawn_cli**，
 * `provider.id === "subscription"` 的 `form: http_service` 卡一样会触发（下面
 * 这个函数按 `isSubscriptionProviderId` 判定，不看 form），因为
 * `isSubscriptionProviderId` 本身就是只认 id、不认 form 的判定。
 *
 * `registry.ts` 的 `assertNoContradictoryRelayClaim` 从注册源头堵死这张矛盾卡
 * （spawn_cli/订阅类 provider 不允许声明 `privacy_class: local_only` 或
 * `tier: local`）；这个函数是运行时的第二道防线——即便未来有别的路径构造出
 * 类似的卡，三处判定依然会一致地把它算成 `remote`。
 *
 * fail-closed：locality 缺失或不是三值之一时按 `remote` 回报，不默认 `loopback`。
 */
export function effectiveServedLocality(card: ComponentCard): ServedLocality {
  if (card.form === "spawn_cli" || isSubscriptionProviderId(card.provider.id)) return "remote";
  const locality: unknown = card.provider.locality;
  return typeof locality === "string" && SERVED_LOCALITY_VALUES.has(locality) ? (locality as ServedLocality) : "remote";
}

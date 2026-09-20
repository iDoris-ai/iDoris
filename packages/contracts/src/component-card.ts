import { z } from "zod";
import { loadPolicySchema } from "./load-policy.js";
import { privacyClassSchema, providerDescriptorSchema } from "./provider.js";

/** 06 §3 六形态。 */
export const componentFormSchema = z.enum([
  "http_service", "spawn_cli", "bundled_binary", "nostr_node", "mitm_proxy", "batch_job",
]);
/** 允许的出站范围。 */
export const egressSchema = z.enum(["none", "loopback", "lan", "internet"]);
/** 失败时行为。 */
export const fallbackPolicySchema = z.enum(["fail_closed", "next_in_chain"]);

/**
 * 组件卡（06 §10.2）。强制策略字段缺失 → 不注册，启动即报错。
 * 交叉规则（privacy_class=local_only ⇒ fail_closed、tier=local+locality=remote 非法、
 * allowed_egress 含 internet 时 privacy_class 不得为 local_only）在 validate.ts 中强制。
 */
export const componentCardSchema = z
  .object({
    provider: providerDescriptorSchema,
    form: componentFormSchema,
    endpoint: z.string().min(1),
    version_pin: z.string().min(1),
    privacy_class: privacyClassSchema,
    allowed_egress: z.array(egressSchema).min(1),
    fallback_policy: fallbackPolicySchema,
    fail_closed: z.boolean(),
    load_policy: loadPolicySchema.optional(),
  })
  .strict();

export type ComponentCard = z.infer<typeof componentCardSchema>;

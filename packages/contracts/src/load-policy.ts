import { z } from "zod";

/** 常驻/临时/驱逐后加载 —— 引擎无关的抽象语义（06 §10.3）。 */
export const loadModeSchema = z.enum(["resident", "on_demand", "evict_to_load"]);
/** 能否与常驻模型共存。 */
export const admissionSchema = z.enum(["coexist", "requires_eviction"]);

const keepaliveSchema = z
  .object({
    /** 常驻：pinned=true，不进 EVICTING。 */
    pinned: z.boolean().optional(),
    /** 临时：idle 超时（秒）后被驱逐。 */
    idle_ttl_s: z.number().int().positive().optional(),
  })
  .strict()
  .refine((v) => v.pinned !== undefined || v.idle_ttl_s !== undefined, {
    message: "keepalive must set `pinned` or `idle_ttl_s`",
  });

/** LoadPolicy / ModelLease（06 §10.3）。 */
export const loadPolicySchema = z
  .object({
    mode: loadModeSchema,
    keepalive: keepaliveSchema,
    admission: admissionSchema,
  })
  .strict();

export type LoadPolicy = z.infer<typeof loadPolicySchema>;

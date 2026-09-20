import { z } from "zod";

/** 来源家族（06 §10.1）。 */
export const providerFamilySchema = z.enum(["idoris", "claude", "openai", "local", "other"]);
/** 路由层 tier —— 隐私判据（spec.md §数据模型）。 */
export const tierSchema = z.enum(["local", "remote", "lora"]);
/** L3 能力枚举（06 §2）。 */
export const capabilitySchema = z.enum([
  "chat", "reasoning", "vision", "asr", "tts", "coding", "embedding", "rerank",
]);
/** 该 provider 可承载的最高隐私级。 */
export const privacyClassSchema = z.enum(["local_only", "any"]);
/** 物理位置 —— 决定 tier=local 是否可信。 */
export const localitySchema = z.enum(["loopback", "lan", "remote"]);

/**
 * canonical ProviderDescriptor（06 §10.1）。
 * 路由/隐私/fallback/计费/发现读同一个描述符。
 */
export const providerDescriptorSchema = z
  .object({
    id: z.string().min(1),
    family: providerFamilySchema,
    tier: tierSchema,
    capabilities: z.array(capabilitySchema).min(1),
    privacy_class: privacyClassSchema,
    cost: z
      .object({
        input_per_m: z.number().finite().nonnegative(),
        output_per_m: z.number().finite().nonnegative(),
      })
      .strict(),
    locality: localitySchema,
  })
  .strict();

export type ProviderDescriptor = z.infer<typeof providerDescriptorSchema>;

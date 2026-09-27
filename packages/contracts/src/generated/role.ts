// GENERATED FROM packages/contracts/schema/role.schema.json — do not edit by hand.
import { z } from "zod"

export const roleSchema = z.enum(["fast","daily","deep","vision","embed","rerank","decide","auto"]).describe("idoris/<role> 稳定契约（docs/interfaces/iDoris-Agent24-边界与接口规范.md §3.3、§3.12）：fast=常驻 1-4B 延迟优先；daily=7-12B 质量优先（默认角色，旧名 core）；deep=complexity complex；vision/embed/rerank/decide 为专用能力；auto=交给 iDoris 选。")


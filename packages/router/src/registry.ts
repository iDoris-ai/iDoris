import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { createBackend, type ModelBackend } from "@idoris/adapters";
import { validateComponentCard, type ComponentCard } from "@idoris/contracts";
import { parse } from "yaml";

export interface Registered {
  card: ComponentCard;
  backend: ModelBackend;
}

/** 从 config/components/*.yaml 加载组件卡；校验不过则抛错 → 启动失败。 */
export function loadComponents(dir: string): Registered[] {
  if (!statSync(dir).isDirectory()) throw new Error("components dir is not a directory: " + dir);
  const files = readdirSync(dir).filter((f) => f.endsWith(".yaml") || f.endsWith(".yml")).sort();
  return files.map((f) => {
    const raw = parse(readFileSync(join(dir, f), "utf8")) as unknown;
    const card = validateComponentCard(raw);
    return { card, backend: createBackend(card) };
  });
}

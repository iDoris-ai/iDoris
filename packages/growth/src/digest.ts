import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";

export function sha256Of(data: Buffer | string): string {
  return "sha256:" + createHash("sha256").update(data).digest("hex");
}

/**
 * 目录指纹：按相对路径排序后把「路径 + 内容」依次喂进同一个 hash。
 * 顺序稳定 ⇒ 同名同内容的目录指纹稳定；权重换一个字节指纹就变。
 */
export function digestDirectory(dir: string): string {
  const hash = createHash("sha256");
  const walk = (d: string): string[] => {
    const out: string[] = [];
    for (const name of readdirSync(d).sort()) {
      const p = join(d, name);
      if (statSync(p).isDirectory()) out.push(...walk(p));
      else out.push(p);
    }
    return out;
  };
  for (const file of walk(dir).sort()) {
    hash.update(relative(dir, file).split(sep).join("/"));
    hash.update("\0");
    hash.update(readFileSync(file));
    hash.update("\0");
  }
  return "sha256:" + hash.digest("hex");
}

/** 多文件指纹（tokenizer 往往由若干文件共同决定）。 */
export function digestFiles(files: readonly string[]): string {
  const hash = createHash("sha256");
  for (const file of [...files].sort()) {
    hash.update(file);
    hash.update("\0");
    hash.update(readFileSync(file));
    hash.update("\0");
  }
  return "sha256:" + hash.digest("hex");
}

/** 模型目录里可能的 tokenizer 文件（MLX 布局不统一，所以按存在性收集）。 */
export const TOKENIZER_FILES = [
  "tokenizer.json",
  "tokenizer_config.json",
  "vocab.json",
  "merges.txt",
  "special_tokens_map.json",
] as const;

export function tokenizerFilesIn(modelDir: string): string[] {
  return TOKENIZER_FILES.map((f) => join(modelDir, f)).filter((p) => existsSync(p));
}

/** 基础模型权重文件（只看顶层 + 一层 safetensors/npz）。 */
export function weightFilesIn(modelDir: string): string[] {
  return readdirSync(modelDir, { withFileTypes: true })
    .filter((e) => e.isFile() && (e.name.endsWith(".safetensors") || e.name.endsWith(".npz")))
    .map((e) => join(modelDir, e.name))
    .sort();
}

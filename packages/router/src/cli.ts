#!/usr/bin/env node
import { serve } from "./serve.js";

/**
 * H1：之前这里用 `import.meta.url === pathToFileURL(process.argv[1]).href` 判断
 * "是不是被直接当入口跑起来"，想用它避免被 import 时产生副作用。但真实部署常见
 * 经软链调用（`node_modules/.bin/idoris-router` -> 本文件，或手工建的软链/
 * LaunchAgent 里配一条软链路径），此时 `import.meta.url` 是软链解析后的**真实
 * 文件路径**，而 `process.argv[1]` 是**调用时敲的软链路径**——两者永远不相等，
 * 判断结果恒为 false，`main()` 从不执行：整个服务静默退出、退出码还是 0，比
 * 直接报错更难排查。
 *
 * cli.ts 现在只做一件事——本来就是给「作为可执行文件跑」用的入口，不需要兼顾
 * "被 import 时不要有副作用"这个次要目标；真要在测试里复用逻辑，去 import
 * ./serve.ts（parsePort/serve 都在那边，本文件不再导出任何东西）。
 */
async function main(argv: string[], env: NodeJS.ProcessEnv): Promise<void> {
  const cmd = argv[2];
  if (cmd !== "serve") {
    console.error("用法：idoris-router serve");
    process.exitCode = 1;
    return;
  }
  await serve(env);
}

main(process.argv, process.env).catch((err: unknown) => {
  console.error("[idoris-router] 启动失败：" + (err instanceof Error ? err.message : String(err)));
  process.exit(1);
});

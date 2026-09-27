import { spawnSync } from "node:child_process";

export interface FlowerProbe {
  available: boolean;
  reason: string;
}

/**
 * 探测本机是否具备真 Flower 联邦后端（T3.3.1）。
 *
 * 如实说明范围：本包实现并验证的是**联邦协议与载荷契约**（两个本地客户端 + 合成数据
 * + 一轮 FedAvg + 「只传权重」的哨兵断言），跑的是同进程的本地 transport。
 * 真 Flower（跨进程 gRPC）后端需要 `fl` 与 `flwr`，探不到就 SKIPPED —— 不假装跑过。
 */
export function detectFlower(python = process.env.IDORIS_PYTHON ?? "python3"): FlowerProbe {
  const probe = spawnSync(python, ["-c", "import flwr"], { encoding: "utf8" });
  if (probe.status !== 0) {
    return { available: false, reason: "flwr not importable (pip install flwr)" };
  }
  return { available: true, reason: "ok" };
}

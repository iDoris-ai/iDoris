import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Socket } from "node:net";
import { request as httpRequest } from "node:http";
import { afterEach, describe, expect, it } from "vitest";
import { startRouter, type Router } from "../src/server.js";
import { installEgressProbe, type EgressProbe } from "./egress-probe.js";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const componentsDir = join(repoRoot, "config", "components");

/** TEST-NET-1 (RFC 5737)：保证永不可达，不会真的连上任何东西。 */
const UNREACHABLE = { socket: "192.0.2.1", fetch: "192.0.2.2", http: "192.0.2.3" };

let probe: EgressProbe | undefined;
let running: Router | undefined;

afterEach(async () => {
  if (running) await running.close();
  running = undefined;
  probe?.restore();
  probe = undefined;
});

describe("startup egress assertion (T1.3.6)", () => {
  it("Router makes ZERO non-loopback connections while serving /v1/models", async () => {
    probe = installEgressProbe();
    running = await startRouter({ componentsDir, port: 0 });
    await fetch("http://127.0.0.1:" + running.port + "/v1/models");
    await fetch("http://127.0.0.1:" + running.port + "/health");
    expect(probe.outbound).toEqual([]);
  });

  it("negative control: a loopback connection is NOT recorded", async () => {
    probe = installEgressProbe();
    running = await startRouter({ componentsDir, port: 0 });
    await fetch("http://127.0.0.1:" + running.port + "/health");
    expect(probe.outbound).toEqual([]);
  });
});

/**
 * 每条**真实调用方会用的路径**各一个正对照（评审 PR #23）。
 *
 * 原版只对 `new Socket().connect()` 做了正对照 —— 而那正是探针 patch 的那个 API，
 * 所以它只证明了「拦截逻辑对它自己有效」。上面那条零出网断言走的是 `fetch()`，
 * 它有没有被覆盖，原版没有任何断言。
 *
 * **这几条红了不代表 Router 出网了，代表探针失明了** —— 那时上面的零出网断言
 * 就不再有意义，必须先修探针。这个区分要写在测试名里，否则下一个人会以为
 * 是 Router 的问题。
 */
describe("PROBE COVERAGE positive controls — 每条真实路径都必须被抓到，否则探针失明", () => {
  it("path 1/3 · raw net.Socket.connect（探针直接 patch 的 API）", () => {
    probe = installEgressProbe();
    const s = new Socket();
    s.connect(80, UNREACHABLE.socket);
    s.destroy();
    expect(probe.outbound.some((h) => h.startsWith(UNREACHABLE.socket))).toBe(true);
  });

  it("path 2/3 · fetch()（undici —— 零出网断言实际走的就是这条，原版未覆盖）", async () => {
    probe = installEgressProbe();
    try {
      await fetch("http://" + UNREACHABLE.fetch + ":80/", { signal: AbortSignal.timeout(1500) });
    } catch {
      // 连不上是预期的；我们测的是「这次尝试有没有被探针看见」，不是它成不成功。
    }
    expect(
      probe.outbound.some((h) => h.startsWith(UNREACHABLE.fetch)),
      "fetch() 的连接尝试没有被探针抓到 —— 探针对 undici 路径失明，" +
        "此时「Router 零出网」这条断言不再承重，必须先修探针的拦截点。",
    ).toBe(true);
  });

  it("path 3/3 · node:http request（很多库直连走这条）", async () => {
    probe = installEgressProbe();
    await new Promise<void>((resolve) => {
      const rq = httpRequest({ host: UNREACHABLE.http, port: 80, timeout: 1000 }, () => resolve());
      rq.on("error", () => resolve());
      rq.on("timeout", () => {
        rq.destroy();
        resolve();
      });
      rq.end();
    });
    expect(
      probe.outbound.some((h) => h.startsWith(UNREACHABLE.http)),
      "http.request 的连接尝试没有被探针抓到 —— 探针对该路径失明。",
    ).toBe(true);
  });
});

import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Socket } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import { startRouter, type Router } from "../src/server.js";
import { installEgressProbe, type EgressProbe } from "./egress-probe.js";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const componentsDir = join(repoRoot, "config", "components");

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

  it("POSITIVE CONTROL: a deliberate outbound connection IS captured by the probe", () => {
    probe = installEgressProbe();
    const s = new Socket();
    s.connect(80, "192.0.2.1"); // TEST-NET-1: never actually reachable
    s.destroy();
    expect(probe.outbound.some((h) => h.startsWith("192.0.2.1"))).toBe(true);
  });

  it("negative control: a loopback connection is NOT recorded", async () => {
    probe = installEgressProbe();
    running = await startRouter({ componentsDir, port: 0 });
    await fetch("http://127.0.0.1:" + running.port + "/health");
    expect(probe.outbound).toEqual([]);
  });
});

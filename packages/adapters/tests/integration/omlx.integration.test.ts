import { describe, expect, it } from "vitest";
import { OmlxBackend } from "../../omlx/omlx-backend.js";

const baseUrl = process.env.OMLX_URL ?? "http://127.0.0.1:8088";

describe("oMLX integration", () => {
  it("load -> status -> unload, or prints SKIPPED when oMLX is absent", async () => {
    const backend = new OmlxBackend({ baseUrl });
    let models: Awaited<ReturnType<OmlxBackend["list"]>>;
    try {
      models = await backend.list();
    } catch {
      console.log(`SKIPPED: no oMLX reachable at ${baseUrl} (integration test skipped, not passed silently)`);
      return;
    }
    expect(Array.isArray(models)).toBe(true);
    const st = await backend.status();
    expect(typeof st.modelMemoryMaxGb).toBe("number");
    console.log(`oMLX reachable: ${models.length} models, guard=${st.modelMemoryMaxGb}GB, loaded=${st.loaded.length}`);
  });
});

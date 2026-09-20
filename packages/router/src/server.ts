import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { HealthTracker } from "./health.js";
import { loadComponents, type Registered } from "./registry.js";

export interface RouterOptions {
  componentsDir: string;
  port?: number;
  health?: HealthTracker;
}

export interface Router {
  server: Server;
  registered: Registered[];
  port: number;
  host: string;
  health: HealthTracker;
  close(): Promise<void>;
}

/** 绑定点硬编码 loopback（涉安全）：不监听非本机地址。 */
const BIND_HOST = "127.0.0.1";

function json(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

export async function startRouter(opts: RouterOptions): Promise<Router> {
  const registered = loadComponents(opts.componentsDir);
  const health = opts.health ?? new HealthTracker();
  const server = createServer((req, res) => {
    void handle(req, res, registered, health);
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(opts.port ?? 0, BIND_HOST, () => resolve());
  });
  const addr = server.address();
  const port = typeof addr === "object" && addr !== null ? addr.port : (opts.port ?? 0);
  return {
    server,
    registered,
    port,
    host: BIND_HOST,
    health,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

async function handle(
  req: IncomingMessage,
  res: ServerResponse,
  registered: Registered[],
  health: HealthTracker,
): Promise<void> {
  if (req.method === "GET" && req.url === "/health") {
    json(res, 200, { status: "ok", components: registered.length });
    return;
  }
  if (req.method === "GET" && req.url === "/v1/models") {
    const data: Array<{ id: string; object: string; owned_by: string }> = [];
    for (const { card, backend } of registered) {
      if (health.isCoolingDown(card.provider.id)) continue;
      try {
        const models = await backend.list();
        health.record(card.provider.id, true);
        for (const m of models) data.push({ id: m.id, object: "model", owned_by: card.provider.id });
      } catch {
        health.record(card.provider.id, false);
      }
    }
    json(res, 200, { object: "list", data });
    return;
  }
  json(res, 404, { error: { type: "not_found" } });
}

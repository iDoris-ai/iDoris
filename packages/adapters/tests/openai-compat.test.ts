import { describe, expect, it, vi } from "vitest";
import { OpenAiCompatBackend } from "../openai-compat/openai-compat-backend.js";

const json = (body: unknown, status = 200) => ({ status, ok: status < 400, json: async () => body });

describe("OpenAiCompatBackend (T2.5.1)", () => {
  it("lists models from /v1/models and strips a trailing slash", async () => {
    const f = vi.fn(async (_url: string) => json({ data: [{ id: "gpt-x" }] }));
    const b = new OpenAiCompatBackend({ baseUrl: "https://api.example.com/", fetchImpl: f as never });
    expect((await b.list()).map((m) => m.id)).toEqual(["gpt-x"]);
    expect(f.mock.calls[0]?.[0]).toBe("https://api.example.com/v1/models");
  });
  it("maps chat and sends the bearer key", async () => {
    const calls: Array<{ url: string; auth?: string }> = [];
    const f = vi.fn(async (url: string, init?: { headers?: Record<string, string> }) => {
      const auth = init?.headers?.authorization;
      calls.push(auth === undefined ? { url } : { url, auth });
      return json({ choices: [{ message: { content: "hi" } }] });
    });
    const b = new OpenAiCompatBackend({ baseUrl: "https://api.example.com", apiKey: "sk-1", fetchImpl: f as never });
    expect((await b.chat({ model: "m", messages: [] })).content).toBe("hi");
    expect(calls[0]?.auth).toBe("Bearer sk-1");
    expect(calls[0]?.url).toBe("https://api.example.com/v1/chat/completions");
  });
  it("forwards the abort signal upstream", async () => {
    let seen: AbortSignal | undefined;
    const f = vi.fn(async (_url: string, init?: { signal?: AbortSignal }) => {
      seen = init?.signal;
      return json({ choices: [{ message: { content: "hi" } }] });
    });
    const ac = new AbortController();
    const b = new OpenAiCompatBackend({ baseUrl: "https://api.example.com", fetchImpl: f as never });
    await b.chat({ model: "m", messages: [], signal: ac.signal });
    expect(seen).toBe(ac.signal);
  });
  it("throws on non-2xx", async () => {
    const f = vi.fn(async (_url: string) => json({}, 401));
    const b = new OpenAiCompatBackend({ baseUrl: "https://api.example.com", fetchImpl: f as never });
    await expect(b.list()).rejects.toThrow(/HTTP 401/);
  });
  it("is a no-op backend for local scheduling", async () => {
    const b = new OpenAiCompatBackend({ baseUrl: "https://api.example.com", fetchImpl: vi.fn() as never });
    await expect(b.load("m")).resolves.toBeUndefined();
    await expect(b.admission("m")).resolves.toBe("coexist");
    expect((await b.status()).loaded).toEqual([]);
  });
});

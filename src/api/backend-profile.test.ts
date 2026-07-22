import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  connectRemoteBackend,
  disconnectRemoteBackend,
  getBackendProfile,
} from "./backend-profile";

describe("remote backend profile", () => {
  beforeEach(() => {
    disconnectRemoteBackend();
    vi.clearAllMocks();
    vi.unstubAllGlobals();
    window.localStorage.clear();
  });

  it("verifies the authenticated role before activating a normalized remote profile", async () => {
    const storageSpy = vi.spyOn(Storage.prototype, "setItem");
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      expect(url).toBe("https://samples.example.com/api/v1/session");
      expect(new Headers(init?.headers).get("Authorization")).toBe("Bearer secret-token");
      return new Response(
        JSON.stringify({ ok: true, data: { role: "editor" }, requestId: "request-1" }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    });
    vi.stubGlobal("fetch", fetchMock);

    const profile = await connectRemoteBackend(
      "https://samples.example.com/api/v1/",
      "secret-token",
    );

    expect(profile).toEqual({
      mode: "remote",
      baseUrl: "https://samples.example.com",
      apiBaseUrl: "https://samples.example.com/api/v1",
      token: "secret-token",
      role: "editor",
    });
    expect(getBackendProfile()).toEqual(profile);
    expect(storageSpy).not.toHaveBeenCalled();
  });

  it("disconnects without retaining the remote token", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        new Response(JSON.stringify({ ok: true, data: { role: "reader" } }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
      ),
    );
    await connectRemoteBackend("http://127.0.0.1:17311", "temporary-token");

    disconnectRemoteBackend();

    expect(getBackendProfile()).toEqual({ mode: "local" });
    expect(JSON.stringify(window.localStorage)).not.toContain("temporary-token");
  });
});

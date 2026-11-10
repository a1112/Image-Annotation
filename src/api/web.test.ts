import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  createWebDataProjectTransform,
  exportWebDataProject,
  getWebApiBaseUrl,
  getWebImageTransform,
  getWebImageTransformDownloadUrl,
  getWebImageContentUrl,
  getWebSession,
  invokeWebCommand,
  listWebDataProjectImages,
  listWebDataProjectTransforms,
  resolveWebResourceUrl,
  uploadWebDatasetImages,
} from "./web";
import { SESSION_CHANGE_EVENT } from "../shared/sessionEvents";

function jsonResponse(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

describe("web transport", () => {
  beforeEach(() => {
    vi.unstubAllEnvs();
    window.history.replaceState(null, "", "/image-annotation/app/");
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.unstubAllEnvs();
  });

  it("derives a same-origin API base by removing the deployment /app suffix", () => {
    expect(getWebApiBaseUrl()).toBe("http://localhost:3000/image-annotation");
  });

  it("keeps root-relative media URLs under the configured deployment prefix", () => {
    vi.stubEnv("VITE_REMOTE_API_URL", "https://workspace.example.test/image-annotation/");
    expect(resolveWebResourceUrl("/api/v1/web/projects/project-1/objects/hash")).toBe("https://workspace.example.test/image-annotation/api/v1/web/projects/project-1/objects/hash");
    expect(getWebImageContentUrl("project-1::road", "image-1")).toBe("https://workspace.example.test/image-annotation/api/v1/web/datasets/project-1%3A%3Aroad/images/image-1/content");
    expect(resolveWebResourceUrl("https://cdn.example.test/object")).toBe("https://cdn.example.test/object");
  });

  it("honors an explicit remote base and sends the DTO command with cookies", async () => {
    vi.stubEnv("VITE_REMOTE_API_URL", "https://workspace.example.test/image-annotation/");
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({ ok: true, data: { accepted: true } }), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(invokeWebCommand("save_image_annotations", { projectId: "p1", imageId: "i1" })).resolves.toEqual({ accepted: true });
    expect(fetchMock).toHaveBeenCalledWith(
      "https://workspace.example.test/image-annotation/api/v1/web/commands/save_image_annotations",
      expect.objectContaining({
        method: "POST",
        credentials: "include",
        body: JSON.stringify({ projectId: "p1", imageId: "i1" }),
      }),
    );
  });

  it("refreshes once after a protected 401 and retries the original request once", async () => {
    vi.stubEnv("VITE_REMOTE_API_URL", "https://workspace.example.test/image-annotation/");
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith("/api/v1/web/commands/list_data_projects")) {
        const attempts = fetchMock.mock.calls.filter(([call]) => String(call).endsWith("/api/v1/web/commands/list_data_projects")).length;
        return attempts === 1
          ? jsonResponse({ message: "expired" }, 401)
          : jsonResponse({ ok: true, data: { items: [] } });
      }
      if (url.endsWith("/api/v1/web/auth/refresh")) {
        return jsonResponse({ authenticated: true, user: { id: "user-1" }, deviceId: "device-1" });
      }
      throw new Error(`unexpected request ${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);

    await expect(invokeWebCommand("list_data_projects")).resolves.toEqual({ items: [] });
    expect(fetchMock).toHaveBeenCalledTimes(3);
    expect(fetchMock.mock.calls.map(([input]) => String(input))).toEqual([
      "https://workspace.example.test/image-annotation/api/v1/web/commands/list_data_projects",
      "https://workspace.example.test/image-annotation/api/v1/web/auth/refresh",
      "https://workspace.example.test/image-annotation/api/v1/web/commands/list_data_projects",
    ]);
  });

  it("shares one refresh promise across parallel expired protected requests", async () => {
    vi.stubEnv("VITE_REMOTE_API_URL", "https://workspace.example.test/image-annotation/");
    let protected401Count = 0;
    let releaseInitial401!: () => void;
    const initial401Released = new Promise<void>((resolve) => { releaseInitial401 = resolve; });
    let refreshCalls = 0;
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith("/api/v1/web/commands/list_data_projects")) {
        protected401Count += 1;
        if (protected401Count <= 2) {
          if (protected401Count === 2) releaseInitial401();
          await initial401Released;
          return jsonResponse({ message: "expired" }, 401);
        }
        return jsonResponse({ ok: true, data: { request: protected401Count } });
      }
      if (url.endsWith("/api/v1/web/auth/refresh")) {
        refreshCalls += 1;
        return jsonResponse({ authenticated: true, user: { id: "user-1" }, deviceId: "device-1" });
      }
      throw new Error(`unexpected request ${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);

    await expect(Promise.all([
      invokeWebCommand("list_data_projects", { page: 1 }),
      invokeWebCommand("list_data_projects", { page: 2 }),
    ])).resolves.toEqual([{ request: 3 }, { request: 4 }]);
    expect(refreshCalls).toBe(1);
    expect(fetchMock).toHaveBeenCalledTimes(5);
  });

  it("getWebSession refreshes an expired access cookie before reporting the session", async () => {
    vi.stubEnv("VITE_REMOTE_API_URL", "https://workspace.example.test/image-annotation/");
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith("/api/v1/web/auth/session")) {
        const sessionAttempts = fetchMock.mock.calls.filter(([call]) => String(call).endsWith("/api/v1/web/auth/session")).length;
        return sessionAttempts === 1
          ? jsonResponse({ message: "expired" }, 401)
          : jsonResponse({ authenticated: true, user: { id: "user-1", email: "user@example.test" }, deviceId: "device-1" });
      }
      if (url.endsWith("/api/v1/web/auth/refresh")) {
        return jsonResponse({ authenticated: true, user: { id: "user-1" }, deviceId: "device-1" });
      }
      throw new Error(`unexpected request ${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);

    await expect(getWebSession()).resolves.toMatchObject({ authenticated: true, user: { id: "user-1" } });
    expect(fetchMock.mock.calls.map(([input]) => String(input))).toEqual([
      "https://workspace.example.test/image-annotation/api/v1/web/auth/session",
      "https://workspace.example.test/image-annotation/api/v1/web/auth/refresh",
      "https://workspace.example.test/image-annotation/api/v1/web/auth/session",
    ]);
  });

  it("broadcasts account invalidation once when the shared refresh fails and never loops", async () => {
    vi.stubEnv("VITE_REMOTE_API_URL", "https://workspace.example.test/image-annotation/");
    const sessionChanges: Event[] = [];
    const onSessionChange = (event: Event) => sessionChanges.push(event);
    window.addEventListener(SESSION_CHANGE_EVENT, onSessionChange);
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith("/api/v1/web/commands/list_data_projects")) return jsonResponse({ message: "expired" }, 401);
      if (url.endsWith("/api/v1/web/auth/refresh")) return jsonResponse({ message: "refresh expired" }, 401);
      throw new Error(`unexpected request ${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);

    try {
      await expect(invokeWebCommand("list_data_projects")).rejects.toMatchObject({ status: 401 });
      expect(fetchMock).toHaveBeenCalledTimes(2);
      expect(sessionChanges).toHaveLength(1);
      expect((sessionChanges[0] as CustomEvent<{ authenticated: boolean }>).detail).toEqual({ authenticated: false });
    } finally {
      window.removeEventListener(SESSION_CHANGE_EVENT, onSessionChange);
    }
  });

  it("uses the authenticated image-transform routes and preserves the server task DTO", async () => {
    vi.stubEnv("VITE_REMOTE_API_URL", "https://workspace.example.test");
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith("/api/v1/data-projects/project-1/image-transforms")) {
        expect(init?.method).toBe("POST");
        expect(init?.body).toBe(JSON.stringify({
          operationId: "op-1",
          sourceHash: "a".repeat(64),
          datasetKey: "road",
          format: "png",
          rotate: 0,
          flip: "none",
        }));
        return new Response(JSON.stringify({ taskId: "task-1", status: "queued", created: true, pollUrl: "/api/v1/image-transforms/task-1" }), { status: 202 });
      }
      if (url.endsWith("/api/v1/data-projects/project-1/image-transforms?datasetKey=road&limit=50")) {
        return new Response(JSON.stringify([{ taskId: "task-1", status: "succeeded", sourceHash: "a".repeat(64) }]), { status: 200 });
      }
      return new Response(JSON.stringify({ taskId: "task-1", status: "succeeded", outputHash: "b".repeat(64), outputMimeType: "image/png", downloadUrl: "/api/v1/image-transforms/task-1/download" }), { status: 200 });
    });
    vi.stubGlobal("fetch", fetchMock);

    await expect(createWebDataProjectTransform("project-1", {
      operationId: "op-1",
      sourceHash: "a".repeat(64),
      datasetKey: "road",
      format: "png",
      rotate: 0,
      flip: "none",
    })).resolves.toMatchObject({ taskId: "task-1", status: "queued", created: true });
    await expect(getWebImageTransform("task-1")).resolves.toMatchObject({ taskId: "task-1", status: "succeeded", outputHash: "b".repeat(64), downloadUrl: "https://workspace.example.test/api/v1/image-transforms/task-1/download" });
    await expect(listWebDataProjectTransforms("project-1", "road")).resolves.toEqual([
      expect.objectContaining({ taskId: "task-1", status: "succeeded", sourceHash: "a".repeat(64) }),
    ]);
    expect(getWebImageTransformDownloadUrl("task-1")).toBe("https://workspace.example.test/api/v1/image-transforms/task-1/download");
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });

  it("normalizes server media URLs in image and upload DTOs", async () => {
    vi.stubEnv("VITE_REMOTE_API_URL", "https://workspace.example.test/image-annotation/");
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith("/api/v1/web/commands/list_data_project_images")) {
        return new Response(JSON.stringify({
          ok: true,
          data: {
            items: [{
              id: "image-1",
              fileName: "road.png",
              width: 100,
              height: 80,
              split: "train",
              status: "raw",
              qaStatus: "pending",
              reviewNote: null,
              tags: [],
              imageHash: "a".repeat(64),
              imageUrl: "/api/v1/web/projects/project-1/objects/a",
              annotationUrl: "/api/v1/web/projects/project-1/objects/b",
            }],
            nextCursor: null,
          },
        }), { status: 200 });
      }
      return new Response(JSON.stringify({
        ok: true,
        data: {
          imageUrl: "/api/v1/web/projects/project-1/objects/a",
          annotationUrl: "/api/v1/web/projects/project-1/objects/b",
        },
      }), { status: 200 });
    });
    vi.stubGlobal("fetch", fetchMock);

    await expect(listWebDataProjectImages("project-1::road")).resolves.toMatchObject({
      items: [{
        imageUrl: "https://workspace.example.test/image-annotation/api/v1/web/projects/project-1/objects/a",
        annotationUrl: "https://workspace.example.test/image-annotation/api/v1/web/projects/project-1/objects/b",
      }],
    });
    await expect(uploadWebDatasetImages("project-1::road", [new File(["bytes"], "road.png", { type: "image/png" })])).resolves.toEqual([{
      ok: true,
      data: {
        imageUrl: "https://workspace.example.test/image-annotation/api/v1/web/projects/project-1/objects/a",
        annotationUrl: "https://workspace.example.test/image-annotation/api/v1/web/projects/project-1/objects/b",
      },
    }]);
  });

  it("exports a server project manifest through the web command surface", async () => {
    vi.stubEnv("VITE_REMOTE_API_URL", "https://workspace.example.test/image-annotation/");
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      expect(String(input)).toBe("https://workspace.example.test/image-annotation/api/v1/web/commands/export_data_project");
      expect(init?.body).toBe(JSON.stringify({ dataProjectId: "project-1", branch: null, format: "manifest-json" }));
      return new Response(JSON.stringify({
        ok: true,
        data: {
          id: "export-1",
          dataProjectId: "project-1",
          branch: "main",
          format: "manifest-json",
          status: "succeeded",
          manifest: { tree: { entries: [{ fileName: "road.png", annotation: [] }] } },
        },
      }), { status: 200 });
    });
    vi.stubGlobal("fetch", fetchMock);

    await expect(exportWebDataProject("project-1")).resolves.toMatchObject({
      id: "export-1",
      dataProjectId: "project-1",
      branch: "main",
      status: "succeeded",
      manifest: { tree: { entries: [{ fileName: "road.png", annotation: [] }] } },
    });
  });
});

import { beforeEach, describe, expect, it, vi } from "vitest";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import {
  BackendRevisionConflictError,
  detectBackendConnection,
  getFileAssetUrl,
  listClassSamples,
  listDatasetProjects,
  openAnnotationWindow,
  syncDatasetSource,
  saveImageAnnotations,
  uploadRemoteImport,
  commitRemoteImport,
} from "./tauri";
import { connectRemoteBackend, disconnectRemoteBackend } from "./backend-profile";
import { invoke } from "@tauri-apps/api/core";

vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: (path: string) => `asset://${path}`,
  invoke: vi.fn(async () => {
    throw new TypeError("Cannot read properties of undefined (reading 'invoke')");
  }),
}));

describe("backend fallback", () => {
  beforeEach(() => {
    disconnectRemoteBackend();
    vi.clearAllMocks();
    vi.unstubAllGlobals();
  });

  it("远程 profile 使用 REST、Bearer token 与远程资源地址", async () => {
    const requests: Array<{ url: string; init?: RequestInit }> = [];
    const createObjectUrl = vi.fn(() => "blob:remote-image");
    Object.defineProperty(URL, "createObjectURL", {
      configurable: true,
      value: createObjectUrl,
    });
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        requests.push({ url, init });
        if (url.endsWith("/session")) {
          return new Response(JSON.stringify({ ok: true, data: { role: "reader" } }), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });
        }
        if (url.endsWith("/projects")) {
          return new Response(JSON.stringify({ ok: true, data: [] }), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });
        }
        return new Response("asset", { status: 200 });
      }),
    );
    await connectRemoteBackend("https://samples.example.com", "reader-token");

    expect(await listDatasetProjects()).toEqual([]);
    expect(await getFileAssetUrl("project one", "image/1")).toBe("blob:remote-image");
    expect(requests[1].url).toBe("https://samples.example.com/api/v1/projects");
    expect(new Headers(requests[1].init?.headers).get("Authorization")).toBe(
      "Bearer reader-token",
    );
    expect(requests[2].url).toBe(
      "https://samples.example.com/api/v1/projects/project%20one/samples/image%2F1/content",
    );
    expect(new Headers(requests[2].init?.headers).get("Authorization")).toBe(
      "Bearer reader-token",
    );
    expect(invoke).not.toHaveBeenCalledWith("list_dataset_projects");
  });

  it("远程注释版本冲突转换为专用错误", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url.endsWith("/session")) {
          return new Response(JSON.stringify({ ok: true, data: { role: "editor" } }), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });
        }
        return new Response(
          JSON.stringify({
            ok: false,
            error: { code: "revision_conflict", message: "annotation revision has changed" },
            requestId: "request-conflict",
          }),
          { status: 409, headers: { "Content-Type": "application/json" } },
        );
      }),
    );
    await connectRemoteBackend("https://samples.example.com", "editor-token");

    await expect(saveImageAnnotations("project", "sample", "revision-1", [])).rejects.toBeInstanceOf(
      BackendRevisionConflictError,
    );
  });

  it("远程导入使用 multipart 分析并显式确认格式", async () => {
    const requests: Array<{ url: string; init?: RequestInit }> = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        requests.push({ url, init });
        if (url.endsWith("/session")) {
          return new Response(JSON.stringify({ ok: true, data: { role: "editor" } }), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });
        }
        return new Response(
          JSON.stringify({
            ok: true,
            data: {
              id: "import-1",
              projectId: "project-1",
              state: url.endsWith("/commit") ? "completed" : "analyzed",
              detectedFormat: "yolo-detect",
              imageCount: 1,
              annotationCount: 1,
              classCount: 1,
              classes: ["object"],
              warnings: [],
              problems: [],
              tree: [],
              bytesReceived: 12,
              fileCount: 2,
              errorMessage: null,
              createdAt: "now",
              updatedAt: "now",
            },
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }),
    );
    await connectRemoteBackend("https://samples.example.com", "editor-token");
    const image = new File(["image"], "sample.png", { type: "image/png" });

    const analyzed = await uploadRemoteImport("project-1", [image]);
    const committed = await commitRemoteImport(analyzed.id, "yolo-detect");

    expect(requests[1].url).toBe(
      "https://samples.example.com/api/v1/projects/project-1/imports",
    );
    expect(requests[1].init?.body).toBeInstanceOf(FormData);
    expect(new Headers(requests[1].init?.headers).has("Content-Type")).toBe(false);
    expect(requests[2].url).toBe(
      "https://samples.example.com/api/v1/imports/import-1/commit",
    );
    expect(JSON.parse(String(requests[2].init?.body))).toEqual({ format: "yolo-detect" });
    expect(committed.state).toBe("completed");
  });

  it("按类别样本查询调用真实后端命令", async () => {
    vi.mocked(invoke).mockResolvedValueOnce([]);

    const samples = await listClassSamples("coco128", {
      classId: 0,
      label: "person",
      offset: 0,
      limit: 48,
    });

    expect(samples).toEqual([]);
    expect(invoke).toHaveBeenCalledWith("list_class_samples", {
      projectId: "coco128",
      classId: 0,
      label: "person",
      offset: 0,
      limit: 48,
    });
  });

  it("COCO 源同步调用数据集级后端命令", async () => {
    vi.mocked(invoke).mockResolvedValueOnce({
      path: "F:/datasets/annotations.json",
      sourceVersion: "100:200",
    });

    const result = await syncDatasetSource("linked-coco");

    expect(result.sourceVersion).toBe("100:200");
    expect(invoke).toHaveBeenCalledWith("sync_dataset_source", {
      projectId: "linked-coco",
    });
  });

  it("普通浏览器能检测已启动的 Tauri 桌面后台", async () => {
    const fetchMock = vi.fn(async (url: string) => {
      expect(url).toBe("http://127.0.0.1:17310/api/health");
      return new Response(
        JSON.stringify({
          ok: true,
          data: {
            status: "ok",
            service: "image-annotation-rust-backend",
            version: "0.1.0",
            runtime: "tauri-desktop",
            capabilities: ["datasets", "assets", "annotations", "windows", "tasks"],
          },
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    });
    vi.stubGlobal("fetch", fetchMock);

    const connection = await detectBackendConnection();

    expect(invoke).toHaveBeenCalledWith("backend_health");
    expect(connection.mode).toBe("web-local-desktop");
    expect(connection.label).toBe("已连接桌面后台");
    expect(connection.health?.capabilities).toContain("windows");
  });

  it("普通浏览器缺少 Tauri invoke 时使用本地 Rust HTTP 后端", async () => {
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      expect(url).toBe("http://127.0.0.1:17310/api/invoke/list_dataset_projects");
      expect(init?.method).toBe("POST");
      return new Response(
        JSON.stringify({
          ok: true,
          data: [
            {
              id: "local-out",
              name: "本机 out",
              description: "本机数据集",
              annotationTypes: ["BBox"],
              imageCount: 1,
              annotatedPercent: 0,
              reviewCount: 0,
              issueCount: 0,
              classCount: 1,
              tagGroupCount: 1,
              status: "已导入",
              tags: ["source: local-linked"],
            },
          ],
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    });
    vi.stubGlobal("fetch", fetchMock);

    const projects = await listDatasetProjects();

    expect(invoke).toHaveBeenCalledWith("list_dataset_projects");
    expect(projects[0].id).toBe("local-out");
  });

  it("普通浏览器缺少 Tauri asset 协议时使用 Rust 图片代理地址", async () => {
    const fetchMock = vi.fn(async () => {
      throw new Error("should not fetch for asset url construction");
    });
    vi.stubGlobal("fetch", fetchMock);

    const url = await getFileAssetUrl("local-out", "img-1");

    expect(url).toBe("http://127.0.0.1:17310/api/assets/local-out/img-1");
  });

  it("普通浏览器打开标注窗口时通过本地桌面后台转发", async () => {
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      expect(url).toBe("http://127.0.0.1:17310/api/invoke/open_annotation_window");
      expect(init?.method).toBe("POST");
      expect(JSON.parse(String(init?.body))).toEqual({
        projectId: "coco128",
        imageId: "000000000009",
      });
      return new Response(JSON.stringify({ ok: true, data: null }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    });
    vi.stubGlobal("fetch", fetchMock);

    await openAnnotationWindow("coco128", "000000000009");

    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("Tauri dev 启动时不先占用 standalone 后台端口", async () => {
    const config = JSON.parse(
      await readFile(resolve(process.cwd(), "src-tauri/tauri.conf.json"), "utf-8"),
    );

    expect(config.build.beforeDevCommand).toContain("vite");
    expect(config.build.beforeDevCommand).not.toContain("npm run dev");
    expect(config.build.beforeDevCommand).not.toContain("dev-with-backend");
  });
});

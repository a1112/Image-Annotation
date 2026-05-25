import { beforeEach, describe, expect, it, vi } from "vitest";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { detectBackendConnection, getFileAssetUrl, listClassSamples, listDatasetProjects, openAnnotationWindow } from "./tauri";
import { invoke } from "@tauri-apps/api/core";

vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: (path: string) => `asset://${path}`,
  invoke: vi.fn(async () => {
    throw new TypeError("Cannot read properties of undefined (reading 'invoke')");
  }),
}));

describe("backend fallback", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.unstubAllGlobals();
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

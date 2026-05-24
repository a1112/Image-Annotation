import { invoke } from "@tauri-apps/api/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";

const tauriState = vi.hoisted(() => ({
  backendAvailable: true,
  builtinDownloaded: true,
}));

vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: (path: string) => `asset://${path}`,
  invoke: vi.fn(async (command: string, args?: Record<string, unknown>) => {
    if (!tauriState.backendAvailable) {
      throw new TypeError("Cannot read properties of undefined (reading 'invoke')");
    }

    if (command === "list_builtin_datasets") {
      return [
        {
          key: "coco128",
          name: "COCO128",
          description: "128 张 COCO 训练图片",
          taskType: "目标检测",
          format: "yolo-detect",
          downloaded: tauriState.builtinDownloaded,
          projectId: tauriState.builtinDownloaded ? "coco128" : null,
        },
      ];
    }

    if (command === "backend_health") {
      return {
        status: "ok",
        service: "image-annotation-rust-backend",
        version: "0.1.0",
        runtime: "tauri-desktop",
        capabilities: ["datasets", "assets", "annotations", "windows", "tray", "tasks"],
      };
    }

    if (command === "list_dataset_projects") {
      if (!tauriState.builtinDownloaded) {
        return [];
      }
      return [
        {
          id: "coco128",
          name: "COCO128",
          description: "真实 COCO128 测试数据集",
          annotationTypes: ["BBox"],
          imageCount: 128,
          annotatedPercent: 100,
          reviewCount: 0,
          issueCount: 0,
          classCount: 80,
          tagGroupCount: 3,
          status: "已导入",
          tags: ["source: ultralytics", "format: yolo-detect", "split: train"],
        },
      ];
    }

    if (command === "get_project_detail") {
      return {
        project: {
          id: "coco128",
          name: "COCO128",
          description: "真实 COCO128 测试数据集",
          annotationTypes: ["BBox"],
          imageCount: 128,
          annotatedPercent: 100,
          reviewCount: 0,
          issueCount: 0,
          classCount: 80,
          tagGroupCount: 3,
          status: "已导入",
          tags: ["source: ultralytics", "format: yolo-detect", "split: train"],
        },
        tagGroups: [
          {
            id: "train",
            name: "train",
            conditions: ["split=train"],
            imageCount: 128,
            annotatedPercent: 100,
            issueCount: 0,
            exportEnabled: true,
          },
        ],
        classes: [{ label: "person", color: "#cc54d8", count: 12, attributes: [] }],
        tasks: [],
        qualityChecks: [],
        exportPresets: [],
      };
    }

    if (command === "list_project_images") {
      const imageIds = [
        "000000000009",
        "000000000025",
        "000000000030",
        "000000000034",
        "000000000036",
        "000000000042",
        "000000000049",
        "000000000061",
        "000000000064",
        "000000000071",
        "000000000072",
        "000000000073",
        "000000000074",
        "000000000077",
        "000000000078",
        "000000000081",
        "000000000086",
        "000000000089",
        "000000000092",
        "000000000094",
        "000000000109",
        "000000000110",
        "000000000113",
        "000000000127",
        "000000000133",
        "000000000136",
        "000000000138",
        "000000000142",
        "000000000143",
        "000000000144",
        "000000000149",
        "000000000151",
        "000000000154",
        "000000000164",
        "000000000165",
        "000000000192",
        "000000000194",
        "000000000196",
        "000000000201",
        "000000000208",
        "000000000241",
        "000000000247",
        "000000000250",
        "000000000257",
        "000000000260",
        "000000000263",
        "000000000283",
        "000000000294",
      ];
      const offset = Number(args?.offset ?? 0);
      const limit = Number(args?.limit ?? imageIds.length);
      return imageIds.slice(offset, offset + limit).map((id) => ({
        id,
        fileName: `${id}.jpg`,
        width: 640,
        height: 480,
        split: "train",
        status: "已标注",
        tags: ["split=train"],
      }));
    }

    if (command === "get_file_asset_path") {
      if (!tauriState.builtinDownloaded) {
        throw new Error("image not found");
      }
      return `F:/project/Image-Annotation/data/test_data/projects/${args?.projectId}/raw/images/train2017/${args?.imageId}.jpg`;
    }

    if (command === "get_image_annotations") {
      return [
        {
          id: "ann-1",
          classId: 0,
          label: "person",
          type: "bbox",
          bbox: { x: 256, y: 96, width: 128, height: 48 },
          attributes: { split: "train" },
        },
      ];
    }

    if (command === "get_image_annotation_state") {
      return {
        imageId: args?.imageId,
        revision: "rev-1",
        status: "已标注",
        updatedAt: "1778638136",
        objects: [
          {
            id: "ann-1",
            classId: 0,
            label: "person",
            type: "bbox",
            bbox: { x: 256, y: 96, width: 128, height: 48 },
            attributes: { split: "train" },
          },
        ],
      };
    }

    if (command === "save_image_annotations") {
      return {
        revision: "rev-2",
        savedAt: "1778638137",
        auditEventId: "audit-1",
      };
    }

    if (command === "submit_image_annotations") {
      return null;
    }

    if (command === "list_snapshots") {
      return [];
    }

    if (command === "create_dataset_snapshot") {
      return {
        id: "snapshot-1",
        name: args?.name,
        imageCount: 3,
        manifestPath: "F:/project/Image-Annotation/data/workspaces/default/projects/coco128/snapshots/snapshot-1/manifest.json",
        createdAt: "1778638138",
      };
    }

    if (command === "list_exports") {
      return [];
    }

    if (command === "export_dataset") {
      return {
        id: "export-1",
        snapshotId: args?.snapshotId,
        format: args?.format,
        status: "completed",
        outputPath: "F:/project/Image-Annotation/data/workspaces/default/projects/coco128/exports/export-1",
        createdAt: "1778638139",
      };
    }

    if (command === "list_review_queue") {
      return [];
    }

    if (command === "download_test_dataset") {
      await new Promise((resolve) => setTimeout(resolve, 80));
      tauriState.builtinDownloaded = true;
      return {
        id: "download-coco128",
        datasetKey: args?.datasetKey,
        status: "completed",
        progress: 100,
        message: "COCO128 已下载并导入 128 张图片",
        projectId: "coco128",
      };
    }

    if (command === "create_dataset_project") {
      return {
        id: "demo-bbox",
        name: args?.name,
        description: "新建 Demo BBox 数据集",
        annotationTypes: ["BBox"],
        imageCount: 3,
        annotatedPercent: 100,
        reviewCount: 0,
        issueCount: 0,
        classCount: 3,
        tagGroupCount: 3,
        status: "已导入",
        tags: ["source: demo", "format: yolo-detect", "split: train"],
      };
    }

    if (command === "open_local_dataset") {
      return {
        id: "local-out",
        name: "本机 out",
        description: "本机 Pascal VOC 数据集",
        annotationTypes: ["BBox"],
        imageCount: 128,
        annotatedPercent: 100,
        reviewCount: 0,
        issueCount: 0,
        classCount: 1,
        tagGroupCount: 1,
        status: "已导入",
        tags: ["source: local-linked", "format: voc-detect"],
      };
    }

    if (command === "list_backend_tasks") {
      return [
        {
          id: "task-coco128",
          title: "COCO128 导入",
          kind: "dataset-import",
          status: "completed",
          progress: 100,
          message: "COCO128 已下载并导入 128 张图片",
          startedAt: "1778638135",
          finishedAt: "1778638136",
        },
      ];
    }

    if (command === "clear_completed_backend_tasks") {
      return null;
    }

    return null;
  }),
}));

beforeEach(() => {
  window.location.hash = "";
  tauriState.backendAvailable = true;
  tauriState.builtinDownloaded = true;
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => {
      throw new Error("local backend unavailable in App component tests");
    }),
  );
  vi.clearAllMocks();
});

describe("desktop shell", () => {
  it("后端可用但 COCO128 未下载时会自动下载真实测试数据", async () => {
    tauriState.builtinDownloaded = false;

    render(<App />);

    expect(await screen.findByText("正在下载 COCO128 测试数据...")).toBeInTheDocument();
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("download_test_dataset", { datasetKey: "coco128" }),
    );
    expect(await screen.findByRole("button", { name: "标注" })).toBeInTheDocument();
  });

  it("普通浏览器无 Tauri 后端时显示后端未连接且不展示假数据", async () => {
    tauriState.backendAvailable = false;

    render(<App />);

    expect(await screen.findByRole("heading", { name: "后端未连接" })).toBeInTheDocument();
    expect(screen.getByText("请在 Tauri 桌面环境启动应用。")).toBeInTheDocument();
    expect(screen.queryByText("COCO128")).not.toBeInTheDocument();
  });

  it("直接打开项目路由但无 Tauri 后端时显示后端未连接", async () => {
    tauriState.backendAvailable = false;
    window.location.hash = "#/datasets/coco128";

    render(<App />);

    expect(await screen.findByRole("heading", { name: "后端未连接" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "返回数据集" })).toBeInTheDocument();
  });

  it("新建数据集弹窗要求选择数据集类型并调用真实后端创建", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("button", { name: "新建数据集" }));

    expect(screen.getByRole("dialog", { name: "新建数据集" })).toBeInTheDocument();
    expect(screen.getByLabelText("数据集名称")).toHaveValue("Demo BBox 数据集");
    expect(screen.getByLabelText("数据集类型")).toHaveValue("yolo-detect");

    await user.click(screen.getByRole("button", { name: "创建数据集" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("create_dataset_project", {
        name: "Demo BBox 数据集",
        datasetType: "yolo-detect",
        demoTemplate: "demo-bbox",
      }),
    );
  });

  it("主窗口后端任务按钮打开独立后台任务窗口", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("button", { name: "后端任务" }));

    await waitFor(() => expect(invoke).toHaveBeenCalledWith("open_backend_task_tray"));
  });

  it("后台任务独立路由展示任务并可清理已完成任务", async () => {
    const user = userEvent.setup();
    window.location.hash = "#/backend-tasks";
    render(<App />);

    expect(await screen.findByRole("complementary", { name: "后端任务托盘" })).toBeInTheDocument();
    expect(screen.getByText("COCO128 导入")).toBeInTheDocument();
    expect(screen.getByText("completed")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "清理已完成" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("clear_completed_backend_tasks"),
    );
  });

  it("数据集主页只展示高频项目入口，预设下载移动到数据提交弹窗", async () => {
    const user = userEvent.setup();
    render(<App />);

    expect(screen.getByRole("heading", { name: "数据集" })).toBeInTheDocument();
    expect(await screen.findByText("COCO128")).toBeInTheDocument();
    expect(screen.getByText("3 个保存分组")).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "内置下载" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "下载 COCO128" })).not.toBeInTheDocument();
    expect(screen.queryByRole("complementary", { name: "Dataset details" })).not.toBeInTheDocument();
    expect(await screen.findByAltText("COCO128 预览 1")).toHaveAttribute(
      "src",
      expect.stringContaining("asset://"),
    );

    await user.click(screen.getByRole("button", { name: "数据提交" }));

    expect(screen.getByRole("dialog", { name: "数据提交" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "内置下载" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "下载 COCO128" })).toBeInTheDocument();
  });

  it("数据集主页只请求项目预览图片，避免大目录一次性加载", async () => {
    render(<App />);

    await screen.findByText("COCO128");

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("list_project_images", {
        projectId: "coco128",
        groupId: null,
        offset: 0,
        limit: 3,
      }),
    );
  });

  it("数据提交弹窗中的预设下载调用真实后端下载", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("button", { name: "数据提交" }));
    await user.click(await screen.findByRole("button", { name: "下载 COCO128" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("download_test_dataset", { datasetKey: "coco128" }),
    );
  });

  it("数据提交弹窗可以打开本机 LabelImg VOC 目录", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("button", { name: "数据提交" }));
    await user.type(
      screen.getByLabelText("本地路径"),
      "L:\\data_tool\\datas\\lg\\1580_2d\\新建文件夹\\2D数据标注原始\\out",
    );
    await user.click(screen.getByRole("button", { name: "打开本机数据集" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("open_local_dataset", {
        sourcePath: "L:\\data_tool\\datas\\lg\\1580_2d\\新建文件夹\\2D数据标注原始\\out",
        datasetType: "voc-detect",
      }),
    );
  });

  it("数据提交弹窗说明本机目录会原地写回 VOC 或 YOLO", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("button", { name: "数据提交" }));

    expect(screen.getByText(/保存时原地写回 XML 或 TXT/)).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "YOLO BBox TXT" })).toBeInTheDocument();
  });

  it("低频工程说明移动到工程信息弹窗", async () => {
    const user = userEvent.setup();
    render(<App />);

    expect(screen.queryByText("data/workspaces/default")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "工程信息" }));

    expect(screen.getByRole("dialog", { name: "工程信息" })).toBeInTheDocument();
    expect(screen.getByText("data/workspaces/default")).toBeInTheDocument();
    expect(screen.getByText("COCO/YOLO 小数据集导入")).toBeInTheDocument();
  });

  it("左侧主导航使用中文无障碍标签并保持图标化", () => {
    render(<App />);

    expect(screen.getByRole("navigation", { name: "主导航" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "数据集" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    expect(screen.getByRole("button", { name: "任务" })).toBeInTheDocument();
  });

  it("桌面环境顶部显示当前后端连接模式", async () => {
    render(<App />);

    expect(await screen.findByText("Tauri 内部")).toBeInTheDocument();
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("backend_health"));
  });

  it("从数据集卡片进入中文标注工作台", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "标注" }));

    expect(screen.getByRole("heading", { name: "标注工作台" })).toBeInTheDocument();
    expect(screen.getAllByText("000000000009.jpg").length).toBeGreaterThan(0);
    expect(screen.getByText("对象")).toBeInTheDocument();
  });

  it("双击数据集卡片可以打开项目详情", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.dblClick(await screen.findByRole("article", { name: "COCO128 数据集卡片" }));

    expect(screen.getByRole("heading", { name: "COCO128" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "数据分组" })).toBeInTheDocument();
  });

  it("进入单项目详情后可切换中文数据分组、质检和导出页面", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "打开" }));

    expect(screen.getByRole("heading", { name: "COCO128" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "数据分组" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "数据分组" }));
    expect(screen.getByText("标签维度")).toBeInTheDocument();
    expect(screen.getAllByText("train").length).toBeGreaterThan(0);

    await user.click(screen.getByRole("button", { name: "质检" }));
    expect(screen.getByText("质检队列")).toBeInTheDocument();
    expect(screen.getByText("暂无质检问题")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "导出" }));
    expect(screen.getByText("导出预设")).toBeInTheDocument();
    expect(screen.getByText("暂无导出记录")).toBeInTheDocument();
  });

  it("图片浏览页面使用真实图像缩略图", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "打开" }));
    await user.click(screen.getByRole("button", { name: "图片" }));

    expect(await screen.findByAltText("000000000009.jpg")).toHaveAttribute(
      "src",
      expect.stringContaining("asset://"),
    );
  });

  it("图片浏览页面为当前批次靠后的图片继续加载真实缩略图", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "打开" }));
    await user.click(screen.getByRole("button", { name: "图片" }));

    expect(await screen.findByText("000000000196.jpg")).toBeInTheDocument();
    expect(await screen.findByAltText("000000000196.jpg")).toHaveAttribute(
      "src",
      expect.stringContaining("asset://"),
    );
  });

  it("图片浏览页面缩略图显示真实标注框", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "打开" }));
    await user.click(screen.getByRole("button", { name: "图片" }));

    expect(await screen.findByLabelText("000000000196.jpg 标注预览")).toBeInTheDocument();
    expect(screen.getByLabelText("000000000196.jpg 标注框 person")).toBeInTheDocument();
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("get_image_annotations", {
        projectId: "coco128",
        imageId: "000000000196",
      }),
    );
  });

  it("点击开始标注会请求 Tauri 打开独立标注窗口", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "独立窗口标注" }));

    expect(invoke).toHaveBeenCalledWith("open_annotation_window", {
      projectId: "coco128",
      imageId: "000000000009",
    });
  });

  it("直接访问标注 URL 会加载真实图片和标注对象", async () => {
    window.location.hash = "#/annotate/coco128/000000000009";

    render(<App />);

    expect(await screen.findByRole("heading", { name: "标注工作台" })).toBeInTheDocument();
    expect(screen.getByAltText("000000000009.jpg")).toHaveAttribute(
      "src",
      expect.stringContaining("asset://"),
    );
    expect(screen.getAllByText("person").length).toBeGreaterThan(0);
  });

  it("保存 bbox 标注会调用后端持久化命令", async () => {
    const user = userEvent.setup();
    window.location.hash = "#/annotate/coco128/000000000009";

    render(<App />);

    await user.click(await screen.findByRole("button", { name: "保存标注" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "save_image_annotations",
        expect.objectContaining({
          projectId: "coco128",
          imageId: "000000000009",
          revision: "rev-1",
          objects: expect.any(Array),
        }),
      ),
    );
  });

  it("标注控制台保存后显示原地写回提示", async () => {
    const user = userEvent.setup();
    window.location.hash = "#/annotate/coco128/000000000009";

    render(<App />);

    await user.click(await screen.findByRole("button", { name: "保存标注" }));

    expect(await screen.findByText(/已保存并写回标注文件/)).toBeInTheDocument();
  });

  it("标注控制台提交质检会调用后端状态流", async () => {
    const user = userEvent.setup();
    window.location.hash = "#/annotate/coco128/000000000009";

    render(<App />);

    await user.click(await screen.findByRole("button", { name: "提交质检" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("submit_image_annotations", {
        projectId: "coco128",
        imageId: "000000000009",
      }),
    );
  });

  it("标注控制台支持绘制、选中、删除和保存 bbox", async () => {
    const user = userEvent.setup();
    window.location.hash = "#/annotate/coco128/000000000009";

    render(<App />);

    const canvas = await screen.findByTestId("annotation-canvas");
    await user.click(screen.getByRole("button", { name: "BBox" }));

    fireEvent.mouseDown(canvas, { clientX: 120, clientY: 120 });
    fireEvent.mouseMove(canvas, { clientX: 240, clientY: 220 });
    fireEvent.mouseUp(canvas, { clientX: 240, clientY: 220 });

    expect((await screen.findAllByText("object")).length).toBeGreaterThan(0);

    await user.click(screen.getByRole("button", { name: "删除对象" }));
    expect(screen.queryByText("object")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "保存标注" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "save_image_annotations",
        expect.objectContaining({
          revision: "rev-1",
          objects: expect.not.arrayContaining([
            expect.objectContaining({ label: "object" }),
          ]),
        }),
      ),
    );
  });

  it("标注控制台支持精确编辑 bbox 坐标并保存", async () => {
    const user = userEvent.setup();
    window.location.hash = "#/annotate/coco128/000000000009";

    render(<App />);

    const widthInput = await screen.findByLabelText("宽度");
    fireEvent.change(widthInput, { target: { value: "160" } });

    await user.click(screen.getByRole("button", { name: "保存标注" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "save_image_annotations",
        expect.objectContaining({
          revision: "rev-1",
          objects: expect.arrayContaining([
            expect.objectContaining({
              label: "person",
              bbox: expect.objectContaining({ width: 160 }),
            }),
          ]),
        }),
      ),
    );
  });

  it("标注控制台支持复制选中对象", async () => {
    const user = userEvent.setup();
    window.location.hash = "#/annotate/coco128/000000000009";

    render(<App />);

    await screen.findByRole("heading", { name: "标注工作台" });
    await user.click(screen.getByRole("button", { name: "复制对象" }));

    expect(screen.getByText("对象数")).toBeInTheDocument();
    expect(screen.getByText("2")).toBeInTheDocument();
  });

  it("项目详情支持创建快照并基于快照导出", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "打开" }));
    await user.click(screen.getByRole("button", { name: "快照" }));
    await user.click(screen.getByRole("button", { name: "创建快照" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("create_dataset_snapshot", {
        projectId: "coco128",
        name: expect.stringContaining("COCO128"),
      }),
    );

    await user.click(screen.getByRole("button", { name: "导出" }));
    await user.click(screen.getByRole("button", { name: "导出 YOLO" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("export_dataset", {
        projectId: "coco128",
        snapshotId: "snapshot-1",
        format: "yolo",
      }),
    );
  });
});

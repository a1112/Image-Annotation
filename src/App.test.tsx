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
      return [
        {
          id: "000000000009",
          fileName: "000000000009.jpg",
          width: 640,
          height: 480,
          split: "train",
          status: "已标注",
          tags: ["split=train"],
        },
        {
          id: "000000000025",
          fileName: "000000000025.jpg",
          width: 640,
          height: 480,
          split: "train",
          status: "已标注",
          tags: ["split=train"],
        },
        {
          id: "000000000030",
          fileName: "000000000030.jpg",
          width: 640,
          height: 480,
          split: "train",
          status: "已标注",
          tags: ["split=train"],
        },
      ];
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

    if (command === "download_test_dataset") {
      await new Promise((resolve) => setTimeout(resolve, 20));
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

    expect(await screen.findByText("后端未连接")).toBeInTheDocument();
    expect(screen.getByText("请在 Tauri 桌面环境启动应用。")).toBeInTheDocument();
    expect(screen.queryByText("COCO128")).not.toBeInTheDocument();
  });

  it("直接打开项目路由但无 Tauri 后端时显示后端未连接", async () => {
    tauriState.backendAvailable = false;
    window.location.hash = "#/datasets/coco128";

    render(<App />);

    expect(await screen.findByText("后端未连接")).toBeInTheDocument();
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

  it("后端任务托盘展示任务并可清理已完成任务", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("button", { name: "后端任务" }));

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

  it("数据提交弹窗中的预设下载调用真实后端下载", async () => {
    const user = userEvent.setup();
    render(<App />);

    await user.click(screen.getByRole("button", { name: "数据提交" }));
    await user.click(await screen.findByRole("button", { name: "下载 COCO128" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("download_test_dataset", { datasetKey: "coco128" }),
    );
  });

  it("低频工程说明移动到工程信息弹窗", async () => {
    const user = userEvent.setup();
    render(<App />);

    expect(screen.queryByText("data/test_data")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "工程信息" }));

    expect(screen.getByRole("dialog", { name: "工程信息" })).toBeInTheDocument();
    expect(screen.getByText("data/test_data")).toBeInTheDocument();
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
    expect(screen.getByText("暂无导出预设")).toBeInTheDocument();
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
          objects: expect.any(Array),
        }),
      ),
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
});

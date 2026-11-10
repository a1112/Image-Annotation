import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { DatasetCard } from "./DatasetCard";

vi.mock("../shared/useImageAssetUrls", () => ({
  useImageAssetUrls: () => ({}),
}));

describe("DatasetCard remote cache presentation", () => {
  it("opens management from the compact body and keeps annotation on the primary action", async () => {
    const user = userEvent.setup();
    const onOpen = vi.fn();
    const onAnnotate = vi.fn();
    render(
      <DatasetCard
        compact
        dataset={{
          id: "dataset-actions",
          name: "操作数据集",
          description: "",
          annotationTypes: ["BBox"],
          imageCount: 1,
          annotatedPercent: 0,
          reviewCount: 0,
          issueCount: 0,
          classCount: 0,
          tagGroupCount: 0,
          status: "ready",
          tags: [],
        }}
        onAnnotate={onAnnotate}
        onOpen={onOpen}
        previewImages={[]}
      />,
    );

    const card = screen.getByRole("article", { name: "操作数据集 数据集卡片" });
    await user.click(within(card).getByRole("heading", { name: "操作数据集" }));
    expect(onOpen).toHaveBeenCalledTimes(1);

    await user.click(within(card).getByRole("button", { name: "开始标注" }));
    expect(onAnnotate).toHaveBeenCalledTimes(1);
    expect(onOpen).toHaveBeenCalledTimes(1);
  });

  it("hides remote implementation details behind a collaboration label", () => {
    render(
      <DatasetCard
        dataset={{
          id: "remote-cache",
          name: "远端工程",
          description: "真实 remote:https://server.test 账号UUID:user-1:项目UUID:project-1 测试数据集",
          annotationTypes: ["BBox"],
          imageCount: 3,
          annotatedPercent: 0,
          reviewCount: 0,
          issueCount: 0,
          classCount: 1,
          tagGroupCount: 1,
          status: "已导入",
          tags: ["source: ultralytics", "format: remote", "split: train"],
        }}
        onAnnotate={vi.fn()}
        onOpen={vi.fn()}
        onOpenWindow={vi.fn()}
        previewImages={[]}
      />,
    );

    expect(screen.getByText("已缓存的协作项目")).toBeInTheDocument();
    expect(screen.queryByText(/真实 remote:/)).not.toBeInTheDocument();
    expect(screen.queryByText("source: ultralytics")).not.toBeInTheDocument();
    expect(screen.queryByText("format: remote")).not.toBeInTheDocument();
    expect(screen.getByText("split: train")).toBeInTheDocument();
  });

  it("renders a compact card with a clear annotation action and management entry", () => {
    render(
      <DatasetCard
        compact
        dataset={{
          id: "dataset-compact",
          name: "项目数据集",
          description: "较长的说明不应占据项目卡片主区域",
          annotationTypes: ["BBox"],
          imageCount: 12,
          annotatedPercent: 25,
          reviewCount: 3,
          issueCount: 0,
          classCount: 2,
          tagGroupCount: 1,
          status: "ready",
          tags: ["split: train"],
        }}
        onAnnotate={vi.fn()}
        onOpen={vi.fn()}
        previewImages={[]}
      />,
    );

    expect(screen.getByRole("button", { name: "开始标注" })).toHaveClass("primary");
    expect(screen.getByRole("button", { name: "开始标注" })).toBeInTheDocument();
    expect(screen.queryByText("较长的说明不应占据项目卡片主区域")).not.toBeInTheDocument();
    expect(screen.queryByText("待审核")).not.toBeInTheDocument();
  });

  it("keeps compact management actions behind a keyboard-reachable ellipsis menu", async () => {
    const user = userEvent.setup();
    const onRemove = vi.fn();
    render(
      <DatasetCard
        compact
        compactMenu={<button type="button" onClick={onRemove}>移出分组</button>}
        dataset={{
          id: "dataset-menu",
          name: "菜单数据集",
          description: "",
          annotationTypes: ["BBox"],
          imageCount: 2,
          annotatedPercent: 50,
          reviewCount: 0,
          issueCount: 0,
          classCount: 1,
          tagGroupCount: 0,
          status: "ready",
          tags: [],
        }}
        onAnnotate={vi.fn()}
        onOpen={vi.fn()}
        previewImages={[]}
      />,
    );

    expect(screen.getByRole("button", { name: "移出分组" })).not.toBeVisible();
    const menu = screen.getByRole("article", { name: "菜单数据集 数据集卡片" }).querySelector("summary") as HTMLElement;
    menu.focus();
    await user.keyboard(" ");
    expect(screen.getByRole("button", { name: "移出分组" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "移出分组" }));
    expect(onRemove).toHaveBeenCalledTimes(1);
  });

  it("requests one preview lazily when a compact card has no parent-provided image", async () => {
    const loadPreviewImages = vi.fn().mockResolvedValue([{
      id: "image-1",
      fileName: "road.png",
      width: 320,
      height: 240,
      split: "train",
      status: "raw",
      qaStatus: "pending",
      reviewNote: null,
      tags: [],
    }]);
    render(
      <DatasetCard
        compact
        dataset={{
          id: "dataset-preview",
          name: "懒加载数据集",
          description: "",
          annotationTypes: ["BBox"],
          imageCount: 1,
          annotatedPercent: 0,
          reviewCount: 0,
          issueCount: 0,
          classCount: 0,
          tagGroupCount: 0,
          status: "ready",
          tags: [],
        }}
        onAnnotate={vi.fn()}
        onOpen={vi.fn()}
        previewImages={[]}
        loadPreviewImages={loadPreviewImages}
      />,
    );

    await waitFor(() => expect(loadPreviewImages).toHaveBeenCalledWith("dataset-preview"));
  });
});

import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { DatasetGroupingBoard, canonicalDatasetKey } from "./DatasetGroupingBoard";
import type { DatasetProject } from "../../types/domain";

vi.mock("../shared/useImageAssetUrls", () => ({ useImageAssetUrls: () => ({}) }));

const datasets: DatasetProject[] = [
  {
    id: "project-uuid::road-images",
    name: "道路数据",
    description: "",
    annotationTypes: ["BBox"],
    imageCount: 3,
    annotatedPercent: 40,
    reviewCount: 1,
    issueCount: 0,
    classCount: 2,
    tagGroupCount: 0,
    status: "ready",
    tags: [],
  },
  {
    id: "project-uuid::night-images",
    name: "夜间数据",
    description: "",
    annotationTypes: ["BBox"],
    imageCount: 2,
    annotatedPercent: 0,
    reviewCount: 0,
    issueCount: 0,
    classCount: 1,
    tagGroupCount: 0,
    status: "ready",
    tags: [],
  },
];

function renderBoard(overrides: Partial<React.ComponentProps<typeof DatasetGroupingBoard>> = {}) {
  return render(<DatasetGroupingBoard
    datasets={datasets}
    groups={[]}
    canEdit
    onPersist={vi.fn().mockResolvedValue(undefined)}
    onOpen={vi.fn()}
    onAnnotate={vi.fn()}
    {...overrides}
  />);
}

describe("DatasetGroupingBoard", () => {
  it("normalizes Web dataset IDs to the stored canonical key", () => {
    expect(canonicalDatasetKey("project-uuid::road-images")).toBe("road-images");
    expect(canonicalDatasetKey("road-images")).toBe("road-images");
  });

  it("moves an ungrouped dataset into a group with pointer events", async () => {
    const onPersist = vi.fn().mockResolvedValue(undefined);
    renderBoard({
      groups: [{ id: "group-a", name: "白天", datasetKeys: ["night-images"], position: 0 }],
      onPersist,
    });

    const roadCard = screen.getByRole("article", { name: "道路数据 数据集卡片" }).parentElement as HTMLElement;
    const targetGroup = screen.getByRole("button", { name: "分组菜单 白天" }).closest("article") as HTMLElement;
    fireEvent.pointerDown(roadCard, { button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerMove(roadCard, { clientX: 12, clientY: 12 });
    fireEvent.pointerUp(targetGroup, { clientX: 24, clientY: 24 });

    await waitFor(() => expect(onPersist).toHaveBeenCalledWith([{ id: "group-a", name: "白天", datasetKeys: ["night-images", "road-images"], position: 0 }]));
  });

  it("creates a group from an ungrouped dataset and renames it inline", async () => {
    const user = userEvent.setup();
    const onPersist = vi.fn().mockResolvedValue(undefined);
    renderBoard({ onPersist });

    const roadCard = screen.getByRole("article", { name: "道路数据 数据集卡片" }).parentElement as HTMLElement;
    await user.click(within(roadCard).getByRole("button", { name: "道路数据 管理菜单" }));
    await user.click(within(roadCard).getByRole("button", { name: "新建分组" }));
    const renameInput = await screen.findByRole("textbox", { name: /重命名分组/ });
    await user.clear(renameInput);
    await user.type(renameInput, "白天数据");
    await user.keyboard("{Enter}");

    await waitFor(() => expect(onPersist).toHaveBeenCalledTimes(2));
    expect(onPersist.mock.calls[1][0][0]).toMatchObject({ name: "白天数据", datasetKeys: ["road-images"] });
  });

  it("opens the group picker instead of persisting an empty group", async () => {
    const user = userEvent.setup();
    const onPersist = vi.fn().mockResolvedValue(undefined);
    renderBoard({ onPersist });

    const board = screen.getByRole("region", { name: "数据集分组" });
    await user.click(within(board).getAllByRole("button", { name: "新建分组" })[0]);

    const picker = screen.getByRole("dialog", { name: "选择分组数据集" });
    expect(within(picker).getByRole("button", { name: "创建分组" })).toBeDisabled();
    await user.click(within(picker).getByRole("checkbox", { name: "道路数据" }));
    await user.click(within(picker).getByRole("button", { name: "创建分组" }));

    await waitFor(() => expect(onPersist).toHaveBeenCalledTimes(1));
    expect(onPersist.mock.calls[0][0]).toEqual([
      expect.objectContaining({ name: "新分组", datasetKeys: ["road-images"] }),
    ]);
    expect(screen.queryByRole("dialog", { name: "选择分组数据集" })).not.toBeInTheDocument();
  });

  it("expands a group and exposes management and annotation actions for members", async () => {
    const user = userEvent.setup();
    const onOpen = vi.fn();
    const onAnnotate = vi.fn();
    renderBoard({
      groups: [{ id: "group-a", name: "白天", datasetKeys: ["road-images"], position: 0 }],
      onOpen,
      onAnnotate,
    });

    expect(screen.queryByRole("article", { name: "道路数据 数据集卡片" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "白天", exact: true }));
    const member = screen.getByRole("article", { name: "道路数据 数据集卡片" });

    await user.click(within(member).getByRole("button", { name: "打开" }));
    await user.click(within(member).getByRole("button", { name: "开始标注" }));
    expect(onOpen).toHaveBeenCalledWith("project-uuid::road-images");
    expect(onAnnotate).toHaveBeenCalledWith(datasets[0]);
  });

  it("keeps group management hidden until the header menu is opened", async () => {
    const user = userEvent.setup();
    renderBoard({ groups: [{ id: "group-a", name: "白天", datasetKeys: ["road-images"], position: 0 }] });

    expect(screen.queryByRole("button", { name: "重命名" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "分组菜单 白天" }));
    expect(screen.getByRole("button", { name: "重命名" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "解散分组" })).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "移动分组 白天" })).toBeInTheDocument();
  });

  it("moves a grouped dataset onto an ungrouped dataset with one atomic persist", async () => {
    const onPersist = vi.fn().mockResolvedValue(undefined);
    renderBoard({
      groups: [{ id: "group-a", name: "白天", datasetKeys: ["road-images"], position: 0 }],
      onPersist,
    });

    await userEvent.setup().click(screen.getByRole("button", { name: "白天", exact: true }));
    const source = screen.getByRole("article", { name: "道路数据 数据集卡片" }).closest(".data-projects-group-member") as HTMLElement;
    const target = screen.getByRole("article", { name: "夜间数据 数据集卡片" }).parentElement as HTMLElement;
    fireEvent.pointerDown(source, { button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerMove(target, { clientX: 12, clientY: 12 });
    expect(screen.getByRole("status")).toHaveTextContent("创建新分组");
    fireEvent.pointerUp(target, { clientX: 24, clientY: 24 });

    await waitFor(() => expect(onPersist).toHaveBeenCalledTimes(1));
    expect(onPersist.mock.calls[0][0]).toEqual([
      expect.objectContaining({ datasetKeys: ["night-images", "road-images"] }),
    ]);
    expect(onPersist.mock.calls[0][0]).toHaveLength(1);
  });

  it("restores the previous layout and reports an error when a move cannot be saved", async () => {
    const onPersist = vi.fn().mockRejectedValue(new Error("保存分组失败"));
    renderBoard({
      groups: [{ id: "group-a", name: "白天", datasetKeys: ["night-images"], position: 0 }],
      onPersist,
    });

    const source = screen.getByRole("article", { name: "道路数据 数据集卡片" }).parentElement as HTMLElement;
    const target = screen.getByRole("button", { name: "分组菜单 白天" }).closest("article") as HTMLElement;
    fireEvent.pointerDown(source, { button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerMove(target, { clientX: 12, clientY: 12 });
    fireEvent.pointerUp(target, { clientX: 24, clientY: 24 });

    expect(await screen.findByRole("alert")).toHaveTextContent("保存分组失败");
    expect(screen.getByRole("article", { name: "道路数据 数据集卡片" }).closest(".data-projects-ungrouped-cards")).toBeInTheDocument();
    expect(onPersist).toHaveBeenCalledTimes(1);
  });

  it("highlights a valid drop and suppresses the click that follows a drag", async () => {
    const onPersist = vi.fn().mockResolvedValue(undefined);
    const onOpen = vi.fn();
    renderBoard({
      groups: [{ id: "group-a", name: "白天", datasetKeys: ["night-images"], position: 0 }],
      onPersist,
      onOpen,
    });

    const source = screen.getByRole("article", { name: "道路数据 数据集卡片" }).parentElement as HTMLElement;
    const target = screen.getByRole("button", { name: "分组菜单 白天" }).closest("article") as HTMLElement;
    const ungrouped = screen.getByText("未分组").closest(".data-projects-ungrouped-column") as HTMLElement;
    fireEvent.pointerDown(source, { button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerUp(ungrouped, { clientX: 24, clientY: 24 });
    fireEvent.click(screen.getByRole("article", { name: "道路数据 数据集卡片" }));
    expect(onOpen).not.toHaveBeenCalled();

    fireEvent.pointerDown(source, { button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerMove(target, { clientX: 12, clientY: 12 });
    expect(screen.getByRole("status")).toHaveTextContent("移入此分组");
    expect(target).toHaveClass("is-drop-target");
    fireEvent.pointerUp(target, { clientX: 24, clientY: 24 });

    expect(onOpen).not.toHaveBeenCalled();
    await waitFor(() => expect(onPersist).toHaveBeenCalledTimes(1));
  });

  it("does not persist self-drops or pointer releases outside the board", async () => {
    const onPersist = vi.fn().mockResolvedValue(undefined);
    renderBoard({ onPersist });

    const roadWrapper = screen.getByRole("article", { name: "道路数据 数据集卡片" }).parentElement as HTMLElement;
    fireEvent.pointerDown(roadWrapper, { button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerUp(roadWrapper, { clientX: 20, clientY: 20 });
    fireEvent.pointerDown(roadWrapper, { button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerUp(document.body, { clientX: 20, clientY: 20 });

    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(onPersist).not.toHaveBeenCalled();
  });

  it("does not persist a same-group drop and supports undo", async () => {
    const onPersist = vi.fn().mockResolvedValue(undefined);
    renderBoard({
      groups: [{ id: "group-a", name: "白天", datasetKeys: ["road-images"], position: 0 }],
      onPersist,
    });

    const group = screen.getByRole("button", { name: "分组菜单 白天" }).closest("article") as HTMLElement;
    await userEvent.setup().click(screen.getByRole("button", { name: "白天", exact: true }));
    const member = screen.getByText("道路数据").closest(".data-projects-group-member") as HTMLElement;
    fireEvent.pointerDown(member, { button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerUp(group, { clientX: 20, clientY: 20 });
    expect(onPersist).not.toHaveBeenCalled();

    await userEvent.setup().click(within(member).getByRole("button", { name: "道路数据 管理菜单" }));
    const moveButton = within(member).getByRole("button", { name: "移出分组" });
    await userEvent.setup().click(moveButton);
    await waitFor(() => expect(onPersist).toHaveBeenCalledTimes(1));
    await userEvent.setup().click(screen.getByRole("button", { name: "撤销" }));
    await waitFor(() => expect(onPersist).toHaveBeenCalledTimes(2));
    expect(onPersist.mock.calls[1][0][0].datasetKeys).toEqual(["road-images"]);
  });

  it("keeps undo available when the parent echoes a successful group save", async () => {
    const onPersist = vi.fn<(next: Array<{ id: string; name: string; datasetKeys: string[]; position: number }>) => void>();
    function EchoBoard() {
      const [groups, setGroups] = useState([{ id: "group-a", name: "白天", datasetKeys: ["night-images"], position: 0 }]);
      return <DatasetGroupingBoard
        datasets={datasets}
        groups={groups}
        canEdit
        onPersist={(next) => { onPersist(next); setGroups(next); }}
        onOpen={vi.fn()}
        onAnnotate={vi.fn()}
      />;
    }

    render(<EchoBoard />);
    const roadCard = screen.getByRole("article", { name: "道路数据 数据集卡片" }).parentElement as HTMLElement;
    const targetGroup = screen.getByRole("button", { name: "分组菜单 白天" }).closest("article") as HTMLElement;
    fireEvent.pointerDown(roadCard, { button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerMove(roadCard, { clientX: 12, clientY: 12 });
    fireEvent.pointerUp(targetGroup, { clientX: 24, clientY: 24 });

    const undo = await screen.findByRole("button", { name: "撤销" });
    await waitFor(() => expect(onPersist).toHaveBeenCalledTimes(1));
    expect(undo).toBeEnabled();
  });

  it("hides grouping mutation controls for read-only project metadata", () => {
    renderBoard({ canEdit: false, groups: [{ id: "group-a", name: "白天", datasetKeys: ["road-images"], position: 0 }] });
    expect(screen.queryByRole("button", { name: "新建分组" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "分组菜单 白天" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /移出/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("combobox", { name: /移动数据集/ })).not.toBeInTheDocument();
  });
});

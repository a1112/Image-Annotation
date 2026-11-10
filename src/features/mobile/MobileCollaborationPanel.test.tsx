import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { MobileCollaborationPanel } from "./MobileCollaborationPanel";
import {
  claimRemoteTaskItem,
  flushOfflineQueue,
  listRemoteReviewQueue,
  listRemoteTaskItems,
  listRemoteTasks,
  releaseRemoteTaskItem,
} from "../../api/mobile";
import {
  cacheRemoteAsset,
  claimTaskItem,
  listReviewQueue,
  listTaskItems,
  listTasks,
  releaseTaskItem,
  reviewTaskItem,
} from "../../api/tauri";

vi.mock("../../api/mobile", () => ({
  claimRemoteTaskItem: vi.fn(),
  flushOfflineQueue: vi.fn(),
  listRemoteReviewQueue: vi.fn(),
  listRemoteTaskItems: vi.fn(),
  listRemoteTasks: vi.fn(),
  releaseRemoteTaskItem: vi.fn(),
}));

vi.mock("../../api/tauri", () => ({
  cacheRemoteAsset: vi.fn(),
  claimTaskItem: vi.fn(),
  listReviewQueue: vi.fn(),
  listTaskItems: vi.fn(),
  listTasks: vi.fn(),
  releaseTaskItem: vi.fn(),
  reviewTaskItem: vi.fn(),
}));

const offlineStatus = {
  online: true,
  queuedOperations: 0,
  conflictCount: 0,
  cachedAssets: 0,
  lastSyncAt: null,
  lastError: null,
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(listTasks).mockResolvedValue([]);
  vi.mocked(listTaskItems).mockResolvedValue([]);
  vi.mocked(listReviewQueue).mockResolvedValue([]);
  vi.mocked(listRemoteTasks).mockResolvedValue([]);
  vi.mocked(listRemoteTaskItems).mockResolvedValue([]);
  vi.mocked(listRemoteReviewQueue).mockResolvedValue([]);
  vi.mocked(flushOfflineQueue).mockResolvedValue(offlineStatus);
  vi.mocked(cacheRemoteAsset).mockResolvedValue({
    assetId: "asset-1",
    contentHash: "hash",
    localPath: "cache/image.jpg",
    cacheKind: "original",
    byteSize: 1,
    lastAccessedAt: "",
    verifiedAt: null,
    pinned: true,
  });
});

describe("MobileCollaborationPanel image entry points", () => {
  it("opens a local cached image from a task item instead of exposing the remote UUID", async () => {
    vi.mocked(listTasks).mockResolvedValue([{
      id: "task-1",
      name: "第一轮标注",
      status: "进行中",
      createdAt: "",
      updatedAt: "",
    }]);
    vi.mocked(listTaskItems).mockResolvedValue([{
      id: "item-1",
      taskId: "task-1",
      imageId: "remote-image-uuid",
      clientImageId: "local-image-1",
      status: "草稿",
      qaStatus: "",
      reviewNote: null,
      lockedAt: null,
    }]);
    const onOpenImage = vi.fn();

    render(
      <MobileCollaborationPanel
        authenticated={false}
        localProjectId="local-project"
        onMessage={vi.fn()}
        onOpenImage={onOpenImage}
        section="tasks"
      />,
    );

    await screen.findByText("local-image-1");
    expect(screen.queryByText("remote-image-uuid")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "打开标注" }));
    expect(onOpenImage).toHaveBeenCalledWith("local-image-1");
  });

  it("opens the cached client image before taking a review decision", async () => {
    vi.mocked(listReviewQueue).mockResolvedValue([{
      imageId: "remote-image-uuid",
      clientImageId: "local-image-2",
      fileName: "cat.jpg",
      annotationStatus: "待审阅",
    }]);
    const onOpenImage = vi.fn();

    render(
      <MobileCollaborationPanel
        authenticated={false}
        localProjectId="local-project"
        onMessage={vi.fn()}
        onOpenImage={onOpenImage}
        section="review"
      />,
    );

    await waitFor(() => expect(screen.getByText("cat.jpg")).toBeInTheDocument());
    fireEvent.click(screen.getByRole("button", { name: "查看标注" }));
    expect(onOpenImage).toHaveBeenCalledWith("local-image-2");
  });

  it("does not request the review queue from the remote task section", async () => {
    const onMessage = vi.fn();
    render(
      <MobileCollaborationPanel
        authenticated
        localProjectId="local-project"
        onMessage={onMessage}
        onOpenImage={vi.fn()}
        remoteProjectId="remote-project"
        section="tasks"
      />,
    );

    await waitFor(() => expect(listRemoteTasks).toHaveBeenCalledWith("remote-project"));
    expect(listRemoteReviewQueue).not.toHaveBeenCalled();
    expect(onMessage).not.toHaveBeenCalled();
  });
});

void claimRemoteTaskItem;
void listRemoteReviewQueue;
void listRemoteTaskItems;
void releaseRemoteTaskItem;
void claimTaskItem;
void releaseTaskItem;
void reviewTaskItem;

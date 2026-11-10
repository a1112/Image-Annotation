import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  HOSTING_ARCHIVE_MAX_BYTES,
  addHostedMember,
  beginStableHostingOperation,
  captureHostedProject,
  completeStableHostingOperation,
  createHostedDataset,
  createHostedVersion,
  downloadHostedExport,
  getHostingQuota,
  getHostingJob,
  listHostingJobs,
  listHostedDatasets,
  readHostingArchive,
  reuseHostedVersion,
  openHostedExport,
  updateHostedDataset,
  uploadHostedArchive,
} from "./hosting";
import { remoteRequest } from "./mobile";
import { invokeBackendCommand } from "./tauri";

vi.mock("./mobile", () => ({
  remoteRequest: vi.fn(),
}));

vi.mock("./tauri", () => ({
  invokeBackendCommand: vi.fn(),
}));

vi.mock("../platform/runtime", () => ({
  isTauriRuntime: vi.fn(() => false),
}));

describe("hosting API adapter", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    window.sessionStorage.clear();
  });

  it("keeps one operation id for the same account-scoped intent until confirmation", () => {
    const context = {
      accountScope: "https://example.test:user-1",
      datasetId: "dataset-1",
      action: "export",
      payload: { versionId: "version-1", format: "native" },
    };
    const first = beginStableHostingOperation(context);
    const retry = beginStableHostingOperation(context);
    expect(retry).toEqual(first);

    const otherAccount = beginStableHostingOperation({ ...context, accountScope: "https://example.test:user-2" });
    expect(otherAccount.operationId).not.toBe(first.operationId);

    completeStableHostingOperation(first);
    const nextIntent = beginStableHostingOperation(context);
    expect(nextIntent.operationId).not.toBe(first.operationId);
  });

  it("normalizes dataset list responses and keeps the server path scoped", async () => {
    vi.mocked(remoteRequest).mockResolvedValue({
      status: 200,
      data: { items: [{ id: "dataset-1", name: "道路", itemCount: 4, totalBytes: 2048, role: "owner", revision: 3, status: "active" }] },
    });

    await expect(listHostedDatasets()).resolves.toEqual([expect.objectContaining({
      id: "dataset-1",
      name: "道路",
      itemCount: 4,
      totalBytes: 2048,
      revision: 3,
      role: "owner",
    })]);
    expect(remoteRequest).toHaveBeenCalledWith({ method: "GET", path: "/api/v1/datasets" });
  });

  it("retries one transient read but does not retry deterministic authorization errors", async () => {
    vi.mocked(remoteRequest)
      .mockRejectedValueOnce(new Error("remote request failed (503)"))
      .mockResolvedValueOnce({ status: 200, data: { items: [] } });
    await expect(listHostedDatasets()).resolves.toEqual([]);
    expect(remoteRequest).toHaveBeenCalledTimes(2);

    vi.mocked(remoteRequest).mockReset();
    vi.mocked(remoteRequest).mockRejectedValue(new Error("remote request failed (403): forbidden"));
    await expect(listHostedDatasets()).rejects.toThrow("403");
    expect(remoteRequest).toHaveBeenCalledTimes(1);

    vi.mocked(remoteRequest).mockReset();
    vi.mocked(remoteRequest).mockRejectedValue(new Error("remote request failed (400): invalid query"));
    await expect(listHostedDatasets()).rejects.toThrow("400");
    expect(remoteRequest).toHaveBeenCalledTimes(1);
  });

  it("sends revision-guarded edits instead of browser credentials", async () => {
    vi.mocked(remoteRequest).mockResolvedValue({ status: 200, data: { id: "dataset-1", name: "新道路", revision: 4 } });

    await createHostedDataset({ name: "新道路", description: "说明" });
    await updateHostedDataset("dataset/1", { name: "新名字", description: "新说明", baseRevision: 3 });

    expect(remoteRequest).toHaveBeenNthCalledWith(1, {
      method: "POST",
      path: "/api/v1/datasets",
      body: { name: "新道路", description: "说明" },
    });
    expect(remoteRequest).toHaveBeenNthCalledWith(2, {
      method: "PATCH",
      path: "/api/v1/datasets/dataset%2F1",
      body: { name: "新名字", description: "新说明", baseRevision: 3 },
    });
  });

  it("sends member email to the server while retaining UUID compatibility", async () => {
    vi.mocked(remoteRequest)
      .mockResolvedValueOnce({ status: 200, data: { id: "member-email", email: "member@example.com", role: "viewer" } })
      .mockResolvedValueOnce({ status: 200, data: { id: "member-uuid", userId: "user-2", role: "manager" } });

    await addHostedMember("dataset-1", "member@example.com", "viewer");
    await addHostedMember("dataset-1", "user-2", "manager");

    expect(remoteRequest).toHaveBeenNthCalledWith(1, {
      method: "POST",
      path: "/api/v1/datasets/dataset-1/members",
      body: { email: "member@example.com", role: "viewer" },
    });
    expect(remoteRequest).toHaveBeenNthCalledWith(2, {
      method: "POST",
      path: "/api/v1/datasets/dataset-1/members",
      body: { userId: "user-2", role: "manager" },
    });
  });

  it("normalizes server capture, immutable version, reuse, quota, and job shapes", async () => {
    vi.mocked(remoteRequest)
      .mockResolvedValueOnce({ status: 200, data: { datasetId: "dataset-1", capturedItems: 3, revision: 4 } })
      .mockResolvedValueOnce({ status: 200, data: { id: "dataset-1", name: "道路", revision: 4 } })
      .mockResolvedValueOnce({ status: 200, data: { datasetId: "dataset-1", versionId: "version-2", version: 2, name: "审阅版", itemCount: 3 } })
      .mockResolvedValueOnce({ status: 200, data: { datasetId: "dataset-1", versionId: "version-2", projectId: "project-2", reusedAssets: [{ assetId: "asset-1" }, { assetId: "asset-2" }] } })
      .mockResolvedValueOnce({ status: 200, data: { limitBytes: 1000, usedBytes: 400, reservedBytes: 50, maxAssets: 10, usedAssets: 3, canManage: true } })
      .mockResolvedValueOnce({ status: 200, data: { items: [{ id: "job-1", kind: "export", status: "succeeded", errorMessage: null }] } })
      .mockResolvedValueOnce({ status: 200, data: { id: "job-1", kind: "export", status: "failed", errorMessage: "worker failed", outputByteSize: 512, outputContentHash: "deadbeef" } });

    await expect(captureHostedProject("dataset-1", { projectId: "project-1", operationId: "capture-op" }))
      .resolves.toEqual(expect.objectContaining({ id: "dataset-1", revision: 4 }));
    await expect(createHostedVersion("dataset-1", { name: "审阅版", operationId: "version-op" }))
      .resolves.toEqual(expect.objectContaining({ id: "version-2", versionNumber: 2, itemCount: 3 }));
    await expect(reuseHostedVersion("dataset-1", { versionId: "version-2", operationId: "reuse-op" }))
      .resolves.toEqual({ projectId: "project-2", reusedAssets: 2 });
    await expect(getHostingQuota()).resolves.toEqual(expect.objectContaining({ reservedBytes: 50, canManage: true }));
    await expect(listHostingJobs("dataset-1")).resolves.toEqual([expect.objectContaining({ status: "completed", progress: 100 })]);
    await expect(getHostingJob("job-1")).resolves.toEqual(expect.objectContaining({
      status: "failed",
      error: "worker failed",
      byteSize: 512,
      sha256: "deadbeef",
    }));
  });

  it("passes a stable operation id to the native archive bridge and enforces 64 MiB", async () => {
    vi.mocked(invokeBackendCommand).mockResolvedValue({ id: "job-1", kind: "import", status: "queued", progress: 0 });
    await uploadHostedArchive({
      datasetId: "dataset-1",
      format: "coco",
      fileName: "labels.zip",
      base64: "AQID",
      operationId: "operation-1",
    });
    expect(invokeBackendCommand).toHaveBeenCalledWith("hosting_upload_archive", {
      datasetId: "dataset-1",
      format: "coco",
      fileName: "labels.zip",
      base64: "AQID",
      operationId: "operation-1",
    });

    const oversized = "A".repeat(Math.ceil((HOSTING_ARCHIVE_MAX_BYTES * 4) / 3) + 4);
    await expect(uploadHostedArchive({
      datasetId: "dataset-1",
      format: "native",
      fileName: "too-large.zip",
      base64: oversized,
    })).rejects.toThrow("64 MiB");
  });

  it("turns native export bytes into a download result", async () => {
    vi.mocked(invokeBackendCommand).mockResolvedValue({
      path: "/app_data/exports/dataset.zip",
      fileName: "dataset.zip",
      byteSize: 42,
      sha256: "abc123",
    });

    await expect(downloadHostedExport("job/1")).resolves.toEqual({
      path: "/app_data/exports/dataset.zip",
      fileName: "dataset.zip",
      byteSize: 42,
      sha256: "abc123",
      url: "/app_data/exports/dataset.zip",
    });
    expect(invokeBackendCommand).toHaveBeenCalledWith("hosting_download_export", { jobId: "job/1" });
  });

  it("opens an export through the native job bridge without accepting a path", async () => {
    await expect(openHostedExport("job/1")).resolves.toBeUndefined();
    expect(invokeBackendCommand).toHaveBeenCalledWith("hosting_open_export", { jobId: "job/1" });
  });

  it("reads a small archive without a FileReader or browser-side storage", async () => {
    const file = new File([new Uint8Array([0, 1, 2, 255])], "labels.zip", { type: "application/zip" });
    await expect(readHostingArchive(file)).resolves.toBe("AAEC/w==");
  });
});

import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  cloneDataProject,
  connectRemoteDataProject,
  discoverDataProjectRemotes,
  getDataProjectDatasetGroups,
  setDataProjectDatasetGroups,
} from "./dataProjects";

const tauriApi = vi.hoisted(() => ({
  invokeBackendCommand: vi.fn(),
  isWebBackendTransport: vi.fn(),
}));

vi.mock("./tauri", () => tauriApi);

describe("data-project remote transport adapters", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    tauriApi.isWebBackendTransport.mockReturnValue(true);
  });

  it("uses the Web discovery alias and maps the privacy-safe project summary", async () => {
    tauriApi.invokeBackendCommand.mockResolvedValueOnce([{
      id: "remote-web",
      name: "远端项目",
      description: "浏览器工作区",
      datasetIds: ["remote-web::road"],
      createdAt: "2026-10-26T09:00:00Z",
      updatedAt: "2026-10-26T10:00:00Z",
      accountScope: null,
      revision: 4,
      role: "viewer",
    }]);

    await expect(discoverDataProjectRemotes()).resolves.toEqual([{
      id: "remote-web",
      organizationId: null,
      ownerId: null,
      name: "远端项目",
      description: "浏览器工作区",
      status: "active",
      revision: 4,
      role: "viewer",
      datasetCount: 1,
      refCount: 0,
      commitCount: 0,
      updatedAt: "2026-10-26T10:00:00Z",
    }]);
    expect(tauriApi.invokeBackendCommand).toHaveBeenCalledWith("discover_data_project_remotes");
  });

  it("keeps native discovery on its original command", async () => {
    tauriApi.isWebBackendTransport.mockReturnValue(false);
    tauriApi.invokeBackendCommand.mockResolvedValueOnce([{
      id: "remote-native",
      organizationId: "org-1",
      ownerId: "user-1",
      name: "桌面项目",
      description: "",
      status: "ready",
      revision: 2,
      role: "owner",
      datasetCount: 2,
      refCount: 1,
      commitCount: 3,
      updatedAt: "2026-10-26T10:00:00Z",
    }]);

    await expect(discoverDataProjectRemotes()).resolves.toMatchObject([{ id: "remote-native", ownerId: "user-1", datasetCount: 2 }]);
    expect(tauriApi.invokeBackendCommand).toHaveBeenCalledWith("list_remote_data_projects");
  });

  it("maps Web connect detail and clone report without inventing a local head", async () => {
    tauriApi.invokeBackendCommand
      .mockResolvedValueOnce({
        project: {
          id: "remote-web",
          name: "远端项目",
          description: "",
          datasetIds: ["remote-web::road"],
          createdAt: "2026-10-26T09:00:00Z",
          updatedAt: "2026-10-26T10:00:00Z",
          accountScope: null,
          revision: 7,
          role: "editor",
        },
        datasets: [{ id: "remote-web::road", datasetKey: "road" }],
        branches: [{ name: "main", headCommitId: "abc123" }],
        activeBranch: "main",
      })
      .mockResolvedValueOnce({
        localProjectId: "remote-web",
        remoteProjectId: "remote-web",
        remoteRevision: 7,
        localHead: null,
        remoteHead: null,
        outcome: "already-present",
        trackingRef: null,
      });

    const bundle = await connectRemoteDataProject("local-web", "remote-web");
    expect(bundle.project).toMatchObject({ id: "remote-web", role: "editor", revision: 7 });
    expect(bundle.datasets).toEqual([{ id: "remote-web::road", datasetKey: "road" }]);
    expect(bundle.remoteRevision).toBe(7);

    await expect(cloneDataProject("remote-web")).resolves.toEqual({
      localProjectId: "remote-web",
      remoteProjectId: "remote-web",
      remoteRevision: 7,
      localHead: null,
      remoteHead: null,
      outcome: "already-present",
      trackingRef: null,
    });
    expect(tauriApi.invokeBackendCommand).toHaveBeenLastCalledWith("clone_remote_data_project", { remoteProjectId: "remote-web" });
  });

  it("recovers a Web clone revision from the authenticated discovery summary when dispatcher omits it", async () => {
    tauriApi.invokeBackendCommand
      .mockResolvedValueOnce({
        localProjectId: "remote-web",
        remoteProjectId: "remote-web",
        remoteRevision: null,
        localHead: null,
        remoteHead: null,
        outcome: "already-present",
        trackingRef: null,
      })
      .mockResolvedValueOnce([{
        id: "remote-web",
        name: "远端项目",
        description: "",
        datasetIds: [],
        revision: 8,
        role: "viewer",
        updatedAt: "2026-10-26T10:00:00Z",
      }]);

    await expect(cloneDataProject("remote-web")).resolves.toMatchObject({
      remoteProjectId: "remote-web",
      remoteRevision: 8,
      outcome: "already-present",
    });
    expect(tauriApi.invokeBackendCommand).toHaveBeenNthCalledWith(2, "discover_data_project_remotes");
  });

  it("rejects a malformed discovery response instead of leaking it into the UI", async () => {
    tauriApi.invokeBackendCommand.mockResolvedValueOnce({ items: [] });
    await expect(discoverDataProjectRemotes()).rejects.toThrow("远端数据项目响应格式无效");
  });
});


describe("project group response scope", () => {
  it("rejects mismatched project or branch state before rendering another scope", async () => {
    tauriApi.invokeBackendCommand.mockResolvedValueOnce({ dataProjectId: "other", branch: "main", revision: 1, projectMetadata: { schemaVersion: 1, datasetGroups: [] }, canEdit: true });
    await expect(getDataProjectDatasetGroups("project", "main")).rejects.toThrow("作用域无效");
  });
  it("preserves unknown supported-version metadata and propagates compare-and-swap failures", async () => {
    const state = { dataProjectId: "project", branch: "review", revision: 4, projectMetadata: { schemaVersion: 1, datasetGroups: [], custom: { retained: true } }, canEdit: false };
    tauriApi.invokeBackendCommand.mockResolvedValueOnce(state);
    await expect(getDataProjectDatasetGroups("project", "review")).resolves.toEqual(state);
    tauriApi.invokeBackendCommand.mockRejectedValueOnce(new Error("WORKTREE_REVISION_MISMATCH"));
    await expect(setDataProjectDatasetGroups("project", "review", 4, [])).rejects.toThrow("REVISION_MISMATCH");
  });
});

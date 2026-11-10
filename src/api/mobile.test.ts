import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  DEFAULT_REMOTE_SERVER_URL,
  authAcceptInvitation,
  authLogin,
  createRemoteInvitation,
  defaultRemoteServerUrl,
  getAccountProfile,
  listRemoteProjects,
  listRemoteReviewQueue,
  listRemoteTaskItems,
  listRemoteTasks,
  claimRemoteTaskItem,
  releaseRemoteTaskItem,
  reviewRemoteItem,
  revokeRemoteDevice,
  remoteProjectOpen,
  remoteRequest,
  offlineStatus,
  flushOfflineQueue,
} from "./mobile";

vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: vi.fn((path: string) => `asset://${path}`),
  invoke: vi.fn(),
}));

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(invoke).mockResolvedValue({});
});

it("preserves native sync failures without another transport or retry", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
  vi.mocked(invoke).mockRejectedValue(new Error("role annotator lacks permission"));
  try {
    await expect(offlineStatus("project-1")).rejects.toThrow("lacks permission");
    await expect(flushOfflineQueue("project-1")).rejects.toThrow("lacks permission");
    expect(vi.mocked(invoke).mock.calls.map(([command]) => command)).toEqual(["offline_status", "flush_offline_queue"]);
  } finally {
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
  }
});

describe("mobile Rust command bridge", () => {
  it("uses the configured default collaboration endpoint", () => {
    expect(defaultRemoteServerUrl()).toBe(DEFAULT_REMOTE_SERVER_URL);
  });

  it("passes login and remote requests to Rust without creating a JS token store", async () => {
    await authLogin({
      serverUrl: DEFAULT_REMOTE_SERVER_URL,
      username: "alice",
      password: "secret",
    });
    await remoteRequest({ method: "POST", path: "/api/v1/projects/p1/sync", body: { cursor: "c1" } });

    expect(invoke).toHaveBeenNthCalledWith(1, "auth_login", {
      serverUrl: DEFAULT_REMOTE_SERVER_URL,
      email: "alice",
      password: "secret",
      deviceName: "Image Annotation Mobile",
    });
    expect(invoke).toHaveBeenNthCalledWith(2, "remote_request", {
      method: "POST",
      path: "/api/v1/projects/p1/sync",
      body: { cursor: "c1" },
    });
  });

  it("accepts invitations through the native session command and creates project invites remotely", async () => {
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "auth_accept_invitation") {
        return { authenticated: true, serverUrl: DEFAULT_REMOTE_SERVER_URL, accountId: "alice", deviceId: "device-1" };
      }
      if (command === "remote_request") return { id: "invite-1", email: "bob@example.test", role: "annotator", token: "invite-token" };
      return {};
    });

    await authAcceptInvitation({
      serverUrl: DEFAULT_REMOTE_SERVER_URL,
      token: "invite-token",
      displayName: "Bob",
      password: "a-strong-password",
    });
    await createRemoteInvitation("remote-project", "bob@example.test", "annotator");

    expect(invoke).toHaveBeenCalledWith("auth_accept_invitation", {
      serverUrl: DEFAULT_REMOTE_SERVER_URL,
      token: "invite-token",
      displayName: "Bob",
      password: "a-strong-password",
      deviceName: "Image Annotation Mobile",
    });
    expect(invoke).toHaveBeenCalledWith("remote_request", {
      method: "POST",
      path: "/api/v1/projects/remote-project/invitations",
      body: { email: "bob@example.test", role: "annotator" },
    });
  });

  it("bootstraps project cache through the native command", async () => {
    vi.mocked(invoke).mockImplementation(async (command) => command === "auth_status"
      ? { authenticated: true, serverUrl: DEFAULT_REMOTE_SERVER_URL, accountId: "alice", deviceId: "device-1" }
      : {});
    await remoteProjectOpen("project-1");
    expect(invoke).toHaveBeenLastCalledWith("remote_project_open", {
      serverUrl: DEFAULT_REMOTE_SERVER_URL,
      remoteProjectId: "project-1",
      mode: "cloud_linked",
      cachePolicy: "on_demand",
    });
  });

  it("keeps remote collaboration paths scoped to the remote project and task ids", async () => {
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "remote_request") return { items: [] };
      return { authenticated: true, serverUrl: DEFAULT_REMOTE_SERVER_URL, accountId: "alice", deviceId: "device-1" };
    });
    await listRemoteProjects();
    await listRemoteTasks("remote-project");
    await listRemoteTaskItems("remote-task");
    await claimRemoteTaskItem("remote-task", "remote-image");
    await releaseRemoteTaskItem("remote-task", "remote-image");
    await listRemoteReviewQueue("remote-project");
    await reviewRemoteItem("remote-project", "remote-image", "approve", "ok", 7);
    await revokeRemoteDevice("device-row");

    expect(invoke).toHaveBeenCalledWith("remote_request", expect.objectContaining({
      method: "GET",
      path: "/api/v1/projects/remote-project/tasks",
    }));
    expect(invoke).toHaveBeenCalledWith("remote_request", expect.objectContaining({
      method: "POST",
      path: "/api/v1/tasks/remote-task/items/remote-image/claim",
    }));
    expect(invoke).toHaveBeenCalledWith("remote_request", expect.objectContaining({
      method: "POST",
      path: "/api/v1/projects/remote-project/images/remote-image/review",
      body: expect.objectContaining({decision: "approved", annotationRevision: 7, operationId: expect.any(String)}),
    }));
    expect(invoke).toHaveBeenCalledWith("remote_request", expect.objectContaining({
      method: "DELETE",
      path: "/api/v1/auth/devices/device-row",
    }));
  });

  it("normalizes the account profile response without exposing credentials", async () => {
    vi.mocked(invoke).mockResolvedValue({ id: "user-1", email: "alice@example.test", displayName: "Alice", devices: [] });
    await getAccountProfile();
    expect(invoke).toHaveBeenCalledWith("remote_request", {
      method: "GET",
      path: "/api/v1/auth/me",
      body: null,
    });
  });
});

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { CollaborationPanel } from "./CollaborationPanel";
import {
  authAcceptInvitation,
  authLogin,
  authStatus,
  createRemoteInvitation,
  defaultRemoteServerUrl,
  getAccountProfile,
  listRemoteProjects,
} from "../../api/mobile";
import { publishProject } from "../../api/tauri";

vi.mock("../../api/mobile", () => ({
  authAcceptInvitation: vi.fn(),
  authLogin: vi.fn(),
  authLogout: vi.fn(),
  authRefresh: vi.fn(),
  authStatus: vi.fn(),
  createRemoteInvitation: vi.fn(),
  defaultRemoteServerUrl: vi.fn(() => "https://desktop.example.test/image-annotation"),
  getAccountProfile: vi.fn(),
  listRemoteProjects: vi.fn(),
  remoteProjectOpen: vi.fn(),
  revokeRemoteDevice: vi.fn(),
}));

vi.mock("../../api/tauri", () => ({
  publishProject: vi.fn(),
}));

vi.mock("../hosting/DatasetHostingPanel", () => ({
  DatasetHostingPanel: () => <section aria-label="数据集托管测试面板">数据集托管内容</section>,
}));

const loggedOut = {
  authenticated: false,
  serverUrl: null,
  userId: null,
  displayName: null,
  deviceId: "device-desktop",
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(authStatus).mockResolvedValue(loggedOut);
  vi.mocked(authLogin).mockResolvedValue({ ...loggedOut, authenticated: true, serverUrl: "https://desktop.example.test/image-annotation" });
  vi.mocked(authAcceptInvitation).mockResolvedValue({ ...loggedOut, authenticated: true, serverUrl: "https://desktop.example.test/image-annotation", displayName: "受邀成员" });
  vi.mocked(listRemoteProjects).mockResolvedValue([]);
  vi.mocked(getAccountProfile).mockResolvedValue({ id: "user-1", email: "owner@example.test", displayName: "Owner", devices: [] });
  vi.mocked(createRemoteInvitation).mockResolvedValue({
    id: "invite-1",
    email: "member@example.test",
    role: "annotator",
    token: "invite-token-1",
    expiresAt: "2030-01-01T00:00:00.000Z",
  });
});

describe("CollaborationPanel invitations", () => {
  it("renders successful login feedback with success semantics", async () => {
    render(<CollaborationPanel onClose={vi.fn()} onOpenProject={vi.fn()} />);

    await waitFor(() => expect(authStatus).toHaveBeenCalled());
    fireEvent.change(screen.getByLabelText("用户名"), { target: { value: "owner" } });
    fireEvent.change(screen.getByLabelText("密码"), { target: { value: "a-strong-password" } });
    fireEvent.click(screen.getByRole("button", { name: "登录并发现项目" }));

    const notice = await screen.findByRole("status");
    expect(notice).toHaveTextContent("已登录，远端项目已加载");
    expect(notice).toHaveClass("collaboration-message--success");
  });

  it("accepts an invitation through the native auth command", async () => {
    render(<CollaborationPanel onClose={vi.fn()} onOpenProject={vi.fn()} />);

    await waitFor(() => expect(authStatus).toHaveBeenCalled());
    fireEvent.click(screen.getByRole("button", { name: "接受邀请", exact: true }));
    fireEvent.change(screen.getByLabelText("邀请码"), { target: { value: "invite-token" } });
    fireEvent.change(screen.getByLabelText("显示名"), { target: { value: "受邀成员" } });
    fireEvent.change(screen.getByLabelText("新密码"), { target: { value: "a-strong-password" } });
    fireEvent.click(screen.getByRole("button", { name: "接受邀请并发现项目" }));

    await waitFor(() => expect(authAcceptInvitation).toHaveBeenCalledWith({
      serverUrl: "https://desktop.example.test/image-annotation",
      token: "invite-token",
      displayName: "受邀成员",
      password: "a-strong-password",
      deviceName: "Image Annotation Desktop",
    }));
  });

  it("generates a manually shareable invitation only for a project manager", async () => {
    vi.mocked(authStatus).mockResolvedValue({ ...loggedOut, authenticated: true, serverUrl: "https://desktop.example.test/image-annotation" });
    vi.mocked(listRemoteProjects).mockResolvedValue([
      { id: "project-owner", name: "可邀请项目", role: "owner" },
      { id: "project-viewer", name: "只读项目", role: "viewer" },
    ]);

    render(<CollaborationPanel onClose={vi.fn()} onOpenProject={vi.fn()} />);
    await waitFor(() => expect(screen.getByText("可邀请项目")).toBeInTheDocument());
    expect(screen.getByLabelText("邀请邮箱-project-owner")).toBeInTheDocument();
    expect(screen.queryByLabelText("邀请邮箱-project-viewer")).not.toBeInTheDocument();

    fireEvent.change(screen.getByLabelText("邀请邮箱-project-owner"), { target: { value: "member@example.test" } });
    fireEvent.click(screen.getByRole("button", { name: "生成邀请码" }));

    await waitFor(() => expect(createRemoteInvitation).toHaveBeenCalledWith("project-owner", "member@example.test", "annotator"));
    expect(await screen.findByText("invite-token-1")).toBeInTheDocument();
  });

  it("publishes a local project and reopens the same local cache", async () => {
    vi.mocked(authStatus).mockResolvedValue({
      ...loggedOut,
      authenticated: true,
      serverUrl: "https://desktop.example.test/image-annotation",
    });
    vi.mocked(publishProject).mockResolvedValue({
      remoteProjectId: "remote-published",
      createdRemoteProject: true,
      uploadedAssets: 1,
      reusedAssets: 0,
      initializedAnnotations: 0,
      conflicts: 0,
      sync: {
        projectId: "local-project",
        pushed: 1,
        pulled: 0,
        conflicts: 0,
        failed: 0,
        cursor: "cursor-1",
      },
    });
    const onOpenProject = vi.fn();
    render(
      <CollaborationPanel
        localProjects={[{
          id: "local-project",
          name: "本地项目",
          description: "用于发布验收",
          annotationTypes: ["bbox"],
          imageCount: 1,
          annotatedPercent: 0,
          reviewCount: 0,
          issueCount: 0,
          classCount: 1,
          tagGroupCount: 0,
          status: "active",
          tags: [],
        }]}
        onClose={vi.fn()}
        onOpenProject={onOpenProject}
      />,
    );

    await waitFor(() => expect(screen.getByRole("button", { name: "发布到服务器" })).toBeInTheDocument());
    fireEvent.click(screen.getByRole("button", { name: "发布到服务器" }));

    await waitFor(() => expect(publishProject).toHaveBeenCalledWith("local-project", {
      serverUrl: "https://desktop.example.test/image-annotation",
      deviceId: "device-desktop",
      mode: "mirrored",
      cachePolicy: "on_demand",
    }));
    expect(await screen.findByRole("button", { name: "更新服务器" })).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "更新服务器" }));
    await waitFor(() => expect(publishProject).toHaveBeenCalledTimes(2));
    expect(publishProject).toHaveBeenNthCalledWith(2, "local-project", {
      serverUrl: "https://desktop.example.test/image-annotation",
      deviceId: "device-desktop",
      mode: "mirrored",
      cachePolicy: "on_demand",
    });

    fireEvent.click(screen.getByRole("button", { name: "打开已发布" }));
    expect(onOpenProject).toHaveBeenCalledWith("local-project", "remote-published");
  });

  it("exposes dataset hosting from an authenticated desktop collaboration session", async () => {
    vi.mocked(authStatus).mockResolvedValue({ ...loggedOut, authenticated: true, serverUrl: "https://desktop.example.test/image-annotation" });
    render(<CollaborationPanel onClose={vi.fn()} onOpenProject={vi.fn()} />);
    await waitFor(() => expect(screen.getByText("账号与远端项目")).toBeInTheDocument());
    fireEvent.click(screen.getByRole("button", { name: "打开数据集托管" }));
    expect(screen.getByRole("region", { name: "数据集托管测试面板" })).toBeInTheDocument();
  });
});

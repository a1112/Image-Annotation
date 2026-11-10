import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { MobileWorkspaceShell } from "./MobileWorkspaceShell";
import {
  authAcceptInvitation,
  authLogin,
  authLogout,
  authStatus,
  defaultRemoteServerUrl,
  deviceInfo,
  getAccountProfile,
  listRemoteProjects,
  offlineStatus,
  remoteProjectOpen,
  revokeRemoteDevice,
} from "../../api/mobile";

vi.mock("../../api/mobile", () => ({
  authAcceptInvitation: vi.fn(),
  authLogin: vi.fn(),
  authLogout: vi.fn(),
  authRefresh: vi.fn(),
  authStatus: vi.fn(),
  defaultRemoteServerUrl: vi.fn(() => "https://mobile.example.test/image-annotation"),
  deviceInfo: vi.fn(),
  flushOfflineQueue: vi.fn(),
  getAccountProfile: vi.fn(),
  listRemoteProjects: vi.fn(),
  offlineStatus: vi.fn(),
  remoteProjectOpen: vi.fn(),
  revokeRemoteDevice: vi.fn(),
}));

vi.mock("../../api/tauri", () => ({
  getProjectSyncSummary: vi.fn(),
  listDatasetProjects: vi.fn(),
  listIssues: vi.fn(),
  listSyncConflicts: vi.fn(),
  listTasks: vi.fn(),
  resolveSyncConflict: vi.fn(),
}));

vi.mock("../hosting/DatasetHostingPanel", () => ({
  DatasetHostingPanel: ({ onClose }: { onClose?: () => void }) => (
    <section aria-label="数据集托管测试面板">
      <span>数据集托管内容</span>
      {onClose ? <button type="button" onClick={onClose}>关闭数据集托管</button> : null}
    </section>
  ),
}));

const baseSession = {
  authenticated: false,
  serverUrl: null,
  userId: null,
  displayName: null,
  deviceId: "device-test",
};

beforeEach(async () => {
  vi.clearAllMocks();
  vi.mocked(authStatus).mockResolvedValue(baseSession);
  vi.mocked(deviceInfo).mockResolvedValue({ deviceId: "device-test", platform: "android", model: "test" });
  vi.mocked(offlineStatus).mockResolvedValue({
    online: false,
    queuedOperations: 2,
    conflictCount: 0,
    cachedAssets: 3,
    lastSyncAt: null,
    lastError: null,
  });
  vi.mocked(remoteProjectOpen).mockResolvedValue({ projectId: "project-1", offline: true });
  vi.mocked(listRemoteProjects).mockResolvedValue([]);
  vi.mocked(getAccountProfile).mockResolvedValue({ id: "user-1", email: "alice@example.test", displayName: "Alice", devices: [] });
  vi.mocked(revokeRemoteDevice).mockResolvedValue();
  const tauri = await import("../../api/tauri");
  vi.mocked(tauri.listDatasetProjects).mockResolvedValue([{
    id: "project-1",
    name: "移动项目",
    description: "",
    annotationTypes: ["BBox"],
    imageCount: 1,
    annotatedPercent: 0,
    reviewCount: 0,
    issueCount: 0,
    classCount: 1,
    tagGroupCount: 0,
    status: "进行中",
    tags: [],
  }]);
  vi.mocked(tauri.listTasks).mockResolvedValue([]);
  vi.mocked(tauri.listIssues).mockResolvedValue([]);
  vi.mocked(tauri.listSyncConflicts).mockResolvedValue([]);
  vi.mocked(tauri.getProjectSyncSummary).mockResolvedValue({
    projectId: "project-1",
    projectMode: "cloud_linked",
    pendingOperations: 2,
    failedOperations: 0,
    conflictCount: 0,
    lastPulledAt: null,
    lastPushedAt: null,
  });
});

describe("MobileWorkspaceShell", () => {
  it("provides bottom navigation without opening a second annotation window", async () => {
    const onNavigate = vi.fn();
    render(
      <MobileWorkspaceShell route="#/annotate/project-1/image-1" projectId="project-1" onNavigate={onNavigate}>
        <div>标注内容</div>
      </MobileWorkspaceShell>,
    );

    expect(screen.getByTestId("mobile-bottom-nav")).toHaveAccessibleName("移动端底部导航");
    await waitFor(() => expect(screen.getByText("移动项目")).toBeInTheDocument());
    fireEvent.click(within(screen.getByTestId("mobile-bottom-nav")).getByRole("button", { name: "任务" }));
    expect(onNavigate).toHaveBeenCalledWith("#/datasets/project-1/任务");
    expect(screen.getByText("标注内容")).toBeInTheDocument();
  });

  it("keeps annotation routes in the project section without mounting discovery cards", async () => {
    render(
      <MobileWorkspaceShell route="#/annotate/project-1/image-1" projectId="project-1" onNavigate={vi.fn()}>
        <main className="mobile-annotation-page">标注内容</main>
      </MobileWorkspaceShell>,
    );

    expect(screen.getByText("标注内容")).toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "远端项目" })).not.toBeInTheDocument();
    expect(screen.getByTestId("mobile-bottom-nav").querySelector("button.is-active")).toHaveTextContent("项目");
    expect(document.querySelector(".mobile-shell.is-annotation-route")).toBeInTheDocument();
    expect(document.querySelector(".mobile-shell-content.mobile-shell-content--annotation")).toBeInTheDocument();
    await waitFor(() => expect(document.querySelector(".mobile-sync-strip--annotation")).toBeInTheDocument());
    expect(screen.queryByRole("note")).not.toBeInTheDocument();
  });

  it.each([
    ["任务", "移动任务协同"],
    ["质检", "移动审阅协同"],
  ])("decodes percent-encoded Chinese %s routes for the native collaboration panel", async (tab, panelLabel) => {
    render(
      <MobileWorkspaceShell
        route={`#/datasets/project-1/${encodeURIComponent(tab)}`}
        projectId="project-1"
        onNavigate={vi.fn()}
      >
        <div>桌面内容</div>
      </MobileWorkspaceShell>,
    );

    expect(await screen.findByRole("region", { name: panelLabel })).toBeInTheDocument();
    expect(within(screen.getByTestId("mobile-bottom-nav")).getByRole("button", { name: tab === "任务" ? "任务" : "审阅" }))
      .toHaveClass("is-active");
    expect(screen.queryByRole("region", { name: "远端项目" })).not.toBeInTheDocument();
    expect(screen.queryByText("桌面内容")).not.toBeInTheDocument();
  });

  it("logs in through Rust session commands and keeps the server field explicit", async () => {
    vi.mocked(authLogin).mockResolvedValue({
      ...baseSession,
      authenticated: true,
      serverUrl: defaultRemoteServerUrl(),
      displayName: "移动标注员",
    });
    render(
      <MobileWorkspaceShell route="#/datasets" onNavigate={vi.fn()}>
        <div>项目列表</div>
      </MobileWorkspaceShell>,
    );

    await waitFor(() => expect(authStatus).toHaveBeenCalled());
    fireEvent.click(screen.getByRole("button", { name: "打开服务器与离线设置" }));
    const serverInput = screen.getByLabelText("服务器地址");
    const usernameInput = screen.getByLabelText("用户名");
    const passwordInput = screen.getByLabelText("密码");
    expect(serverInput).toHaveValue("https://mobile.example.test/image-annotation");
    expect(usernameInput).toHaveAttribute("type", "email");
    expect(usernameInput).toHaveAttribute("inputmode", "email");
    expect(usernameInput).toHaveAttribute("autocapitalize", "none");
    expect(usernameInput).toHaveAttribute("autocorrect", "off");
    expect(usernameInput).toHaveAttribute("spellcheck", "false");
    fireEvent.change(usernameInput, { target: { value: "alice@example.test" } });
    fireEvent.change(passwordInput, { target: { value: "secret" } });
    fireEvent.click(screen.getByRole("button", { name: "登录并连接" }));

    await waitFor(() => expect(authLogin).toHaveBeenCalledWith({
      serverUrl: "https://mobile.example.test/image-annotation",
      username: "alice@example.test",
      password: "secret",
    }));
    await waitFor(() => expect(screen.getAllByText("Alice").length).toBeGreaterThan(0));
  });

  it("discovers remote projects and navigates with the native local cache id", async () => {
    vi.mocked(authStatus).mockResolvedValue({
      ...baseSession,
      authenticated: true,
      serverUrl: defaultRemoteServerUrl(),
      displayName: "移动标注员",
    });
    vi.mocked(listRemoteProjects).mockResolvedValue([{
      id: "remote-project-1",
      name: "服务器项目",
      description: "跨设备项目",
      status: "active",
      role: "annotator",
    }]);
    vi.mocked(remoteProjectOpen).mockResolvedValue({
      projectId: "local-cache-1",
      localProjectId: "local-cache-1",
      remoteProjectId: "remote-project-1",
      offline: false,
    });
    const onNavigate = vi.fn();
    render(<MobileWorkspaceShell route="#/datasets" onNavigate={onNavigate}><div /></MobileWorkspaceShell>);
    await waitFor(() => expect(screen.getByText("服务器项目")).toBeInTheDocument());
    expect(screen.getByText("启用 · 标注员")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "打开并缓存" }));
    await waitFor(() => expect(remoteProjectOpen).toHaveBeenCalledWith("remote-project-1"));
    expect(onNavigate).toHaveBeenCalledWith("#/datasets/local-cache-1");
  });

  it("exposes a mobile-safe dataset hosting entry from the project page", async () => {
    vi.mocked(authStatus).mockResolvedValue({
      ...baseSession,
      authenticated: true,
      serverUrl: defaultRemoteServerUrl(),
      displayName: "移动管理员",
    });
    render(<MobileWorkspaceShell route="#/datasets" onNavigate={vi.fn()}><div /></MobileWorkspaceShell>);
    const hostingButton = await screen.findByRole("button", { name: "打开数据集托管" });
    fireEvent.click(hostingButton);
    expect(screen.getByRole("region", { name: "数据集托管测试面板" })).toBeInTheDocument();
  });

  it("accepts an invitation through the Rust session bridge and loads remote projects", async () => {
    vi.mocked(authAcceptInvitation).mockResolvedValue({
      ...baseSession,
      authenticated: true,
      serverUrl: defaultRemoteServerUrl(),
      displayName: "受邀成员",
    });
    vi.mocked(listRemoteProjects).mockResolvedValue([{
      id: "remote-project-1",
      name: "受邀项目",
      description: "邀请加入的协同项目",
      role: "annotator",
    }]);
    render(<MobileWorkspaceShell route="#/datasets" onNavigate={vi.fn()}><div /></MobileWorkspaceShell>);

    await waitFor(() => expect(authStatus).toHaveBeenCalled());
    fireEvent.click(screen.getByRole("button", { name: "打开服务器与离线设置" }));
    fireEvent.click(screen.getByRole("button", { name: "接受邀请", exact: true }));
    fireEvent.change(screen.getByLabelText("邀请码"), { target: { value: "invite-token" } });
    fireEvent.change(screen.getByLabelText("显示名"), { target: { value: "受邀成员" } });
    fireEvent.change(screen.getByLabelText("新密码"), { target: { value: "a-strong-password" } });
    fireEvent.click(screen.getByRole("button", { name: "接受邀请并连接" }));

    await waitFor(() => expect(authAcceptInvitation).toHaveBeenCalledWith({
      serverUrl: "https://mobile.example.test/image-annotation",
      token: "invite-token",
      displayName: "受邀成员",
      password: "a-strong-password",
      deviceName: "Image Annotation Mobile",
    }));
    await waitFor(() => expect(screen.getByText("受邀项目")).toBeInTheDocument());
  });

  it("clears the previous account cache and offline status after logout", async () => {
    const tauri = await import("../../api/tauri");
    const onNavigate = vi.fn();
    const sessionChange = vi.fn();
    window.addEventListener("image-annotation:session-change", sessionChange);
    vi.mocked(authStatus).mockResolvedValue({
      ...baseSession,
      authenticated: true,
      serverUrl: defaultRemoteServerUrl(),
      userId: "old-user",
      displayName: "旧账号",
    });
    vi.mocked(authLogout).mockResolvedValue();
    vi.mocked(listRemoteProjects).mockResolvedValue([{
      id: "old-remote-project",
      name: "旧账号项目",
      description: "上一账号的远端缓存",
      status: "active",
      role: "annotator",
    }]);
    vi.mocked(offlineStatus).mockResolvedValue({
      online: true,
      queuedOperations: 1,
      conflictCount: 0,
      cachedAssets: 4,
      lastSyncAt: null,
      lastError: "旧账号同步失败",
    });
    vi.mocked(tauri.listDatasetProjects)
      .mockResolvedValueOnce([{
        id: "old-local-cache",
        name: "旧账号缓存",
        description: "上一账号的本地缓存",
        annotationTypes: ["BBox"],
        imageCount: 1,
        annotatedPercent: 0,
        reviewCount: 0,
        issueCount: 0,
        classCount: 1,
        tagGroupCount: 0,
        status: "进行中",
        tags: [],
      }])
      .mockResolvedValueOnce([]);

    render(<MobileWorkspaceShell route="#/datasets" onNavigate={onNavigate}><div /></MobileWorkspaceShell>);
    await waitFor(() => expect(screen.getByText("旧账号项目")).toBeInTheDocument());
    fireEvent.click(screen.getByRole("button", { name: "打开服务器与离线设置" }));
    expect(screen.getByText("待上传 1")).toBeInTheDocument();
    expect(screen.getByText("旧账号同步失败")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "退出登录" }));

    await waitFor(() => expect(authLogout).toHaveBeenCalled());
    await waitFor(() => expect(screen.queryByText("旧账号项目")).not.toBeInTheDocument());
    expect(screen.getByText("待上传 0")).toBeInTheDocument();
    expect(screen.queryByText("旧账号同步失败")).not.toBeInTheDocument();
    expect(onNavigate).toHaveBeenCalledWith("#/datasets");
    expect(sessionChange).toHaveBeenCalledWith(expect.objectContaining({
      detail: expect.objectContaining({ authenticated: false }),
    }));
    window.removeEventListener("image-annotation:session-change", sessionChange);
  });
});

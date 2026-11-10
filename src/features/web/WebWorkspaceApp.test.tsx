import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import WebWorkspaceApp from "./WebWorkspaceApp";

const webApi = vi.hoisted(() => ({
  getWebSession: vi.fn(), loginWeb: vi.fn(), logoutWeb: vi.fn(), createWebDataset: vi.fn(),
  exportWebDataProject: vi.fn(), uploadWebDatasetImages: vi.fn(),
  WebApiError: class extends Error { constructor(public status: number, message: string) { super(message); } },
}));
const tauriApi = vi.hoisted(() => ({ configureBackendTransport: vi.fn(), invalidateImageAssets: vi.fn(), listDatasetProjects: vi.fn(), listProjectImages: vi.fn() }));
const projectApi = vi.hoisted(() => ({ listDataProjects: vi.fn(), listDataProjectDatasets: vi.fn(), getDataProjectDatasetGroups: vi.fn() }));
vi.mock("../../api/web", () => webApi);
vi.mock("../../api/tauri", () => tauriApi);
vi.mock("../../api/dataProjects", () => projectApi);
vi.mock("./ImageTransformsPanel", () => ({ ImageTransformsPanel: () => <section data-testid="image-transforms">图像处理</section> }));
vi.mock("../annotation/AnnotationWorkspace", () => ({
  AnnotationWorkspace: ({ projectId, imageId, onNavigateImage }: { projectId: string; imageId?: string; onNavigateImage?: (id: string) => void }) => <div data-testid="annotation-workspace">标注：{projectId}/{imageId ?? "first"}<button type="button" onClick={() => onNavigateImage?.("image-2")}>下一张</button></div>,
}));
vi.mock("../projects/ProjectWorkspace", () => ({
  ProjectWorkspace: ({ projectId, onAnnotate }: { projectId: string; onAnnotate: (id: string, imageId: string) => void }) => <div data-testid="dataset-management">图像管理：{projectId}<button type="button" onClick={() => onAnnotate(projectId, "image-1")}>标注第一张</button></div>,
}));
vi.mock("../data-projects/DataProjectHome", () => ({
  DataProjectHome: ({ dataProjectId, section, onNavigateProject, onOpen, onAnnotate, onCreateDataset, onUploadImages, onExportDataProject }: {
    dataProjectId?: string; section?: string; onNavigateProject: (id: string) => void;
    onOpen: (id: string) => void; onAnnotate: (project: { id: string }) => void;
    onCreateDataset: (id: string) => void; onUploadImages: (id: string, datasetId: string) => void;
    onExportDataProject: (id: string) => void;
  }) => dataProjectId ? <section data-testid="project-management">项目：{dataProjectId}/{section}
    <button type="button" onClick={() => onOpen("project-uuid::road-images")}>道路图像</button>
    <button type="button" onClick={() => onAnnotate({ id: "project-uuid::road-images" })}>开始标注</button>
    <button type="button" onClick={() => onCreateDataset(dataProjectId)}>新建数据集</button>
    <button type="button" onClick={() => onUploadImages(dataProjectId, "project-uuid::road-images")}>上传图像</button>
    <button type="button" onClick={() => onExportDataProject(dataProjectId)}>导出图像与标签清单</button>
  </section> : <section data-testid="projects-home"><button type="button" onClick={() => onNavigateProject("project-uuid")}>道路检测项目</button></section>,
}));
const session = { authenticated: true, user: { id: "user-1", email: "annotator@example.test", displayName: "标注员", organizationId: null }, deviceId: "device-1" };
const dataset = { id: "project-uuid::road-images", name: "道路图像", description: "", annotationTypes: ["BBox"], imageCount: 1, annotatedPercent: 0, reviewCount: 0, issueCount: 0, classCount: 1, tagGroupCount: 0, status: "ready", tags: [] };
beforeEach(() => {
  window.location.hash = "";
  vi.clearAllMocks();
  webApi.getWebSession.mockResolvedValue(session);
  webApi.logoutWeb.mockResolvedValue(undefined);
  webApi.exportWebDataProject.mockResolvedValue({ dataProjectId: "project-uuid", status: "succeeded", manifest: { tree: {} } });
  projectApi.listDataProjects.mockResolvedValue([{ id: "project-uuid", name: "道路检测项目" }]);
  projectApi.listDataProjectDatasets.mockResolvedValue([dataset]);
  projectApi.getDataProjectDatasetGroups.mockResolvedValue({ canEdit: true });
});

describe("three-part Web workspace", () => {
  it("keeps viewer deep links out of the editable annotation workspace", async () => {
    projectApi.getDataProjectDatasetGroups.mockResolvedValue({ canEdit: false });
    window.location.hash = "#/web/annotate/project-uuid%3A%3Aroad-images/image-1";
    render(<WebWorkspaceApp />);
    expect(await screen.findByText("此项目为只读，请返回数据管理浏览图像与标注。")).toBeInTheDocument();
    expect(screen.queryByTestId("annotation-workspace")).not.toBeInTheDocument();
  });

  it("logs in to a lightweight project entry without loading global images or datasets", async () => {
    webApi.getWebSession.mockResolvedValueOnce({ authenticated: false, user: null, deviceId: null });
    webApi.loginWeb.mockResolvedValueOnce(session);
    const user = userEvent.setup(); render(<WebWorkspaceApp />);
    await user.type(await screen.findByLabelText("邮箱"), "annotator@example.test");
    await user.type(screen.getByLabelText("密码"), "secret");
    await user.click(screen.getByRole("button", { name: "登录", exact: true }));
    expect(await screen.findByTestId("projects-home")).toBeInTheDocument();
    expect(window.location.hash).toBe("#/web/projects");
    expect(tauriApi.listDatasetProjects).not.toHaveBeenCalled();
    expect(tauriApi.listProjectImages).not.toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: "新建数据集" })).not.toBeInTheDocument();
  });

  it("opens a project and then dataset management separately from annotation", async () => {
    const user = userEvent.setup(); render(<WebWorkspaceApp />);
    await user.click(await screen.findByRole("button", { name: "道路检测项目" }));
    expect(await screen.findByTestId("project-management")).toHaveTextContent("项目：project-uuid/data");
    await user.click(screen.getByRole("button", { name: "道路图像" }));
    expect(await screen.findByTestId("dataset-management")).toBeInTheDocument();
    expect(window.location.hash).toContain("/projects/project-uuid/datasets/");
    expect(screen.getByTestId("image-transforms")).toBeInTheDocument();
    expect(screen.queryByTestId("annotation-workspace")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "标注第一张" }));
    expect(await screen.findByTestId("annotation-workspace")).toHaveTextContent("image-1");
    expect(screen.queryByTestId("image-transforms")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "返回数据管理" }));
    expect(await screen.findByTestId("dataset-management")).toBeInTheDocument();
  });

  it("retains the annotation workspace through native-compatible hashes and continuous image navigation", async () => {
    window.location.hash = "#/annotate/project-uuid%3A%3Aroad-images/image-1";
    const user = userEvent.setup(); render(<WebWorkspaceApp />);
    const workspace = await screen.findByTestId("annotation-workspace");
    await waitFor(() => expect(window.location.hash).toBe("#/web/annotate/project-uuid%3A%3Aroad-images/image-1"));
    window.location.hash = "#/annotate/project-uuid%3A%3Aroad-images/image-2";
    await waitFor(() => expect(workspace).toHaveTextContent("image-2"));
    expect(screen.getByTestId("annotation-workspace")).toBe(workspace);
    await user.click(screen.getByRole("button", { name: "下一张" }));
    expect(window.location.hash).toBe("#/web/annotate/project-uuid%3A%3Aroad-images/image-2");
  });

  it("creates datasets within the current project", async () => {
    window.location.hash = "#/web/projects/project-uuid/data";
    webApi.createWebDataset.mockResolvedValueOnce({ id: "project-uuid::new" });
    const user = userEvent.setup(); render(<WebWorkspaceApp />);
    await user.click(await screen.findByRole("button", { name: "新建数据集" }));
    await user.type(screen.getByLabelText("数据集名称"), "新样本");
    await user.click(screen.getByRole("button", { name: "创建数据集" }));
    await waitFor(() => expect(webApi.createWebDataset).toHaveBeenCalledWith(expect.objectContaining({ name: "新样本", dataProjectId: "project-uuid" })));
    expect(await screen.findByRole("status")).toHaveTextContent("数据集已创建");
  });

  it("uploads browser-selected files to the scoped dataset", async () => {
    window.location.hash = "#/web/projects/project-uuid/data";
    const user = userEvent.setup(); render(<WebWorkspaceApp />);
    await user.click(await screen.findByRole("button", { name: "上传图像" }));
    const input = await screen.findByLabelText("图像文件");
    const file = new File(["image-bytes"], "road.png", { type: "image/png" });
    fireEvent.change(input, { target: { files: [file] } });
    const buttons = screen.getAllByRole("button", { name: "上传图像", exact: true });
    await user.click(buttons[buttons.length - 1]);
    await waitFor(() => expect(webApi.uploadWebDatasetImages).toHaveBeenCalledWith(dataset.id, [file]));
    expect(projectApi.listDataProjectDatasets).toHaveBeenCalledWith("project-uuid");
  });

  it("exports the actual project manifest without a native file path", async () => {
    window.location.hash = "#/web/projects/project-uuid/settings";
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    const user = userEvent.setup(); render(<WebWorkspaceApp />);
    try {
      await user.click(await screen.findByRole("button", { name: "导出图像与标签清单" }));
      expect(webApi.exportWebDataProject).toHaveBeenCalledWith("project-uuid");
      expect(await screen.findByRole("status")).toHaveTextContent("已下载图像与标签清单");
    } finally { click.mockRestore(); }
  });
});

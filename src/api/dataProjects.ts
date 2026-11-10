import { invokeBackendCommand, isWebBackendTransport } from "./tauri";
import type {
  DataProjectBranch,
  DataProjectCloudMember,
  DataProjectCloudMemberRemoval,
  DataProjectCloudMemberRole,
  DataProject,
  DataProjectCommit,
  DataProjectCheckout,
  DataProjectGraphBundle,
  DataProjectMergePreview,
  DataProjectMergeResult,
  DataProjectRevertResult,
  DataProjectSyncReport,
  DataProjectSyncStatus,
  RemoteDataProject,
  DataProjectWorkingTree,
  DataProjectGroupState,
  DatasetGroup,
} from "../types/dataProjects";
import type { DatasetProject } from "../types/domain";

function asObject(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function numberOf(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function nullableStringOf(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

/**
 * Native cloud discovery returns RemoteDataProject.  The browser dispatcher
 * intentionally returns its privacy-safe WebDataProject summary instead, so
 * normalize both shapes at this boundary rather than teaching the UI about
 * transport-specific DTOs.
 */
function normalizeRemoteDataProject(value: unknown): RemoteDataProject {
  const item = asObject(value);
  if (!item || typeof item.id !== "string" || typeof item.name !== "string") {
    throw new Error("远端数据项目响应格式无效");
  }
  const datasetIds = Array.isArray(item.datasetIds)
    ? item.datasetIds.filter((datasetId): datasetId is string => typeof datasetId === "string")
    : [];
  return {
    id: item.id,
    organizationId: nullableStringOf(item.organizationId),
    ownerId: nullableStringOf(item.ownerId),
    name: item.name,
    description: typeof item.description === "string" ? item.description : "",
    status: typeof item.status === "string" ? item.status : "active",
    revision: numberOf(item.revision),
    role: typeof item.role === "string" ? item.role : "viewer",
    datasetCount: numberOf(item.datasetCount, datasetIds.length),
    refCount: numberOf(item.refCount),
    commitCount: numberOf(item.commitCount),
    updatedAt: typeof item.updatedAt === "string" ? item.updatedAt : "",
  };
}

function normalizeRemoteDataProjects(value: unknown): RemoteDataProject[] {
  if (!Array.isArray(value)) throw new Error("远端数据项目响应格式无效");
  return value.map(normalizeRemoteDataProject);
}

function normalizeGraphBundle(value: unknown): DataProjectGraphBundle {
  const item = asObject(value);
  if (!item) throw new Error("远端项目连接响应格式无效");
  const project = normalizeRemoteDataProject(item.project ?? item);
  return {
    project,
    members: Array.isArray(item.members) ? item.members : [],
    datasets: Array.isArray(item.datasets) ? item.datasets as DataProjectGraphBundle["datasets"] : [],
    commits: Array.isArray(item.commits) ? item.commits as DataProjectGraphBundle["commits"] : [],
    refs: Array.isArray(item.refs) ? item.refs as DataProjectGraphBundle["refs"] : [],
    objects: Array.isArray(item.objects) ? item.objects as DataProjectGraphBundle["objects"] : [],
    remoteRevision: numberOf(item.remoteRevision, project.revision),
  };
}

function normalizeSyncReport(value: unknown): DataProjectSyncReport {
  const item = asObject(value);
  if (
    !item
    || typeof item.localProjectId !== "string"
    || typeof item.remoteProjectId !== "string"
    || typeof item.remoteRevision !== "number"
    || !Number.isFinite(item.remoteRevision)
    || typeof item.outcome !== "string"
  ) {
    throw new Error("数据项目同步响应格式无效");
  }
  return {
    localProjectId: item.localProjectId,
    remoteProjectId: item.remoteProjectId,
    remoteRevision: item.remoteRevision,
    localHead: nullableStringOf(item.localHead),
    remoteHead: nullableStringOf(item.remoteHead),
    outcome: item.outcome,
    trackingRef: nullableStringOf(item.trackingRef),
  };
}

export async function listDataProjects(): Promise<DataProject[]> {
  return invokeBackendCommand<DataProject[]>("list_data_projects");
}

export async function createDataProject(name: string, description: string): Promise<DataProject> {
  return invokeBackendCommand<DataProject>("create_data_project", { name, description });
}

export async function listDataProjectDatasets(dataProjectId: string): Promise<DatasetProject[]> {
  const value = await invokeBackendCommand<DatasetProject[]>("list_data_project_datasets", { dataProjectId });
  if (!Array.isArray(value)) throw new Error("项目数据集响应格式无效");
  return value;
}

export async function getDataProjectDatasetGroups(dataProjectId: string, branch?: string): Promise<DataProjectGroupState> {
  const result = await invokeBackendCommand<DataProjectGroupState>("get_data_project_dataset_groups", { dataProjectId, branch: branch ?? null });
  return validateGroupState(result, dataProjectId, branch);
}

export async function setDataProjectDatasetGroups(
  dataProjectId: string,
  branch: string,
  expectedRevision: number,
  groups: DatasetGroup[],
): Promise<DataProjectGroupState> {
  const result = await invokeBackendCommand<DataProjectGroupState>("set_data_project_dataset_groups", { dataProjectId, branch, expectedRevision, groups });
  return validateGroupState(result, dataProjectId, branch);
}

function validateGroupState(value: DataProjectGroupState, dataProjectId: string, branch?: string): DataProjectGroupState {
  if (!value || value.dataProjectId !== dataProjectId || typeof value.branch !== "string"
    || (branch !== undefined && value.branch !== branch)
    || !Number.isSafeInteger(value.revision) || value.revision < 0
    || value.projectMetadata?.schemaVersion !== 1 || !Array.isArray(value.projectMetadata.datasetGroups)
    || typeof value.canEdit !== "boolean") {
    throw new Error("项目分组响应格式或作用域无效，请刷新后重试");
  }
  return value;
}

export async function assignDatasetToDataProject(
  dataProjectId: string | null,
  datasetId: string,
): Promise<void> {
  await invokeBackendCommand<void>("assign_dataset_to_data_project", {
    dataProjectId,
    datasetId,
  });
}

export async function getDataProjectWorkingTree(dataProjectId: string): Promise<DataProjectWorkingTree> {
  return invokeBackendCommand<DataProjectWorkingTree>("get_data_project_working_tree", { dataProjectId });
}

export async function commitDataProject(dataProjectId: string, branch: string, message: string): Promise<DataProjectCommit> {
  return invokeBackendCommand<DataProjectCommit>("commit_data_project_branch", { dataProjectId, branch, message });
}

export async function listDataProjectCommits(dataProjectId: string, branch?: string): Promise<DataProjectCommit[]> {
  return invokeBackendCommand<DataProjectCommit[]>("list_data_project_version_commits", { dataProjectId, branch: branch ?? null });
}

export async function listDataProjectBranches(dataProjectId: string): Promise<DataProjectBranch[]> {
  return invokeBackendCommand<DataProjectBranch[]>("list_data_project_branches", { dataProjectId });
}

export async function createDataProjectBranch(
  dataProjectId: string,
  name: string,
  fromCommitId?: string,
): Promise<DataProjectBranch> {
  return invokeBackendCommand<DataProjectBranch>("create_data_project_branch", {
    dataProjectId,
    name,
    fromCommitId: fromCommitId ?? null,
  });
}

export async function switchDataProjectBranch(dataProjectId: string, branchName: string): Promise<DataProjectCheckout> {
  return invokeBackendCommand<DataProjectCheckout>("switch_data_project_branch", {
    dataProjectId,
    branch: branchName,
  });
}

export async function deleteDataProjectBranch(dataProjectId: string, branchName: string): Promise<void> {
  await invokeBackendCommand<void>("delete_data_project_branch", { dataProjectId, branch: branchName });
}

export async function previewDataProjectMerge(
  dataProjectId: string,
  sourceBranch: string,
  targetBranch: string,
): Promise<DataProjectMergePreview> {
  return invokeBackendCommand<DataProjectMergePreview>("preview_data_project_merge", {
    dataProjectId,
    oursBranch: targetBranch,
    theirsBranch: sourceBranch,
  });
}

export async function mergeDataProjectBranches(
  dataProjectId: string,
  sourceBranch: string,
  targetBranch: string,
  message: string,
): Promise<DataProjectMergeResult> {
  return invokeBackendCommand<DataProjectMergeResult>("merge_data_project_branches", {
    dataProjectId,
    oursBranch: targetBranch,
    theirsBranch: sourceBranch,
    message,
  });
}

export async function resolveDataProjectMerge(
  dataProjectId: string,
  sessionId: string,
  conflictKey: string,
  resolution: "ours" | "theirs",
): Promise<DataProjectMergeResult> {
  return invokeBackendCommand<DataProjectMergeResult>("resolve_data_project_merge", {
    dataProjectId,
    sessionId,
    conflictKey,
    resolution,
  });
}

export async function revertDataProject(
  dataProjectId: string,
  branch: string,
  commitId: string,
  message: string,
): Promise<DataProjectRevertResult> {
  return invokeBackendCommand<DataProjectRevertResult>("revert_data_project_commit", {
    dataProjectId,
    branch,
    commitId,
    message,
  });
}

export async function getDataProjectSyncStatus(dataProjectId: string): Promise<DataProjectSyncStatus> {
  return invokeBackendCommand<DataProjectSyncStatus>("get_data_project_sync_status", { dataProjectId });
}

export async function publishDataProject(dataProjectId: string): Promise<DataProjectSyncReport> {
  return normalizeSyncReport(await invokeBackendCommand<unknown>("publish_data_project", { dataProjectId }));
}

export async function discoverDataProjectRemotes(): Promise<RemoteDataProject[]> {
  // The Web dispatcher exposes the privacy-safe discovery alias rather than
  // the native HTTP client command name.  Keep native IPC unchanged.
  const command = isWebBackendTransport() ? "discover_data_project_remotes" : "list_remote_data_projects";
  return normalizeRemoteDataProjects(await invokeBackendCommand<unknown>(command));
}

export async function createRemoteDataProject(name: string, description: string, organizationId?: string | null): Promise<RemoteDataProject> {
  return normalizeRemoteDataProject(await invokeBackendCommand<unknown>("create_remote_data_project", {
    name,
    description,
    organizationId: organizationId ?? null,
  }));
}

export async function connectRemoteDataProject(localProjectId: string, remoteProjectId: string): Promise<DataProjectGraphBundle> {
  return normalizeGraphBundle(await invokeBackendCommand<unknown>("connect_remote_data_project", {
    dataProjectId: localProjectId,
    remoteProjectId,
  }));
}

export async function cloneDataProject(remoteProjectId: string): Promise<DataProjectSyncReport> {
  const value = await invokeBackendCommand<unknown>("clone_remote_data_project", { remoteProjectId });
  const item = asObject(value);
  // Until the Web dispatcher includes the nested project summary in its clone
  // report, it may return a null revision. Recover the authoritative revision
  // through the already-authenticated discovery command; never substitute 0
  // or mark an unknown server state as successfully synchronized.
  if (isWebBackendTransport() && item && item.remoteRevision == null) {
    const remotes = await discoverDataProjectRemotes();
    const remote = remotes.find((candidate) => candidate.id === remoteProjectId);
    if (!remote) throw new Error("克隆响应缺少远端修订，且无法重新读取远端项目");
    return normalizeSyncReport({ ...item, remoteRevision: remote.revision });
  }
  return normalizeSyncReport(value);
}

export async function syncDataProject(dataProjectId: string): Promise<DataProjectSyncReport> {
  return normalizeSyncReport(await invokeBackendCommand<unknown>("sync_data_project", { dataProjectId }));
}

export async function listDataProjectCloudMembers(dataProjectId: string): Promise<DataProjectCloudMember[]> {
  return invokeBackendCommand<DataProjectCloudMember[]>("list_data_project_cloud_members", { dataProjectId });
}

export async function upsertDataProjectCloudMember(
  dataProjectId: string,
  userId: string,
  role: string,
): Promise<DataProjectCloudMemberRole> {
  return invokeBackendCommand<DataProjectCloudMemberRole>("upsert_data_project_cloud_member", {
    dataProjectId,
    userId,
    role,
  });
}

export async function removeDataProjectCloudMember(
  dataProjectId: string,
  userId: string,
): Promise<DataProjectCloudMemberRemoval> {
  return invokeBackendCommand<DataProjectCloudMemberRemoval>("remove_data_project_cloud_member", {
    dataProjectId,
    userId,
  });
}

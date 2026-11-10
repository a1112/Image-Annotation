import { projectTabs, type ProjectTab } from "./routes";
import type { DataProjectSection } from "../types/dataProjects";

export type WorkspaceRoute =
  | { kind: "home" }
  | { kind: "dataProject"; dataProjectId: string; section: DataProjectSection }
  | { kind: "dataset"; dataProjectId?: string; datasetId: string; tab?: ProjectTab }
  | { kind: "annotate"; datasetId: string; imageId?: string }
  | { kind: "backendTasks" };

const sections: DataProjectSection[] = ["data", "versions", "collaboration", "settings"];

export function parseWorkspaceRoute(hash: string): WorkspaceRoute {
  try {
    const parts = hash.replace(/^#\/?/, "").split("/").filter(Boolean);
    if (parts[0] === "web") parts.shift();
    const decoded = parts.map(decodeURIComponent);
    if (decoded[0] === "annotate" && decoded[1]) {
      return { kind: "annotate", datasetId: decoded[1], imageId: decoded[2] };
    }
    if (decoded[0] === "backend-tasks") return { kind: "backendTasks" };
    if (decoded[0] === "projects" && decoded[1]) {
      if (decoded[2] === "datasets" && decoded[3]) {
        return { kind: "dataset", dataProjectId: decoded[1], datasetId: decoded[3], tab: projectTabs.includes(decoded[4] as ProjectTab) ? decoded[4] as ProjectTab : "图片" };
      }
      return { kind: "dataProject", dataProjectId: decoded[1], section: sections.includes(decoded[2] as DataProjectSection) ? decoded[2] as DataProjectSection : "data" };
    }
    if ((decoded[0] === "datasets" || decoded[0] === "dataset") && decoded[1]) {
      return { kind: "dataset", datasetId: decoded[1], tab: projectTabs.includes(decoded[2] as ProjectTab) ? decoded[2] as ProjectTab : undefined };
    }
  } catch { /* Malformed deep links return to the safe project entry. */ }
  return { kind: "home" };
}

export function workspaceRouteHash(route: WorkspaceRoute, web = false): string {
  const prefix = web ? "#/web" : "#";
  const encode = encodeURIComponent;
  switch (route.kind) {
    case "home": return `${prefix}/projects`;
    case "dataProject": return `${prefix}/projects/${encode(route.dataProjectId)}/${route.section}`;
    case "dataset": return route.dataProjectId
      ? `${prefix}/projects/${encode(route.dataProjectId)}/datasets/${encode(route.datasetId)}/${encode(route.tab ?? "图片")}`
      : `${prefix}/datasets/${encode(route.datasetId)}${route.tab ? `/${encode(route.tab)}` : ""}`;
    case "annotate": return `${prefix}/annotate/${encode(route.datasetId)}${route.imageId ? `/${encode(route.imageId)}` : ""}`;
    case "backendTasks": return `${prefix}/backend-tasks`;
  }
}

const returnPrefix = "image-annotation:annotation-return:";

export function rememberAnnotationReturn(datasetId: string, route: string): void {
  const parsed = parseWorkspaceRoute(route);
  if (parsed.kind === "annotate" || !route.startsWith("#/")) return;
  try { window.sessionStorage.setItem(returnPrefix + datasetId, route); } catch { /* Storage is optional. */ }
}

export function annotationReturnRoute(datasetId: string, web = false): string {
  try {
    const stored = window.sessionStorage.getItem(returnPrefix + datasetId);
    if (stored && stored.startsWith("#/") && parseWorkspaceRoute(stored).kind !== "annotate") return stored;
  } catch { /* Resolve a useful fallback from the canonical web identity. */ }
  const separator = datasetId.indexOf("::");
  return separator > 0
    ? workspaceRouteHash({ kind: "dataProject", dataProjectId: datasetId.slice(0, separator), section: "data" }, web)
    : workspaceRouteHash({ kind: "home" }, web);
}

export function clearAnnotationReturnRoutes(): void {
  try {
    for (let index = window.sessionStorage.length - 1; index >= 0; index -= 1) {
      const key = window.sessionStorage.key(index);
      if (key?.startsWith(returnPrefix)) window.sessionStorage.removeItem(key);
    }
  } catch { /* Account transitions must never depend on optional UI storage. */ }
}

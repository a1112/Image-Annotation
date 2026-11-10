export type DatasetViewState = {
  page: number;
  classFilter: string;
  statusFilter: string;
  folderFilter: string;
  selectedImageId: string | null;
  scrollTop: number;
};

const prefix = "image-annotation:dataset-view:";
const empty: DatasetViewState = { page: 0, classFilter: "all", statusFilter: "all", folderFilter: "all", selectedImageId: null, scrollTop: 0 };

export function datasetViewStateKey(datasetId: string, scope = "local"): string {
  return prefix + encodeURIComponent(scope) + ":" + encodeURIComponent(datasetId);
}

export function readDatasetViewState(key: string): DatasetViewState {
  try {
    const value = JSON.parse(window.sessionStorage.getItem(key) ?? "null") as Partial<DatasetViewState> | null;
    if (!value) return { ...empty };
    return {
      page: Number.isInteger(value.page) && value.page! >= 0 ? value.page! : 0,
      classFilter: typeof value.classFilter === "string" ? value.classFilter : "all",
      statusFilter: typeof value.statusFilter === "string" ? value.statusFilter : "all",
      folderFilter: typeof value.folderFilter === "string" ? value.folderFilter : "all",
      selectedImageId: typeof value.selectedImageId === "string" ? value.selectedImageId : null,
      scrollTop: typeof value.scrollTop === "number" && Number.isFinite(value.scrollTop) ? Math.max(0, value.scrollTop) : 0,
    };
  } catch { return { ...empty }; }
}

export function saveDatasetViewState(key: string, state: DatasetViewState): void {
  try { window.sessionStorage.setItem(key, JSON.stringify(state)); } catch { /* Optional view state. */ }
}

export function clearDatasetViewStates(): void {
  try {
    for (let index = window.sessionStorage.length - 1; index >= 0; index -= 1) {
      const key = window.sessionStorage.key(index);
      if (key?.startsWith(prefix)) window.sessionStorage.removeItem(key);
    }
  } catch { /* Session cleanup must remain available without storage. */ }
}

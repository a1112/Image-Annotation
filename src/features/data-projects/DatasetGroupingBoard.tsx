import { ChevronDown, GripVertical, MoreHorizontal, Pencil, Plus, Undo2 } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent, ReactNode } from "react";
import type { DatasetProject } from "../../types/domain";
import type { DatasetGroup } from "../../types/dataProjects";
import { DatasetCard } from "../datasets/DatasetCard";
import type { DatasetImage } from "../../types/domain";

export type DatasetGroupingBoardProps = {
  datasets: DatasetProject[];
  projectImages?: Record<string, DatasetImage[]>;
  groups: DatasetGroup[];
  canEdit: boolean;
  saving?: boolean;
  stateVersion?: string;
  toolbarContent?: ReactNode;
  toolbarActions?: ReactNode;
  onPersist: (groups: DatasetGroup[]) => Promise<void> | void;
  onOpen: (datasetId: string) => void;
  onAnnotate: (dataset: DatasetProject) => void;
  onOpenWindow?: (dataset: DatasetProject) => void;
  loadPreviewImages?: (datasetId: string) => Promise<DatasetImage[]>;
};

type DragSource =
  | { kind: "dataset"; datasetKey: string; groupId?: string }
  | { kind: "group"; groupId: string }
  | null;

type BoardGroup = DatasetGroup & { id: string; name: string; datasetKeys: string[] };

/** The server stores Web dataset keys without the project UUID prefix. */
export function canonicalDatasetKey(datasetId: string): string {
  const separator = datasetId.indexOf("::");
  return separator >= 0 ? datasetId.slice(separator + 2) : datasetId;
}

function groupIdFor(groups: BoardGroup[], datasetKey: string): string | undefined {
  return groups.find((group) => group.datasetKeys.includes(datasetKey))?.id;
}

function cloneGroups(groups: BoardGroup[]): BoardGroup[] {
  return groups.map((group) => ({ ...group, datasetKeys: [...group.datasetKeys] }));
}

function groupsSignature(groups: DatasetGroup[]): string {
  return JSON.stringify(groups.map((group) => ({
    id: group.id,
    name: group.name,
    datasetKeys: [...group.datasetKeys],
    position: group.position,
  })));
}

function equalGroups(left: BoardGroup[], right: BoardGroup[]): boolean {
  return JSON.stringify(left.map((group) => ({ id: group.id, name: group.name, datasetKeys: group.datasetKeys })))
    === JSON.stringify(right.map((group) => ({ id: group.id, name: group.name, datasetKeys: group.datasetKeys })));
}

function groupName(name: string, fallback: string): string {
  const trimmed = name.trim();
  return trimmed || fallback;
}

export function DatasetGroupingBoard({
  datasets,
  projectImages = {},
  groups: inputGroups,
  canEdit,
  saving = false,
  stateVersion,
  toolbarContent,
  toolbarActions,
  onPersist,
  onOpen,
  onAnnotate,
  onOpenWindow,
  loadPreviewImages,
}: DatasetGroupingBoardProps) {
  const [groups, setGroups] = useState<BoardGroup[]>(() => cloneGroups(inputGroups as BoardGroup[]));
  const [editingGroupId, setEditingGroupId] = useState<string | null>(null);
  const [editingName, setEditingName] = useState("");
  const [dragSource, setDragSource] = useState<DragSource>(null);
  const [undoGroups, setUndoGroups] = useState<BoardGroup[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(() => new Set());
  const [menuGroupId, setMenuGroupId] = useState<string | null>(null);
  const [groupPickerOpen, setGroupPickerOpen] = useState(false);
  const [pickedKeys, setPickedKeys] = useState<Set<string>>(() => new Set());
  const [dropHint, setDropHint] = useState<string | null>(null);
  const [dropTarget, setDropTarget] = useState<string | null>(null);
  const suppressClickRef = useRef(false);
  const editingIdRef = useRef<string | null>(null);
  const pointerDownRef = useRef<{ source: DragSource; x: number; y: number } | null>(null);
  const localPersistedSignatureRef = useRef<string | null>(null);

  const inputGroupSignature = useMemo(() => groupsSignature(inputGroups), [inputGroups]);
  useEffect(() => {
    const isLocalEcho = localPersistedSignatureRef.current === inputGroupSignature;
    localPersistedSignatureRef.current = null;
    setGroups(cloneGroups(inputGroups as BoardGroup[]));
    if (!isLocalEcho) {
      setUndoGroups(null);
      setEditingGroupId(null);
      editingIdRef.current = null;
    }
  }, [inputGroupSignature, stateVersion]);

  useEffect(() => {
    if (!dragSource) return;
    const cancelPointer = () => {
      pointerDownRef.current = null;
      setDragSource(null);
      setDropHint(null);
      setDropTarget(null);
    };
    window.addEventListener("pointerup", cancelPointer);
    window.addEventListener("pointercancel", cancelPointer);
    return () => {
      window.removeEventListener("pointerup", cancelPointer);
      window.removeEventListener("pointercancel", cancelPointer);
    };
  }, [dragSource]);

  const datasetByKey = useMemo(() => {
    const result = new Map<string, DatasetProject>();
    datasets.forEach((dataset) => {
      result.set(canonicalDatasetKey(dataset.id), dataset);
      result.set(dataset.id, dataset);
    });
    return result;
  }, [datasets]);

  const normalizedGroups = useMemo(() => {
    const known = new Set<string>();
    return groups.map((group) => ({
      ...group,
      datasetKeys: group.datasetKeys.filter((key) => {
        if (known.has(key)) return false;
        known.add(key);
        return datasetByKey.has(key) || datasetByKey.has(canonicalDatasetKey(key));
      }),
    }));
  }, [datasetByKey, groups]);

  const groupedKeys = useMemo(() => new Set(normalizedGroups.flatMap((group) => group.datasetKeys)), [normalizedGroups]);
  const ungrouped = datasets.filter((dataset) => !groupedKeys.has(canonicalDatasetKey(dataset.id)));

  function setLocalGroups(next: BoardGroup[], previous: BoardGroup[] = groups) {
    setUndoGroups(cloneGroups(previous));
    setGroups(cloneGroups(next));
    setError(null);
  }

  async function persist(next: BoardGroup[], previous: BoardGroup[] = groups) {
    if (!canEdit || saving || equalGroups(next, previous)) return false;
    next = next.map((group, position) => ({ ...group, position }));
    localPersistedSignatureRef.current = groupsSignature(next);
    setLocalGroups(next, previous);
    try {
      await onPersist(next.map((group) => ({ ...group, datasetKeys: [...group.datasetKeys] })));
      return true;
    } catch (cause) {
      localPersistedSignatureRef.current = null;
      setGroups(cloneGroups(previous));
      setUndoGroups(null);
      setError(cause instanceof Error ? cause.message : String(cause));
      return false;
    }
  }

  function beginRename(group: BoardGroup) {
    if (!canEdit || saving) return;
    editingIdRef.current = group.id;
    setEditingGroupId(group.id);
    setEditingName(group.name);
  }

  async function finishRename(group: BoardGroup) {
    if (editingIdRef.current !== group.id) return;
    editingIdRef.current = null;
    const name = groupName(editingName, "未命名分组");
    setEditingGroupId(null);
    if (name === group.name) return;
    const next = normalizedGroups.map((candidate) => candidate.id === group.id ? { ...candidate, name } : candidate);
    await persist(next, groups);
  }

  async function createFromKeys(keys: string[]) {
    if (!canEdit || saving || !keys.length) return;
    const id = typeof crypto.randomUUID === "function" ? crypto.randomUUID() : `group-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const picked = new Set(keys);
    const remaining = cloneGroups(normalizedGroups).map((group) => ({ ...group, datasetKeys: group.datasetKeys.filter((key) => !picked.has(key)) })).filter((group) => group.datasetKeys.length > 0);
    const next: BoardGroup[] = [...remaining, {
      id, name: "新分组", datasetKeys: [...new Set(keys)],
      position: Math.max(-1, ...normalizedGroups.map((group) => group.position)) + 1,
    }];
    if (await persist(next, groups)) {
      setGroupPickerOpen(false);
      setPickedKeys(new Set());
      setExpandedGroups((current) => new Set([...current, id]));
      editingIdRef.current = id;
      setEditingGroupId(id);
      setEditingName("新分组");
    }
  }

  async function createGroup(datasetKey?: string) {
    if (!canEdit || saving) return;
    if (datasetKey) await createFromKeys([datasetKey]);
    else { setPickedKeys(new Set()); setGroupPickerOpen(true); }
  }

  async function moveDataset(datasetKey: string, targetGroupId: string | null) {
    const currentGroupId = groupIdFor(normalizedGroups, datasetKey);
    if ((currentGroupId ?? null) === targetGroupId) return;
    const next = cloneGroups(normalizedGroups);
    if (currentGroupId) {
      const current = next.find((group) => group.id === currentGroupId);
      if (current) current.datasetKeys = current.datasetKeys.filter((key) => key !== datasetKey);
    }
    if (targetGroupId) {
      const target = next.find((group) => group.id === targetGroupId);
      if (!target || target.datasetKeys.includes(datasetKey)) return;
      target.datasetKeys.push(datasetKey);
    }
    await persist(next.filter((group) => group.datasetKeys.length > 0), groups);
  }

  async function mergeGroups(sourceGroupId: string, targetGroupId: string) {
    if (sourceGroupId === targetGroupId) return;
    const source = normalizedGroups.find((group) => group.id === sourceGroupId);
    const target = normalizedGroups.find((group) => group.id === targetGroupId);
    if (!source || !target) return;
    const next = normalizedGroups
      .filter((group) => group.id !== sourceGroupId)
      .map((group) => group.id === targetGroupId
        ? { ...group, datasetKeys: [...group.datasetKeys, ...source.datasetKeys.filter((key) => !group.datasetKeys.includes(key))] }
        : { ...group, datasetKeys: [...group.datasetKeys] });
    await persist(next, groups);
  }

  async function dissolveGroup(groupId: string) {
    const group = normalizedGroups.find((candidate) => candidate.id === groupId);
    if (!group) return;
    await persist(normalizedGroups.filter((candidate) => candidate.id !== groupId), groups);
  }

  async function drop(source: DragSource, target: { kind: "group" | "dataset" | "ungrouped"; id?: string; groupId?: string }) {
    if (!source || !canEdit || saving) return;
    if (source.kind === "group") {
      if (target.kind === "group" && target.id) await mergeGroups(source.groupId, target.id);
      else if (target.kind === "dataset" && target.groupId) await mergeGroups(source.groupId, target.groupId);
      return;
    }
    if (target.kind === "group" && target.id) {
      await moveDataset(source.datasetKey, target.id);
    } else if (target.kind === "ungrouped") {
      await moveDataset(source.datasetKey, null);
    } else if (target.kind === "dataset" && target.id) {
      const targetGroupId = target.groupId ?? groupIdFor(normalizedGroups, target.id);
      if (targetGroupId) {
        await moveDataset(source.datasetKey, targetGroupId);
      } else if (target.id !== source.datasetKey) {
        await createFromKeys([target.id, source.datasetKey]);
      }
    }
  }

  function pointerDown(event: ReactPointerEvent<HTMLElement>, source: DragSource) {
    // Touch/pen PointerEvents report button=-1; only secondary/middle mouse
    // buttons should be ignored.
    if (!canEdit || saving || event.button > 0) return;
    const target = event.target as HTMLElement;
    if (target.closest("button, input, select, summary")) return;
    suppressClickRef.current = false;
    pointerDownRef.current = { source, x: event.clientX, y: event.clientY };
    setDragSource(source);
  }

  function pointerUp(event: ReactPointerEvent<HTMLElement>, target: { kind: "group" | "dataset" | "ungrouped"; id?: string; groupId?: string }) {
    const start = pointerDownRef.current;
    pointerDownRef.current = null;
    setDragSource(null);
    setDropHint(null);
    setDropTarget(null);
    if (!start) return;
    if (Math.abs(event.clientX - start.x) + Math.abs(event.clientY - start.y) < 6) return;
    event.preventDefault();
    suppressClickRef.current = true;
    window.setTimeout(() => { suppressClickRef.current = false; }, 0);
    void drop(start.source, target);
  }

  function pointerMove(event: ReactPointerEvent<HTMLElement>) {
    const start = pointerDownRef.current;
    if (!start?.source || Math.abs(event.clientX - start.x) + Math.abs(event.clientY - start.y) < 6) return;
    const node = (event.target as HTMLElement).closest<HTMLElement>("[data-group-drop-kind]");
    const kind = node?.dataset.groupDropKind;
    const id = node?.dataset.groupDropId;
    const groupId = node?.dataset.groupDropParent;
    let hint: string | null = null;
    const source = start.source;
    if (source.kind === "group") {
      const targetId = kind === "group" ? id : groupId;
      if (targetId && source.groupId !== targetId) hint = "合并到此分组";
    } else if (kind === "ungrouped" && groupIdFor(normalizedGroups, source.datasetKey)) hint = "移到未分组";
    else if (kind === "group" && id && groupIdFor(normalizedGroups, source.datasetKey) !== id) hint = "移入此分组";
    else if (kind === "dataset" && id !== source.datasetKey) {
      if (groupId && groupIdFor(normalizedGroups, source.datasetKey) !== groupId) hint = "移入此分组";
      else if (!groupId) hint = "创建新分组";
    }
    setDropHint(hint);
    setDropTarget(hint ? `${kind}:${id ?? ""}` : null);
  }

  function toggleGroup(id: string) {
    setExpandedGroups((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id); else next.add(id);
      return next;
    });
  }

  async function undo() {
    if (!undoGroups || saving || !canEdit) return;
    const previous = cloneGroups(groups);
    const next = cloneGroups(undoGroups);
    setUndoGroups(null);
    localPersistedSignatureRef.current = groupsSignature(next);
    setGroups(next);
    try {
      await onPersist(next.map((group) => ({ ...group, datasetKeys: [...group.datasetKeys] })));
    } catch (cause) {
      setGroups(previous);
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }

  function renderDataset(dataset: DatasetProject, groupId?: string) {
    const key = canonicalDatasetKey(dataset.id);
    const managementMenu = canEdit ? <>
      {groupId ? <button type="button" disabled={saving} onClick={() => void moveDataset(key, null)}>移出分组</button> : <button type="button" disabled={saving} onClick={() => void createGroup(key)}>新建分组</button>}
      <label><span>移动到</span><select aria-label={`移动数据集 ${dataset.name}`} disabled={saving} value="" onChange={(event) => { if (event.target.value === "__ungrouped") void moveDataset(key, null); else if (event.target.value) void moveDataset(key, event.target.value); }}><option value="">选择分组</option>{groupId ? <option value="__ungrouped">未分组</option> : null}{normalizedGroups.filter((candidate) => candidate.id !== groupId).map((candidate) => <option value={candidate.id} key={candidate.id}>{candidate.name}</option>)}</select></label>
    </> : null;
    return <div
      className={`${groupId ? "data-projects-group-member" : "data-projects-group-dataset"} ${dragSource?.kind === "dataset" && dragSource.datasetKey === key ? "is-dragging" : ""} ${dropTarget === `dataset:${key}` ? "is-drop-target" : ""}`}
      key={dataset.id} draggable={false} data-group-drop-kind="dataset" data-group-drop-id={key} data-group-drop-parent={groupId}
      onPointerDown={(event) => { event.stopPropagation(); pointerDown(event, { kind: "dataset", datasetKey: key, groupId }); }}
      onPointerUp={(event) => { event.stopPropagation(); pointerUp(event, { kind: "dataset", id: key, groupId }); }}
    >
      <DatasetCard compact canAnnotate={canEdit} compactMenu={managementMenu} dataset={dataset} previewImages={projectImages[dataset.id] ?? []} loadPreviewImages={loadPreviewImages} onOpen={() => onOpen(dataset.id)} onAnnotate={() => onAnnotate(dataset)} onOpenWindow={() => onOpenWindow?.(dataset)} />
    </div>;
  }

  return <section className={`data-projects-groups-board${canEdit ? "" : " is-readonly"}`} aria-label="数据集分组" onPointerMove={pointerMove} onClickCapture={(event) => {
    if (suppressClickRef.current) { event.preventDefault(); event.stopPropagation(); suppressClickRef.current = false; }
  }}>
    <div className="data-projects-groups-toolbar"><h3>数据集</h3>{toolbarContent ? <div className="data-projects-groups-toolbar-content">{toolbarContent}</div> : null}<div className="data-projects-groups-actions">{toolbarActions}{canEdit ? <><button type="button" disabled={saving || !datasets.length} onClick={() => void createGroup()}><Plus size={14} />新建分组</button><button type="button" disabled={!undoGroups || saving} onClick={() => void undo()}><Undo2 size={14} />撤销</button></> : <span>只读</span>}</div></div>
    {dropHint ? <div className="data-projects-drop-hint" role="status">{dropHint}</div> : null}
    {error ? <div className="data-projects-feature-error" role="alert">{error}</div> : null}
    {groupPickerOpen ? <section className="data-projects-group-picker" role="dialog" aria-label="选择分组数据集"><strong>选择数据集</strong>{datasets.map((dataset) => {
      const key = canonicalDatasetKey(dataset.id);
      return <label key={key}><input type="checkbox" checked={pickedKeys.has(key)} onChange={(event) => setPickedKeys((current) => { const next = new Set(current); if (event.target.checked) next.add(key); else next.delete(key); return next; })} />{dataset.name}</label>;
    })}<div><button className="primary" type="button" disabled={!pickedKeys.size || saving} onClick={() => void createFromKeys([...pickedKeys])}>创建分组</button><button type="button" onClick={() => setGroupPickerOpen(false)}>取消</button></div></section> : null}
    <div className="data-projects-group-columns">
      <div className={`data-projects-group-column data-projects-ungrouped-column ${dropTarget === "ungrouped:" ? "is-drop-target" : ""}`} data-group-drop-kind="ungrouped" onPointerUp={(event) => pointerUp(event, { kind: "ungrouped" })}>
        <div className="data-projects-group-column-heading"><strong>未分组</strong><span>{ungrouped.length} 个数据集</span></div>
        <div className="data-projects-ungrouped-cards">{ungrouped.map((dataset) => renderDataset(dataset))}</div>
        {!ungrouped.length ? <div className="data-projects-group-empty" title="拖入这里可移出分组">未分组为空</div> : null}
      </div>
      <div className="data-projects-group-list" aria-label="分组列表">{normalizedGroups.map((group) => {
        const expanded = expandedGroups.has(group.id);
        const members = group.datasetKeys.map((key) => datasetByKey.get(key) ?? datasetByKey.get(canonicalDatasetKey(key))).filter((dataset): dataset is DatasetProject => !!dataset);
        const imageCount = members.reduce((total, dataset) => total + dataset.imageCount, 0);
        const progress = imageCount ? Math.round(members.reduce((total, dataset) => total + dataset.imageCount * dataset.annotatedPercent, 0) / imageCount) : 0;
        return <article className={`data-projects-group-card ${expanded ? "is-expanded" : ""} ${dragSource?.kind === "group" && dragSource.groupId === group.id ? "is-dragging" : ""} ${dropTarget === `group:${group.id}` ? "is-drop-target" : ""}`} key={group.id} data-group-drop-kind="group" data-group-drop-id={group.id}
          onPointerDown={(event) => pointerDown(event, { kind: "group", groupId: group.id })}
          onPointerUp={(event) => { event.stopPropagation(); pointerUp(event, { kind: "group", id: group.id }); }}>
          <div className="data-projects-group-header"><GripVertical aria-hidden="true" size={15} />
            {editingGroupId === group.id ? <input autoFocus aria-label={`重命名分组 ${group.name}`} maxLength={80} value={editingName} onChange={(event) => setEditingName(event.target.value)} onBlur={() => void finishRename(group)} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); void finishRename(group); } else if (event.key === "Escape") { editingIdRef.current = null; setEditingGroupId(null); } }} /> : <button className="data-projects-group-title" type="button" aria-expanded={expanded} onClick={() => toggleGroup(group.id)}>{group.name}<ChevronDown size={14} /></button>}
            <div className="data-projects-group-stats"><span>{members.length} 个数据集</span><span>{imageCount.toLocaleString("zh-CN")} 张图像</span><strong>{progress}%</strong></div>
            {canEdit ? <div className="data-projects-group-menu"><button type="button" aria-label={`分组菜单 ${group.name}`} aria-expanded={menuGroupId === group.id} disabled={saving} onClick={() => setMenuGroupId((current) => current === group.id ? null : group.id)}><MoreHorizontal size={15} /></button>{menuGroupId === group.id ? <div className="data-projects-group-menu-content" role="menu"><button type="button" disabled={saving} onClick={() => { setMenuGroupId(null); beginRename(group); }}><Pencil size={13} />重命名</button><button type="button" disabled={saving} onClick={() => { setMenuGroupId(null); void dissolveGroup(group.id); }}>解散分组</button><label><span>合并到</span><select aria-label={`移动分组 ${group.name}`} disabled={saving} value="" onChange={(event) => { if (event.target.value) { setMenuGroupId(null); void mergeGroups(group.id, event.target.value); } }}><option value="">选择目标分组</option>{normalizedGroups.filter((candidate) => candidate.id !== group.id).map((candidate) => <option value={candidate.id} key={candidate.id}>{candidate.name}</option>)}</select></label></div> : null}</div> : null}
          </div>
          <div className="progress-bar" aria-label={`${group.name} 已标注 ${progress}%`}><span style={{ width: `${progress}%` }} /></div>
          {expanded ? <div className="data-projects-group-members">{members.map((dataset) => renderDataset(dataset, group.id))}</div> : null}
        </article>;
      })}{!normalizedGroups.length ? <div className="data-projects-group-list-empty" title="拖入数据集创建分组">暂无分组</div> : null}</div>
    </div>
  </section>;
}

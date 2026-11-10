import { lazy, Suspense, type ComponentProps } from "react";

const DataProjects = lazy(() => import("../features/data-projects/DataProjectHome").then((module) => ({ default: module.DataProjectHome })));
const DatasetManagement = lazy(() => import("../features/projects/ProjectWorkspace").then((module) => ({ default: module.ProjectWorkspace })));
const Annotation = lazy(() => import("../features/annotation/AnnotationWorkspace").then((module) => ({ default: module.AnnotationWorkspace })));

function Loading() {
  return <div className="workspace-loading" role="status">正在加载…</div>;
}

export function DataProjectHome(props: ComponentProps<typeof DataProjects>) {
  return <Suspense fallback={<Loading />}><DataProjects {...props} /></Suspense>;
}

export function ProjectWorkspace(props: ComponentProps<typeof DatasetManagement>) {
  return <Suspense fallback={<Loading />}><DatasetManagement {...props} /></Suspense>;
}

export function AnnotationWorkspace(props: ComponentProps<typeof Annotation>) {
  return <Suspense fallback={<Loading />}><Annotation {...props} /></Suspense>;
}

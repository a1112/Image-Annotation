// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from "vitest";
import { annotationReturnRoute, clearAnnotationReturnRoutes, parseWorkspaceRoute, rememberAnnotationReturn, workspaceRouteHash } from "./workspaceRoutes";

beforeEach(() => window.sessionStorage.clear());

describe("three-part workspace routes", () => {
  it("keeps data project identity separate from the annotation dataset identity", () => {
    const route = { kind: "dataset", dataProjectId: "inspection", datasetId: "inspection::night", tab: "图片" } as const;
    expect(parseWorkspaceRoute(workspaceRouteHash(route, true))).toEqual(route);
    expect(parseWorkspaceRoute(workspaceRouteHash(route, false))).toEqual(route);
  });
  it("accepts previous dataset, annotation and malformed links", () => {
    expect(parseWorkspaceRoute("#/web/datasets")).toEqual({ kind: "home" });
    expect(parseWorkspaceRoute("#/datasets/night/类别")).toEqual({ kind: "dataset", datasetId: "night", tab: "类别" });
    expect(parseWorkspaceRoute("#/annotate/inspection::night/image-2")).toEqual({ kind: "annotate", datasetId: "inspection::night", imageId: "image-2" });
    expect(parseWorkspaceRoute("#/projects/%E0%A4%A")).toEqual({ kind: "home" });
  });
  it("returns to the management page and clears account-specific navigation", () => {
    rememberAnnotationReturn("night", "#/projects/inspection/datasets/night/图片");
    expect(annotationReturnRoute("night")).toBe("#/projects/inspection/datasets/night/图片");
    clearAnnotationReturnRoutes();
    expect(annotationReturnRoute("night")).toBe("#/projects");
    expect(annotationReturnRoute("inspection::night", true)).toBe("#/web/projects/inspection/data");
  });
});

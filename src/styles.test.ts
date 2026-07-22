import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const styles = readFileSync(resolve("src/styles.css"), "utf8");

function cssBlock(selector: string) {
  const match = styles.match(new RegExp(`${selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*\\{([^}]*)\\}`));
  return match?.[1] ?? "";
}

describe("project detail responsive layout", () => {
  it("数据集卡片使用带色背景与页面区分", () => {
    const card = cssBlock(".dataset-card");
    const metrics = cssBlock(".metric-grid");

    expect(card).toContain("background: linear-gradient");
    expect(card).not.toContain("background: #ffffff");
    expect(metrics).toContain("background: rgba(255, 255, 255");
  });

  it("顶部品牌区域仅保留紧凑图标位", () => {
    const brand = cssBlock(".brand");

    expect(brand).toContain("width: 34px");
    expect(brand).toContain("min-width: 34px");
  });

  it("顶部栏移除搜索列后操作区靠右", () => {
    const topbar = cssBlock(".topbar");
    const context = cssBlock(".topbar-context");
    const actions = cssBlock(".topbar-actions");

    expect(topbar).toContain("grid-template-columns: auto minmax(180px, 1fr) auto");
    expect(context).toContain("min-width: 0");
    expect(actions).toContain("justify-self: end");
    expect(styles).not.toContain(".search");
  });

  it("主内容区域贴近窗口边缘", () => {
    const datasetMain = cssBlock(".dataset-main");
    const projectMain = cssBlock(".project-main");
    const stickyHeader = cssBlock(".project-sticky-header");

    expect(datasetMain).toContain("padding: 0");
    expect(projectMain).toContain("padding: 0");
    expect(stickyHeader).not.toContain("margin: -");
  });

  it("项目详情页主内容占满可用宽度且页头操作可换行", () => {
    const page = cssBlock(".project-page");
    const tabs = cssBlock(".project-tabs");
    const surface = cssBlock(".project-surface");

    expect(page).toContain("grid-template-columns: minmax(0, 1fr)");
    expect(page).not.toContain("336px");
    expect(page).toContain("width: 100%");
    expect(tabs).toContain("display: flex");
    expect(surface).toContain("min-width: 0");
  });

  it("项目详情页表头在图片列表滚动时冻结", () => {
    const stickyHeader = cssBlock(".project-sticky-header");
    const main = cssBlock(".project-main");

    expect(main).toContain("overflow: auto");
    expect(stickyHeader).toContain("position: sticky");
    expect(stickyHeader).toContain("top: 0");
    expect(stickyHeader).toContain("z-index:");
    expect(stickyHeader).toContain("background:");
  });

  it("单数据集概览使用稳定的运营台网格并在窄屏切换单栏", () => {
    const overviewGrid = cssBlock(".project-overview-grid");
    const sampleGrid = cssBlock(".overview-sample-grid");

    expect(overviewGrid).toContain("grid-template-columns: minmax(0, 2fr) minmax(280px, 1fr)");
    expect(sampleGrid).toContain("grid-template-columns: repeat(6, minmax(0, 1fr))");
    expect(sampleGrid).toContain("min-height:");
    expect(styles).toMatch(/@media \(max-width: 960px\)[\s\S]*\.project-overview-grid[\s\S]*grid-template-columns: minmax\(0, 1fr\)/);
  });
});

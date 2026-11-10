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

  it("移动表单控件保持 16px 以避免 WKWebView 聚焦放大", () => {
    expect(styles).toMatch(/\.mobile-shell input,\s*\.mobile-shell select,\s*\.mobile-shell textarea\s*\{[\s\S]*min-width: 0;[\s\S]*font-size: 16px;/);
    const loginInput = cssBlock(".mobile-login-form input");
    expect(loginInput).toContain("font-family: inherit");
    expect(loginInput).toContain("font-size: 16px");
    expect(loginInput).not.toContain("font: inherit");
  });

  it("移动标注布局按 runtime 壳固定画布并使用属性抽屉", () => {
    expect(styles).toMatch(/\.mobile-shell\.is-annotation-route\s*\{[\s\S]*height: 100dvh;[\s\S]*overflow: hidden;/);
    expect(styles).toMatch(/\.mobile-shell-content--annotation > \.mobile-annotation-page\s*\{[\s\S]*grid-template-columns: 44px minmax\(0, 1fr\);[\s\S]*height: 100%;[\s\S]*overflow: hidden;/);
    expect(styles).toMatch(/\.mobile-shell-content--annotation \.mobile-inspector\s*\{[\s\S]*position: absolute;[\s\S]*display: none;[\s\S]*overflow: auto;/);
    expect(styles).toMatch(/\.mobile-shell-content--annotation \.mobile-inspector\s*\{[\s\S]*grid-column: auto;[\s\S]*grid-row: auto;/);
    expect(styles).toMatch(/\.mobile-shell-content--annotation \.mobile-inspector\.is-mobile-open\s*\{[\s\S]*display: block;/);
    expect(styles).toMatch(/\.mobile-shell-content--annotation \.mobile-inspector-header\s*\{[\s\S]*position: sticky;/);
    expect(styles).toMatch(/\.mobile-shell-content--annotation \.mobile-inspector-close\s*\{[\s\S]*white-space: nowrap;/);
    expect(styles).toMatch(/@media \(min-width: 900px\)[\s\S]*\.mobile-shell-content--annotation > \.mobile-annotation-page\s*\{[\s\S]*grid-template-columns: 44px minmax\(0, 1fr\) 320px;/);
    expect(styles).toMatch(/@media \(min-width: 900px\)[\s\S]*\.mobile-shell-content--annotation \.mobile-inspector-header\s*\{[\s\S]*display: none;/);
  });

  it("移动标注主操作不被 flex 压窄或中文换行", () => {
    expect(styles).toMatch(/\.mobile-shell-content--annotation \.annotation-actions > button\s*\{[\s\S]*flex: 0 0 auto;[\s\S]*white-space: nowrap;/);
    expect(styles).toMatch(/\.mobile-shell-content--annotation \.annotation-actions > \.mobile-inspector-toggle,[\s\S]*\.mobile-primary-action\s*\{[\s\S]*order: -1;/);
    expect(styles).toMatch(/\.mobile-shell-content--annotation \.annotation-actions > span\s*\{[\s\S]*order: 2;[\s\S]*white-space: nowrap;/);
  });

  it("托管导入行给格式选择保留宽度并让文件控件可收缩", () => {
    const importRow = cssBlock(".hosting-import-row");
    const importSelect = cssBlock(".hosting-import-row select");
    const importFile = cssBlock('.hosting-import-row input[type="file"]');

    expect(importRow).toContain("grid-template-columns: minmax(120px, .35fr) minmax(0, 1fr)");
    expect(importSelect).toContain("min-width: 120px");
    expect(importFile).toContain("min-width: 0");
    expect(importFile).toContain("max-width: 100%");
    expect(styles).toMatch(/@media \(max-width: 760px\)[\s\S]*\.hosting-import-row\s*\{[\s\S]*grid-template-columns: minmax\(0, 1fr\)/);
  });

  it("托管导出结果在移动端限制宽度并允许沙盒信息换行", () => {
    const result = styles.match(/(?:^|\n)\.hosting-download-result\s*\{([^}]*)\}/)?.[1] ?? "";
    const meta = cssBlock(".hosting-download-result__meta");

    expect(result).toContain("width: 100%");
    expect(result).toContain("max-width: 100%");
    expect(result).toContain("min-width: 0");
    expect(result).toContain("white-space: normal");
    expect(meta).toContain("max-width: 100%");
    expect(meta).toContain("min-width: 0");
    expect(styles).toMatch(/@media \(max-width: 680px\)[\s\S]*\.hosting-download-result\s*\{[\s\S]*grid-template-columns: minmax\(0, 1fr\)/);
    expect(styles).toMatch(/\.hosting-version-detail\s*\{[\s\S]*max-width: 100%;[\s\S]*min-width: 0;/);
    expect(styles).toMatch(/\.hosting-version-detail pre\s*\{[\s\S]*max-width: 100%;[\s\S]*overflow: auto;/);
    expect(styles).toMatch(/\.mobile-hosting-modal\s*\{[\s\S]*box-sizing: border-box;[\s\S]*max-width: 100%;[\s\S]*min-width: 0;/);
  });
});

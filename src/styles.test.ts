import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const styles = readFileSync(resolve("src/styles.css"), "utf8");

function cssBlock(selector: string) {
  const match = styles.match(new RegExp(`${selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*\\{([^}]*)\\}`));
  return match?.[1] ?? "";
}

describe("project detail responsive layout", () => {
  it("项目详情页主内容占满可用宽度且页头操作可换行", () => {
    const page = cssBlock(".project-page");
    const header = cssBlock(".project-header");
    const actions = cssBlock(".project-actions");
    const surface = cssBlock(".project-surface");

    expect(page).toContain("grid-template-columns: minmax(0, 1fr)");
    expect(page).not.toContain("336px");
    expect(page).toContain("width: 100%");
    expect(header).toContain("flex-wrap: wrap");
    expect(actions).toContain("flex-wrap: wrap");
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
});

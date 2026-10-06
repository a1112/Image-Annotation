import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["src/**/*.test.{ts,tsx}"],
    environment: "jsdom",
    setupFiles: "./src/test/setup.ts",
    // src-tauri 下的 *.test.cjs 使用 node:test,由 `npm run test:chrome` 单独执行。
    exclude: ["**/node_modules/**", "src-tauri/**", "./src-tauri/**"],
  },
});

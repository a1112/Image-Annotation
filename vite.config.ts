import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// @ts-expect-error process is a Node.js global in Vite config.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  // @ts-expect-error Vitest reads this Vite-compatible extension at test time.
  test: {
    include: ["src/**/*.test.{ts,tsx}"],
    exclude: ["**/._*"],
  },
  server: {
    port: 1440,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1441,
        }
      : undefined,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
});

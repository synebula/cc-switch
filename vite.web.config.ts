import path from "node:path";
import fs from "node:fs";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { codeInspectorPlugin } from "code-inspector-plugin";

// Web-only build:
// - Does not require Tauri runtime
// - Keeps upstream `src/` untouched by aliasing Tauri imports to local shims
const pkg = JSON.parse(
  fs.readFileSync(path.resolve(__dirname, "package.json"), "utf-8"),
) as { version?: string };
const appVersion = pkg.version ?? "0.0.0-web";

export default defineConfig(({ command }) => ({
  root: "web",
  plugins: [
    command === "serve" &&
      codeInspectorPlugin({
        bundler: "vite",
      }),
    react(),
  ].filter(Boolean),
  base: "./",
  define: {
    __CC_SWITCH_VERSION__: JSON.stringify(appVersion),
  },
  build: {
    outDir: "../dist-web",
    emptyOutDir: true,
  },
  server: {
    port: 3001,
    strictPort: true,
  },
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
      // Tauri shims (web mode)
      "@tauri-apps/api/core": path.resolve(
        __dirname,
        "./web/tauri-shim/api/core.ts",
      ),
      "@tauri-apps/api/event": path.resolve(
        __dirname,
        "./web/tauri-shim/api/event.ts",
      ),
      "@tauri-apps/api/path": path.resolve(
        __dirname,
        "./web/tauri-shim/api/path.ts",
      ),
      "@tauri-apps/api/app": path.resolve(
        __dirname,
        "./web/tauri-shim/api/app.ts",
      ),
      "@tauri-apps/plugin-updater": path.resolve(
        __dirname,
        "./web/tauri-shim/plugin-updater.ts",
      ),
      "@tauri-apps/plugin-process": path.resolve(
        __dirname,
        "./web/tauri-shim/plugin-process.ts",
      ),
    },
  },
  clearScreen: false,
  envPrefix: ["VITE_", "TAURI_"],
}));

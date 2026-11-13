import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: { port: 5273, strictPort: true, watch: { ignored: ["**/src-tauri/**"] } },
  build: {
    target: "es2021",
    rollupOptions: { input: { main: "index.html", preview: "preview.html" } },
  },
});

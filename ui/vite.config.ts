import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// 产物直接给 Tauri 的 frontendDist 用（ADR-0013：不依赖 dev server 也能起窗口）。
// target 对齐 WebView2 的常青版本，不为老浏览器降级。
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { outDir: 'dist', emptyOutDir: true, target: 'chrome110', sourcemap: false },
});

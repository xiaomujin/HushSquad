import { MockBackend, type Backend } from './ipc';
import { TauriBackend } from './tauri-backend';

/**
 * 后端实例。
 *
 * - 在 Tauri 壳里（`window.__TAURI_INTERNALS__` 存在）→ 真后端，跑真客户端；
 * - 在浏览器里（`pnpm dev`）→ 假后端，让界面能在不开 Tauri 的情况下看与点。
 *
 * 组件一行都不用知道这件事——这是 ADR-0013「前端只是渲染器」的具体做法。
 */
const inTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

export const backend: Backend = inTauri ? new TauriBackend() : new MockBackend();
export { inTauri };

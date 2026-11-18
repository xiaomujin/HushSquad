import { MockBackend, type Backend } from './ipc';

/**
 * 后端实例。
 *
 * 现在固定是 Mock（浏览器里 `pnpm dev` 就能看界面）；Tauri 壳接进来之后，
 * 这里换成 `new TauriBackend()`，组件一行都不用改——这是 ADR-0013 里
 * 「前端只是渲染器」的具体做法。
 */
export const backend: Backend = new MockBackend();

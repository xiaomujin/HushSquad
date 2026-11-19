// 给 Tauri 窗口截图 / 点按钮：走 WebView2 的 CDP，不需要任何额外依赖（Node 22+ 自带 WebSocket）。
//
// 为什么需要它：`tauri build` 出来的窗口没法用 Chrome 的 --screenshot，
// 而「界面在真窗口里长什么样」必须能被取证（docs/ui/shots/app-tauri-*.png 就是这么来的）。
//
// 用法：
//   1. 带调试端口启动外壳：
//        $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222"
//        & apps/app/target/debug/hushsquad-app.exe
//   2. 拿页面地址：
//        (Invoke-WebRequest http://127.0.0.1:9222/json/list -UseBasicParsing).Content
//   3. 截图（可以顺带在页面上执行几步 JS，格式 "等多久::表达式"）：
//        node scripts/tauri-shot.mjs <ws-url> out.png "0::document.querySelector('.btn-primary').click()" "4000::1"
//
// 每一步会打印表达式的返回值，方便确认「点到了」。

import fs from 'node:fs';

const [wsUrl, outFile, ...steps] = process.argv.slice(2);
if (!wsUrl || !outFile) {
  console.error('用法：node scripts/tauri-shot.mjs <ws-url> <out.png> ["毫秒::表达式" ...]');
  process.exit(2);
}

const ws = new WebSocket(wsUrl);
const pending = new Map();
let nextId = 0;

ws.addEventListener('message', (event) => {
  const message = JSON.parse(event.data);
  if (message.id && pending.has(message.id)) {
    pending.get(message.id)(message);
    pending.delete(message.id);
  }
});

const send = (method, params = {}) =>
  new Promise((resolve) => {
    const id = ++nextId;
    pending.set(id, resolve);
    ws.send(JSON.stringify({ id, method, params }));
  });

const evaluate = async (expression) => {
  const reply = await send('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  return reply.result?.result?.value ?? reply.result?.exceptionDetails?.text;
};

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

await new Promise((resolve) => ws.addEventListener('open', resolve));

for (const step of steps) {
  const [wait, expression] = step.split('::');
  if (expression) {
    console.log('>', expression);
    console.log(' ', await evaluate(expression));
  }
  await sleep(Number(wait) || 0);
}

const shot = await send('Page.captureScreenshot', { format: 'png' });
fs.writeFileSync(outFile, Buffer.from(shot.result.data, 'base64'));
console.log('screenshot ->', outFile);
ws.close();

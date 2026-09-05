const fs = require('node:fs');

const READY_RETRIES = 40;
const READY_INTERVAL_MS = 250;
const COMMAND_TIMEOUT_MS = 10_000;

function parseArgs(argv) {
  const result = {};
  for (let index = 0; index < argv.length; index += 2) {
    const key = argv[index];
    const value = argv[index + 1];
    if (!key?.startsWith('--') || value === undefined) {
      throw new Error('用法: open-workbench.cjs --url <file-url> --payload <payload.json> [--screenshot <output.png>]');
    }
    result[key.slice(2)] = value;
  }
  return result;
}

function readPayload(payloadPath) {
  const payload = JSON.parse(fs.readFileSync(payloadPath, 'utf8'));
  if (!payload || !Array.isArray(payload.customers)) {
    throw new Error('payload.customers 必须是数组');
  }
  const hasPlaceholder = payload.customers.some((customer) => String(customer?.id || '').startsWith('placeholder-'));
  if (hasPlaceholder) throw new Error('payload 中不能包含占位客户');
  return payload;
}

function toExpressionValue(value) {
  return JSON.stringify(value)
    .replaceAll('<', '\\u003c')
    .replaceAll('\u2028', '\\u2028')
    .replaceAll('\u2029', '\\u2029');
}

function sleep(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function discoverTarget(port) {
  const response = await fetch(`http://127.0.0.1:${port}/json/list`);
  if (!response.ok) throw new Error(`CDP 目标发现失败: HTTP ${response.status}`);
  const targets = await response.json();
  const target = Array.isArray(targets) ? targets.find((item) => item?.type === 'page') : null;
  if (!target?.webSocketDebuggerUrl) throw new Error('CDP 未返回可控制的页面目标');
  return target.webSocketDebuggerUrl;
}

async function connect(webSocketUrl) {
  const socket = new WebSocket(webSocketUrl);
  const pending = new Map();
  let nextId = 0;

  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, { once: true });
    socket.addEventListener('error', () => reject(new Error('CDP WebSocket 连接失败')), { once: true });
  });

  socket.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    const request = pending.get(message.id);
    if (!request) return;
    pending.delete(message.id);
    clearTimeout(request.timeout);
    if (message.error) request.reject(new Error(message.error.message || 'CDP 命令失败'));
    else request.resolve(message.result);
  });

  function send(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++nextId;
      const timeout = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`CDP 命令超时: ${method}`));
      }, COMMAND_TIMEOUT_MS);
      pending.set(id, { resolve, reject, timeout });
      socket.send(JSON.stringify({ id, method, params }));
    });
  }

  return { socket, send };
}

async function waitForWorkbench(send) {
  for (let attempt = 0; attempt < READY_RETRIES; attempt += 1) {
    const result = await send('Runtime.evaluate', {
      expression: 'typeof window.setApprovalWorkbenchData',
      returnByValue: true,
    });
    if (result?.result?.value === 'function') return;
    await sleep(READY_INTERVAL_MS);
  }
  throw new Error('工作台数据入口未就绪');
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (!args.url || !args.payload) throw new Error('必须提供 --url 和 --payload');
  if (new URL(args.url).protocol !== 'file:') throw new Error('--url 必须是 file:// 工作台地址');

  const port = Number(process.env.AIONUI_CDP_ACTIVE_PORT);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error('AIONUI_CDP_ACTIVE_PORT 未设置或无效');
  }

  const payload = readPayload(args.payload);
  const webSocketUrl = await discoverTarget(port);
  const { socket, send } = await connect(webSocketUrl);

  try {
    await send('Page.enable');
    await send('Page.navigate', { url: args.url });
    await waitForWorkbench(send);

    const injection = await send('Runtime.evaluate', {
      expression: `window.setApprovalWorkbenchData(${toExpressionValue(payload)})`,
      returnByValue: true,
    });
    const injectionResult = injection?.result?.value;
    if (injectionResult?.customerCount !== payload.customers.length) {
      throw new Error('工作台注入数量与 payload 不一致');
    }

    const verification = await send('Runtime.evaluate', {
      expression: `JSON.stringify({
        visibleNames: approvalCustomers().map(customer => customer.name),
        hasPlaceholder: document.getElementById('queueBody').innerText.includes('占位')
          || approvalCustomers().some(customer => String(customer.id || '').startsWith('placeholder-'))
      })`,
      returnByValue: true,
    });
    const visibleState = JSON.parse(verification?.result?.value || '{}');
    if (visibleState.hasPlaceholder) throw new Error('工作台仍包含占位数据');

    if (args.screenshot) {
      const screenshot = await send('Page.captureScreenshot', { format: 'png' });
      fs.writeFileSync(args.screenshot, Buffer.from(screenshot.data, 'base64'));
    }

    process.stdout.write(
      `${JSON.stringify({
        customerCount: injectionResult.customerCount,
        currentLevel: injectionResult.currentLevel,
        currentTab: injectionResult.currentTab,
        visibleNames: visibleState.visibleNames,
        hasPlaceholder: false,
      })}\n`
    );
  } finally {
    socket.close();
  }
}

main().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});

const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

const { WebSocketServer } = require(require.resolve('ws', { paths: [process.cwd()] }));

const skillRoot = path.resolve(__dirname, '..');
const helperPath = path.join(skillRoot, 'scripts', 'open-workbench.cjs');
const testDir = fs.mkdtempSync(path.join(os.tmpdir(), 'sales-forecast-open-workbench-'));
const payloadPath = path.join(testDir, 'payload.json');
const secret = 'test-secret-must-stay-in-memory';
const receivedMethods = [];

fs.writeFileSync(
  payloadPath,
  JSON.stringify({
    meta: { currentLevel: '客户确认AI预测', currentTab: 'customer' },
    permissions: ['客户确认AI预测'],
    customers: [{ id: 'C001', code: 'C001', name: '回归客户甲', stage: '客户确认AI预测', skus: [] }],
  })
);

function runChild(port) {
  return new Promise((resolve, reject) => {
    const child = spawn(
      process.execPath,
      [helperPath, '--url', 'file:///tmp/workbench.html', '--payload', payloadPath],
      {
        env: { ...process.env, AIONUI_CDP_ACTIVE_PORT: String(port) },
        stdio: ['ignore', 'pipe', 'pipe'],
      }
    );
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => (stdout += chunk));
    child.stderr.on('data', (chunk) => (stderr += chunk));
    child.once('error', reject);
    child.once('exit', (code) => resolve({ code, stdout, stderr }));
  });
}

async function main() {
  const server = http.createServer((request, response) => {
    if (request.url !== '/json/list') {
      response.writeHead(404).end();
      return;
    }
    const { port } = server.address();
    response.setHeader('content-type', 'application/json');
    response.end(
      JSON.stringify([
        {
          type: 'page',
          webSocketDebuggerUrl: `ws://127.0.0.1:${port}/aionui-cdp?token=${secret}`,
        },
      ])
    );
  });
  const webSocketServer = new WebSocketServer({ noServer: true });

  server.on('upgrade', (request, socket, head) => {
    webSocketServer.handleUpgrade(request, socket, head, (webSocket) => {
      webSocketServer.emit('connection', webSocket, request);
    });
  });

  webSocketServer.on('connection', (webSocket) => {
    webSocket.on('message', (data) => {
      const message = JSON.parse(data.toString());
      receivedMethods.push(message.method);
      let result = {};
      if (message.method === 'Runtime.evaluate') {
        if (message.params.expression === 'typeof window.setApprovalWorkbenchData') {
          result = { result: { type: 'string', value: 'function' } };
        } else if (message.params.expression.startsWith('window.setApprovalWorkbenchData(')) {
          result = {
            result: {
              type: 'object',
              value: { customerCount: 1, currentLevel: '客户确认AI预测', currentTab: 'customer' },
            },
          };
        } else {
          result = {
            result: {
              type: 'string',
              value: JSON.stringify({ visibleNames: ['回归客户甲'], hasPlaceholder: false }),
            },
          };
        }
      }
      webSocket.send(JSON.stringify({ id: message.id, result }));
    });
  });

  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const result = await runChild(server.address().port);

  assert.equal(result.code, 0, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout), {
    customerCount: 1,
    currentLevel: '客户确认AI预测',
    currentTab: 'customer',
    visibleNames: ['回归客户甲'],
    hasPlaceholder: false,
  });
  assert.equal(`${result.stdout}${result.stderr}`.includes(secret), false);
  assert.deepEqual(receivedMethods, [
    'Page.enable',
    'Page.navigate',
    'Runtime.evaluate',
    'Runtime.evaluate',
    'Runtime.evaluate',
  ]);

  await new Promise((resolve) => webSocketServer.close(resolve));
  await new Promise((resolve) => server.close(resolve));
  fs.rmSync(testDir, { recursive: true, force: true });
  console.log('PASS: helper discovers CDP in memory and verifies the visible workbench');
}

main().catch((error) => {
  console.error(`FAIL: ${error.message}`);
  process.exitCode = 1;
});

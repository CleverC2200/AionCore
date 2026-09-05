const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { app, BrowserWindow } = require('electron');

const skillRoot = path.resolve(__dirname, '..');
const templatePath = path.join(skillRoot, 'assets', 'plan-submit-template.html');
const testUserData = fs.mkdtempSync(path.join(os.tmpdir(), 'sales-forecast-workbench-e2e-'));

app.setPath('userData', testUserData);

const payload = {
  meta: {
    period: '2026 年 8 月',
    currentLevel: '客户确认AI预测',
    currentTab: 'customer',
    aiTag: '回归验证',
    averageTurnoverDays: 18,
    inventoryFocus: '回归客户甲库存正常',
  },
  permissions: ['客户确认AI预测'],
  customers: [
    {
      id: 'regression-customer-001',
      code: 'C001',
      name: '回归客户甲',
      area: '测试大区',
      province: '测试省区',
      region: '测试区域',
      base: '测试基地',
      stage: '客户确认AI预测',
      rejected: '',
      target: 1000,
      thisShip: 80,
      nextShip: 90,
      health: '健康',
      healthClass: 'healthy',
      ai: '保持当前计划',
      skus: [
        {
          sku: 'SKU001',
          name: '回归商品甲',
          base: 100,
          price: 10,
          qty: 100,
          amtBase: 1000,
          amt: 1000,
        },
      ],
    },
  ],
};

async function readVisibleState(window) {
  return window.webContents.executeJavaScript(`({
    names: approvalCustomers().map(customer => customer.name),
    queueText: document.getElementById('queueBody').innerText,
    navigationType: performance.getEntriesByType('navigation')[0]?.type || 'unknown'
  })`);
}

async function run() {
  const window = new BrowserWindow({ show: false });
  await window.loadFile(templatePath);

  const injectionResult = await window.webContents.executeJavaScript(
    `window.setApprovalWorkbenchData(${JSON.stringify(payload)})`
  );
  assert.equal(injectionResult.customerCount, 1);

  const beforeReload = await readVisibleState(window);
  assert.deepEqual(beforeReload.names, ['回归客户甲']);
  assert.equal(beforeReload.queueText.includes('占位'), false);

  const reloaded = new Promise((resolve) => window.webContents.once('did-finish-load', resolve));
  window.webContents.reload();
  await reloaded;

  const afterReload = await readVisibleState(window);
  assert.equal(afterReload.navigationType, 'reload');
  assert.deepEqual(afterReload.names, ['回归客户甲']);
  assert.equal(afterReload.queueText.includes('占位'), false);

  const recreatedWindow = new BrowserWindow({ show: false });
  await recreatedWindow.loadFile(templatePath);

  const afterRecreate = await readVisibleState(recreatedWindow);
  assert.deepEqual(afterRecreate.names, ['回归客户甲']);
  assert.equal(afterRecreate.queueText.includes('占位'), false);

  recreatedWindow.destroy();
  window.destroy();
  console.log('PASS: injected workbench data survives reload and browser recreation');
}

app
  .whenReady()
  .then(run)
  .then(() => app.quit())
  .catch((error) => {
    console.error(`FAIL: ${error.message}`);
    app.exit(1);
  });

app.on('will-quit', () => {
  fs.rmSync(testUserData, { recursive: true, force: true });
});

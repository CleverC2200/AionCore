# 区域经理审批工作台使用说明

## 1. 文件说明

- 页面文件：`区域经理审批工作台_原型.html`
- 本说明：`区域经理审批工作台_使用说明.md`
- 页面为单文件原型，无需安装依赖。双击 HTML 或拖入浏览器即可打开。
- 页面首次单独打开且没有 24 小时内有效 payload 时显示占位数据，仅用于验证模板交互。技能执行时应先完成 MCP 查询，页面加载后立即调用数据入口整体替换，不等待用户手动操作。

## 2. 页面操作

### 2.1 月份与审批阶段

- 页面左上角显示当前计划月份，月份由 MCP 数据中的 `meta.period` 填充。
- 点击审批阶段可切换当前审核层级：
  - 客户确认AI预测
  - 区域审批
  - 省区审批
  - 大区审批
  - 品类计划审核
- 阶段颜色和完成率根据客户当前所在阶段自动计算。
- 不在 `permissions` 中的阶段不可点击。

### 2.2 AI 建议区

AI 建议区自动展示：

- 本层级待审数量；
- 整体金额达成率；
- 平均库存周转天数；
- 建议关注库存；
- 当前审核层级的操作说明。

其中周转天数、关注库存和标签由 MCP 填充，其余统计值由页面根据客户及 SKU 数据实时计算。

### 2.3 审批核对队列

1. 根据当前审批阶段选择可用的组织维度，例如按省区、区域或客户查看。
2. 可使用大区、省区、区域、客户编号/名称和审批状态筛选数据。
3. 点击“查询”应用筛选条件，点击“重置”恢复默认条件。
4. “待审批”表示客户正停留在当前阶段；“已审批”表示客户已经进入后续阶段。
5. 被标记为 `rejected` 的客户默认不进入队列。

### 2.4 调整明细

- 点击可编辑的组织名称或“调整明细 / 查看影响”打开明细窗口。
- 只有分组内所有客户都停留在当前审批阶段时才允许修改；已进入后续阶段的数据只读。
- 客户维度支持修改 SKU 数量或金额：
  - 修改数量时，金额按单价自动更新；
  - 修改金额时，数量按单价自动反算。
- 聚合维度的修改会按各客户原计划量占比向下分摊到客户和 SKU。
- 点击 AI 建议旁的“采纳”，可将建议量写入当前 SKU 或聚合分组。
- 所有调整仅保存在当前页面内存中，刷新页面后会恢复为最近一次真实注入的数据。

### 2.5 提报进度

- 点击“提报进度”查看当前阶段下级组织的提交情况。
- 点击组织节点可逐级展开或收起。
- 完成率由已到达当前阶段的客户数除以客户总数得到。

## 3. MCP 数据注入

技能执行时优先使用 `scripts/open-workbench.cjs`。先把标准 payload 保存到当前会话目录，再让脚本动态发现 GEA 内置浏览器、打开模板、注入、读回验证并截图。CDP 端口、WebSocket 地址和 token 只在脚本进程内存在，不写入会话文件。

### 3.1 推荐调用方式

页面加载完成后执行：

```javascript
window.setApprovalWorkbenchData(payload);
```

函数完成数据标准化、筛选项重建和页面重绘，并返回：

```javascript
{
  customerCount: 1,
  currentLevel: '大区审批',
  currentTab: 'customer'
}
```

也可以派发事件：

```javascript
window.dispatchEvent(new CustomEvent('approval-workbench:data', {
  detail: payload
}));
```

### 3.2 完整数据示例

```javascript
const payload = {
  meta: {
    period: '2026 年 9 月',
    currentLevel: '大区审批',
    currentTab: 'customer',
    aiTag: '关注',
    averageTurnoverDays: 18,
    inventoryFocus: '品类 A / 品类 B'
  },
  permissions: [
    '客户确认AI预测',
    '区域审批',
    '省区审批',
    '大区审批',
    '品类计划审核'
  ],
  customers: [
    {
      id: 'customer-001',
      code: 'C001',
      name: '客户名称',
      area: '大区名称',
      province: '省区名称',
      region: '区域名称',
      base: '基地名称',
      stage: '大区审批',
      rejected: '',
      target: 100000,
      thisShip: 4200,
      nextShip: 5100,
      health: '关注',
      healthClass: 'warning',
      ai: '建议重点复核库存与计划差异',
      skus: [
        {
          sku: 'SKU001',
          name: 'SKU 名称',
          base: 5000,
          price: 10,
          qty: 5200,
          amtBase: 50000,
          amt: 52000
        }
      ]
    }
  ]
};

window.setApprovalWorkbenchData(payload);
```

## 4. 字段说明

### 4.1 `meta`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `period` | string | 页面左上角显示的计划月份 |
| `currentLevel` | string | 首次展示的审批阶段，必须是允许的阶段值 |
| `currentTab` | string | 首次展示维度：`area`、`province`、`region`、`base`、`customer` |
| `aiTag` | string | AI 建议区标签 |
| `averageTurnoverDays` | number/null | 平均库存周转天数；空值显示 `--` |
| `inventoryFocus` | string | 建议关注的库存或品类 |

### 4.2 `permissions`

审批阶段权限数组，只接受以下值：

```text
客户确认AI预测
区域审批
省区审批
大区审批
品类计划审核
```

未传时默认允许全部阶段。`meta.currentLevel` 不在权限范围内时，页面会回退到可用阶段。

### 4.3 `customers[]`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `id` | string/number | 客户唯一标识，建议稳定且不重复 |
| `code` | string | 客户编号 |
| `name` | string | 客户名称 |
| `area` | string | 大区名称 |
| `province` | string | 省区名称 |
| `region` | string | 区域名称 |
| `base` | string | 基地名称；注意这里是组织字段 |
| `stage` | string | 客户当前审批阶段 |
| `rejected` | string | 驳回原因；非空时该客户默认不进入队列 |
| `target` | number | 预算目标金额 |
| `thisShip` | number | 本月同期拉货量 |
| `nextShip` | number | 次月同期拉货量 |
| `health` | string | 页面展示的健康度文字 |
| `healthClass` | string | `healthy`、`warning`、`danger` |
| `ai` | string | AI 审批意见 |
| `skus` | array | 客户 SKU 明细 |

### 4.4 `customers[].skus[]`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `sku` | string | SKU 编码 |
| `name` | string | SKU 名称 |
| `base` | number | SKU 原计划量；注意这里是数量字段 |
| `price` | number | SKU 单价 |
| `qty` | number | SKU 新计划量；未传时等于 `base` |
| `amtBase` | number | 原计划金额；未传时按 `base × price` 计算 |
| `amt` | number | 新计划金额；未传时按 `qty × price` 计算 |

## 5. 注入规则与注意事项

- 每次调用 `setApprovalWorkbenchData` 都会整体替换当前客户数据，而不是增量追加。
- 大区、省区和区域筛选选项会根据新数据重新生成。
- 数据注入后会清空当前筛选条件、关闭已打开的明细和进度窗口，并重新计算全部统计值。
- 文本字段会在写入动态 HTML 前进行转义，但 MCP 仍应只传递业务数据，不应传递 HTML 或脚本。
- 无效数字按 `0` 处理；缺失文本使用“待填充”占位值。
- 无效 `stage` 会回退到“客户确认AI预测”。
- `healthClass` 无效或缺失时按 `healthy` 处理。
- 当前 HTML 仅展示一个计划月份；再次注入可替换该月份。
- 最近一次非占位 payload 会按当前 HTML 路径隔离保存 24 小时；刷新或浏览器重建后会恢复该 payload，不会回到内置占位数据。

## 6. 当前原型边界

- “同意”“不同意”“导出”按钮仅用于界面展示，尚未连接后端或 MCP 写入动作。
- 表格复选框只维护当前页面的选中样式。
- 每页条数切换可用，但上一页、下一页和页码按钮尚未实现真实分页。
- 页面调整不会自动回写 MCP、数据库或审批系统；如需回写，应由智能体读取页面状态后调用独立的 MCP 写入接口。
- 刷新或浏览器重建后会恢复 24 小时内最近一次真实注入；数据过期或需要更新时，由智能体重新运行 `open-workbench.cjs` 注入本轮 payload。

## 7. 常见问题

### 页面仍显示占位数据

运行 `scripts/open-workbench.cjs` 重新打开并注入；如需在已打开页面原地修复，也可在页面完成加载后调用：

```javascript
window.setApprovalWorkbenchData(payload);
```

并检查 `payload.customers` 是否为数组。
同时确认 `customers` 中不存在 `placeholder-`，且页面文本不含“待 MCP 填充”。

### 看不到某个审批阶段的数据

依次检查：

1. 该阶段是否包含在 `permissions` 中；
2. 客户 `stage` 是否使用了规定的中文阶段值；
3. 当前筛选条件是否排除了该客户；
4. 客户 `rejected` 是否为非空值。

### 明细无法修改

只有客户 `stage` 与当前页面审批阶段完全一致时才允许修改。分组中只要存在一个已进入后续阶段的客户，整个分组即为只读。

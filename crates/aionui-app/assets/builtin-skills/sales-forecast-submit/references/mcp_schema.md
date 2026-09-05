# MCP 数据模型与查询约定

## 语义模型总览

MCP `query_business_data` 背后是 Cube 语义模型。当前 `inspect` 只返回部分 Cube（`agents_sales_forecast_detail` + `safety_stock_snapshot`），但实际可查询的 Cube 更多——需按完整成员名 `Cube名称.成员名称` 直接发 query 验证。与需求预测工作台相关的三个核心 Cube：

| Cube 语义模型 | 中文业务语义 | 事实粒度 | 核心事实 | 禁止混用/替代 |
|---|---|---|---|---|
| `agents_sales_forecast_detail` | 销售计划预测明细 | 计划月 × 经销商 × SKU | AI 预测金额/数量、品类预算、实际销额/数量、完整性计数、价格、置信度、概率、节日/版本解释 | 预测金额与完整实际评价的唯一主语义；不能用预订单或计划执行替代 |
| `dealer_plan_order` | 客户预订单、预算、发货与库存快照 | 客户 × SKU × 月度/库存快照 | 月度预算、发货金额、待发金额、**现库存**、在途、周转、货龄、目标库存、日销、建议订货 | 库存/订单语义；不能把其预算或发货直接说成 AI 预测或 Agent 确认计划 |
| `customer_plan_execution` | 客户计划执行情况 | 客户 × SKU × 统计月 | 产品品类、外部计划量/金额、发货量、历史提报覆盖 | 外部执行语义；plan_quantity/plan_amount 不能替代本工作台最终确认计划 |

（另有 `safety_stock_snapshot` 基地安全库存、`inventory_age` 库存货龄等 Cube，属基地侧语义，不参与客户需求预测主流程。）

### 主键、关联与时间格式

| 对象 | 主粒度/唯一键 | 关联键 | 时间口径 |
|---|---|---|---|
| 主预测明细 | `plan_month + dealer_code + sku_code` | `dealer_code = cust_code`；`sku_code = material_code` | `plan_month` 为 `YYYYMM`，如 `202608` |
| 预订单/库存 | 客户 × SKU × 月度/快照 | `cust_code + material_code` | 库存读取**最新有 `stock_qty` 的库存快照** |
| 计划执行 | 客户 × SKU × 统计月 | `cust_code + material_code` | `stat_month` 为 `YYYYMM` |

- 所有编码关联以去首尾空格后的字符串精确匹配；名称（客户名/SKU名/物料组名）仅供展示，不能作关联键。
- `0` 是有效业务值；只有 `null`/字段缺失才是数据缺失，不得把 `null` 静默补成 0。

---

## 1. `agents_sales_forecast_detail`（主预测事实）

金额单位均为**元**，不换算万元/亿元。完整写法 `agents_sales_forecast_detail.<成员名>`。

### 1.1 维度（dimensions）

| 用途 | 字段 | 类型 | 说明 |
|---|---|---|---|
| 明细主键 | `id` | number | 不参与业务汇总 |
| 计划月份 | `plan_month` | string(6) | YYYYMM；精确筛选用 `equals` |
| 预测月份日期 | `forecast_month` | time | 月份第一天；仅日期范围查询，不能替代 plan_month |
| 经销商代码 | `dealer_code` | string | 关联所有客户级数据 |
| 经销商名称 | `dealer_name` | string | 展示用，不能作关联键 |
| 销售大区 | `region_code` / `region_name` | string | 精确筛选用编码 |
| 销售省区 | `province_region_code` / `province_region_name` | string | 精确筛选用编码 |
| 销售组 | `sales_group_code` / `sales_group_name` | string | 精确筛选用编码 |
| SKU 编码 | `sku_code` | string | 关联库存、计划执行、确认计划 |
| SKU 名称 | `sku_name` | string | 展示用 |
| 物料组 | `material_group_name` | string | 物料组筛选/汇总；不能用于预算去重 |
| 品类 | `product_categ_name` | string | 品类展示；可为空由计划执行补齐 |
| 命中把握 | `hit_confidence` | string | 高/中/低；其他值映射 `unknown` 展示 `-` |
| 发货概率 | `delivery_probability` | number nullable | SKU 明细字段，不能当聚合指标（聚合用 average_delivery_probability） |
| 分类依据（预算去重键） | `category_basis` | string | **月度经营目标去重键**：`plan_month + dealer_code + category_basis` |
| 品类预算金额 | `category_budget_amount` | number | 重复在 SKU 行上的品类级预算，去重后才是品类/客户月度经营目标 |
| 品类预算份额 | `category_budget_share` | number | 同品类重复，禁止跨 SKU 求和 |
| 品类内 SKU 份额 | `category_sku_share` | number | 预测金额缺失时才可用 `category_budget_amount × category_sku_share` 兜底 |
| 品类内条件份额 | `category_conditional_share` | number | 仅品类条件解释，不可跨月/跨客户相加 |
| 预测份额 | `predicted_share` | number | 只作解释，不能跨月/跨客户求和 |
| 未税到岸价 | `landed_price_excl_tax` | number nullable | 数量换算、金额调整与数量核对 |
| 数量状态 | `quantity_status` | string nullable | 数量可用/异常状态说明 |
| 概率状态/版本 | `probability_status` / `probability_calibration_version` | string nullable | 概率状态与版本 |
| 候选来源/品类收缩 | `candidate_source` / `category_shrink_level` | string nullable | 候选来源与品类收缩说明 |
| 节日周期/阶段 | `holiday_cycle` / `holiday_stage` | string nullable | 是否执行以启用且审核通过为准 |
| 是否业务审核新增 | `business_review_added` | boolean | — |
| 是否使用回退 | `fallback_used` | boolean | 不等于数据错误 |
| 版本/时间 | `model_version` / `parameter_version` / `created_at` | string/time | 快照修订、版本追溯 |

> 不向通用 Agent 公开：`model_path`、`category_model_path`、`probability_model_path`、内部算法长文本。解释用受控的 `logic_summary`/`ai_reason`，不泄露内部路径。

### 1.2 度量（measures）

| 用途 | 字段 | 说明 |
|---|---|---|
| **预测销额（权威）** | `predicted_sales_amount` | 权威 AI 预测金额口径；回答预测金额/计划金额用此，勿用实际销额或品类预算替代 |
| 预测数量 | `predicted_quantity` | 按未税到岸价换算并整数平衡后的 SKU 预测数量；订货辅助，不得反推/修改预测金额 |
| 实际销额 | `actual_sales_amount` | 目标月份实际发货金额；仅完整历史月用于达成/准确率/偏差；可能为空 |
| 实际发货数量 | `actual_sales_quantity` | 仅完整历史月用于达成量 |
| 明细记录数 | `row_count` | 当前筛选范围总行数；实际完整性校验用 |
| 已回填实际记录数 | `actual_record_count` | `actual_sales_amount IS NOT NULL` 计数；`row_count>0 && actual_record_count=row_count` 才能评价完整实际 |
| 已实际发货 SKU 数 | `actual_shipped_sku_count` | 实际金额非空且>0 的 SKU 去重数 |
| 经销商数 | `dealer_count` | 聚合用 |
| SKU 数 | `sku_count` | 聚合用 |
| 平均发货概率 | `average_delivery_probability` | 聚合发货概率时放 measures；SKU 明细概率用维度 delivery_probability |
| 数量对应销额 | `quantity_sales_amount` | `预测数量 × 未税到岸价`，仅核对数量取整 |
| 数量金额差额 | `quantity_amount_diff` | `quantity_sales_amount - predicted_sales_amount`；不能反改预测金额 |
| 平均未税到岸价 | `average_landed_price_excl_tax` | — |
| 预测数量缺失记录数 | `empty_predicted_quantity_count` | 提示数量缺失 SKU 数 |

---

## 2. `dealer_plan_order`（客户预订单、库存与补货）

关联：`cust_code = dealer_code`、`material_code = sku_code`。完整写法 `dealer_plan_order.<成员名>`。

| 工作台字段 | Cube 成员 | 使用时点 | 说明 |
|---|---|---|---|
| **当前库存** | `stock_qty`（当前库存量） | 最新库存快照 | SKU 求和到客户；`0` 是有效库存 |
| 在途量 | `in_transit_qty`（在途量） | 最新库存快照 | 与库存分列，不能混为一个值 |
| 待发货金额 | `unship_amount`（下单未发货金额） | 最新库存快照 | 已下单未发金额，不得由目标差额推算 |
| 月度预算额 | `budget_amount` | 预测计划月 | 主预测 SKU 预算/金额为空时才补，不能覆盖权威预测金额 |
| 已发货金额 | `ship_amount` | 预测计划月 | 主预测实际为空时才补；说明为外部订单事实 |
| 周转天数 | `average_turnover_days`（平均周转天数 SKU 层） | 最新库存快照 | 客户级对非空 SKU 做算术平均 |
| 平均货龄 | `average_stock_age` | 最新库存快照 | 客户级对非空 SKU 做算术平均 |
| 目标库存 | `target_inventory` | 最新库存快照 | 仅补货辅助，不参与预测金额 |
| 日销 | `daily_sales` | 最新库存快照 | 仅补货辅助 |
| 建议订货 | `pre_order_qty` | 最新库存快照 | 仅补货辅助 |
| 库存与目标差额 | `inventory_gap_to_target` | 最新库存快照 | 仅补货辅助 |
| 品类 | `product_categ_name` | 最新库存快照 | 主预测品类为空时补齐 |

## 3. `customer_plan_execution`（客户计划执行）

关联：`cust_code = dealer_code`、`material_code = sku_code`、`stat_month = plan_month`。完整写法 `customer_plan_execution.<成员名>`。

| 工作台字段/用途 | Cube 成员 | 使用规则 |
|---|---|---|
| 品类 | `product_categ_name` | 主预测品类为空时补齐 |
| 实际发货量/达成量 | `shipped_quantity` | 主预测实际发货量为空时补；0 为有效值 |
| 最终计划量 | `plan_quantity` | 外部审批后计划量；历史计划偏离用 T-3/T-2/T-1，当前月不得混入 |
| 外部计划金额 | `plan_amount` | 历史月非空才计入历史提报月数；不得显示成「本 Agent 确认计划金额」 |

### 富集缺失降级

- 主预测明细可独立驱动客户队列、SKU 预测与核对。富集 Cube 不可用时相关字段返回 `null`/`-`，不得导致预测主流程失败。
- 库存快照与预测月不同是正常设计，界面说明「最新库存快照」，不得表述为预测月末库存。

---

## KPI 计算口径（工作台统一）

| 界面指标 | 取数/公式 | 边界 |
|---|---|---|
| **月度经营目标** | `category_budget_amount` 按 `plan_month + dealer_code + category_basis` 去重求和 | 不得直接跨 SKU 求和 |
| AI 销售计划/预测金额 | `SUM(predicted_sales_amount)` | 权威预测口径，保留元与原始精度 |
| 计划差额 | `AI预测金额 - 月度经营目标` | >0 红色、<0 绿色、=0 中性 |
| 计划差额率 | `计划差额 / 月度经营目标` | 目标为 0 时不除零 |
| 达成金额 | 完整历史月 `SUM(actual_sales_amount)`；必要时由 `ship_amount` 补为外部订单事实 | 目标月主事实不完整时不得显示完整达成 |
| 达成进度 | `实际发货金额 / 月度经营目标` | 只有完整实际金额时计算，否则 `-` |
| 当前库存/在途 | 最新快照 `SUM(stock_qty)` / `SUM(in_transit_qty)` | 分开保存和展示 |
| 周转天数/平均货龄 | 最新快照非空 SKU 的算术平均 | 无非空值显示 `-` |
| 待发货金额 | 最新快照 `SUM(unship_amount)` | 不得由目标差额推算 |

---

## Cube JSON Query 示例

### ① 查省区经销商列表（含月度经营目标）

```json
{
  "measures": [
    "agents_sales_forecast_detail.predicted_sales_amount",
    "agents_sales_forecast_detail.predicted_quantity"
  ],
  "dimensions": [
    "agents_sales_forecast_detail.dealer_code",
    "agents_sales_forecast_detail.dealer_name"
  ],
  "filters": [
    { "member": "agents_sales_forecast_detail.province_region_name", "operator": "equals", "values": ["豫南经销省区"] },
    { "member": "agents_sales_forecast_detail.plan_month", "operator": "equals", "values": ["202608"] }
  ],
  "order": { "agents_sales_forecast_detail.predicted_sales_amount": "desc" },
  "limit": 50
}
```

### ② 查经销商 SKU 预测明细

```json
{
  "measures": [
    "agents_sales_forecast_detail.predicted_sales_amount",
    "agents_sales_forecast_detail.predicted_quantity"
  ],
  "dimensions": [
    "agents_sales_forecast_detail.sku_code",
    "agents_sales_forecast_detail.sku_name",
    "agents_sales_forecast_detail.product_categ_name",
    "agents_sales_forecast_detail.hit_confidence",
    "agents_sales_forecast_detail.delivery_probability"
  ],
  "filters": [
    { "member": "agents_sales_forecast_detail.dealer_code", "operator": "equals", "values": ["10154909"] },
    { "member": "agents_sales_forecast_detail.plan_month", "operator": "equals", "values": ["202608"] }
  ],
  "order": { "agents_sales_forecast_detail.predicted_sales_amount": "desc" },
  "limit": 50
}
```

### ③ 查经销商月度经营目标（品类预算去重）

```json
{
  "measures": ["agents_sales_forecast_detail.predicted_sales_amount"],
  "dimensions": [
    "agents_sales_forecast_detail.dealer_code",
    "agents_sales_forecast_detail.category_basis",
    "agents_sales_forecast_detail.category_budget_amount"
  ],
  "filters": [
    { "member": "agents_sales_forecast_detail.dealer_code", "operator": "equals", "values": ["10154909"] },
    { "member": "agents_sales_forecast_detail.plan_month", "operator": "equals", "values": ["202608"] }
  ]
}
```

> 说明：`category_budget_amount` 是 SKU 行上重复的品类级预算，必须按 `category_basis` 去重后求和才是该客户月度经营目标。实测河南硕鸣 202608：3 条 `category_basis`，预算合计 2,316,620.00 元。

### ④ 查客户当前库存/在途/待发货（最新快照）

```json
{
  "measures": [
    "dealer_plan_order.stock_qty",
    "dealer_plan_order.in_transit_qty",
    "dealer_plan_order.unship_amount",
    "dealer_plan_order.average_turnover_days"
  ],
  "dimensions": ["dealer_plan_order.cust_code"],
  "filters": [
    { "member": "dealer_plan_order.cust_code", "operator": "equals", "values": ["10154909"] }
  ]
}
```

### ⑤ 查外部计划执行（参考）

```json
{
  "measures": [
    "customer_plan_execution.plan_quantity",
    "customer_plan_execution.plan_amount",
    "customer_plan_execution.shipped_quantity"
  ],
  "dimensions": ["customer_plan_execution.cust_code"],
  "filters": [
    { "member": "customer_plan_execution.cust_code", "operator": "equals", "values": ["10154909"] }
  ]
}
```

### ⑥ 查可用计划月份

```json
{
  "measures": ["agents_sales_forecast_detail.dealer_count"],
  "dimensions": ["agents_sales_forecast_detail.plan_month"],
  "order": { "agents_sales_forecast_detail.plan_month": "desc" },
  "limit": 12
}
```

---

## 把握度 / 置信度 / 发货概率

三者不同，勿混淆：

| 字段 | 含义 | 取值 | 展示规则 |
|---|---|---|---|
| `hit_confidence`（命中把握） | 基于近 6 月发货月数 | 高/中/低/其他 | 高→high、中→medium、低→low；其他值 unknown 展示 `-` |
| `delivery_probability`（发货发生概率） | SKU 明细概率 | 0~1 浮点，可 null | 明细列直接展示；null 展示「待复核」/`-`，不得补 0 |
| `average_delivery_probability`（平均发货概率） | 聚合概率 | 0~1 | 聚合查询放 measures |

把握度口径：近 6 个月有进货的 SKU —— 6 个月高把握、4–5 个月中、1–3 个月低；新品无近 6 个月历史不自动分配（除非有铺货计划）。

> 旧版「按 delivery_probability ≥0.8 高 / 0.5~0.8 中 / <0.5 低」的阈值映射**已废弃**——把握度应看 `hit_confidence`，发货概率只是明细数值。

---

## 业务规则速查

| 如果 | 就 | 边界 |
|---|---|---|
| 汇总预测销额 | 以 SKU `predicted_sales_amount` 聚合为准 | 不得另造预测口径 |
| 汇总品类预算/月度经营目标 | 按「月份 + 经销商 + `category_basis`」去重后汇总 | 不得按物料组去重，不得跨 SKU 直接求和 |
| SKU 近 6 个月有进货 | 6 个月高、4–5 中、1–3 低把握 | 新品无近 6 月历史不自动分配，除非有铺货计划 |
| SKU 是启用且审核通过的汤圆/元宵、粽子/端午 | 应用节日修正 | 春卷只监控、系数 1；普通/历史不足/未启用也系数 1 |
| 有预测销额且有未税到岸价 | 订货辅助量 = `max(1, round(预测销额/未税到岸价))` | 数量取整差额不得反向修改预测销额 |
| 缺少未税到岸价 | 明确数量不可得 | 仍保留预测销额，不补造数量 |
| 实际数据完整的历史月 | 可计算达成、准确率、WAPE、整体偏差 | WAPE 是绝对偏差；整体偏差率只说明方向 |
| 目标月实际销额为空/不完整 | 输出「目标月尚未完整、不能评价达成」 | 0 是实际值；只有 null/未全量回填是缺失 |
| 展示金额 | 保留原始数值及元 | 不换算万/亿，不自行改精度 |
| 差额 >0 / <0 / =0 | 红色/绿色/中性 | 适用于 KPI、客户队列、SKU |
| 月初/纠偏计划窗口 | 月初 16–20 日建议下月；纠偏 10–12 日建议当月 | 窗口只影响建议，不作最终确认强制拦截 |

---

## 省区与河南

- 「河南」在语义模型中被拆分为多个销售省区：`豫南经销省区`、`豫北经销直管区`、`豫陕晋直营省区`。
- 用户说「河南经销省区」时，默认应查询 `豫南经销省区`（112 家）；河南实际分属 3 个销售省区：豫南 112 + 豫北 27 + 豫陕晋 1 = 140 家。

---

## 提报接口预留

> 说明：以下 `API_CONFIG` 与提报 payload 适用于旧版提报工作台模板（`plan-submit-template.legacy.html`）。新版审批工作台（`plan-submit-template.html`）无 `API_CONFIG`，其审批「同意/不同意」的提交接口待后端配置后接入，payload 结构可参照本节。

HTML 模板中 `API_CONFIG` 集中配置，后端封装 MCP 查询并暴露 HTTP 接口：

| 配置项 | 用途 | 期望响应 |
|---|---|---|
| `fetchProvinceDealersUrl` | 查省区经销商列表 | `[{code,name,amount,qty,targetAmount}]` |
| `fetchDealerSkusUrl` | 查经销商 SKU 明细 | `[{code,name,categ,confidence,prob,amount,qty}]` |
| `submitUrl` | 提报表格到其他系统 | 提报成功/失败状态 |

### 提报 payload 结构

```json
{
  "planType": "月初计划",
  "month": "202608",
  "province": "豫南经销省区",
  "dealer": { "code": "10154909", "name": "河南硕鸣供应链管理有限公司" },
  "skus": [
    {
      "skuCode": "10002581",
      "skuName": "BP思念2.5kg灌汤水饺猪肉大葱（4袋）",
      "category": "饺子",
      "hitConfidence": "高",
      "deliveryProbability": 0.928571,
      "aiAmount": 355502.84,
      "aiQty": 5996,
      "confirmQty": 5996,
      "confirmAmount": 355503
    }
  ],
  "submittedAt": "2026-08-19T00:00:00.000Z"
}
```

---

## 页面 ↔ Agent 桥接协议（CDP 直连新模板）

新模板（区域经理审批工作台）是纯前端原型，**没有 `window.__bridge`**。Agent 与页面交互全部走 **CDP 直连**（见 SKILL「操作右边 HTML 工作台」），用 `Runtime.evaluate` 直接调用页面全局函数、读写全局变量。

> 通道：`aionui-browser` MCP 与外部 `chrome-devtools` MCP 均不适用于 GEA 内置浏览器（前者与 AionUi CDP 桥接不兼容、后者连的是谷歌 Chrome）。正确通道是 CDP 直连 `AIONUI_CDP_ACTIVE_PORT`。

### 数据注入入口

`window.setApprovalWorkbenchData(payload)` —— 整体替换（非增量），完成标准化、筛选项重建、清空筛选、关闭弹窗、重绘，返回 `{customerCount, currentLevel, currentTab}`；等价事件 `window.dispatchEvent(new CustomEvent('approval-workbench:data', {detail: payload}))`。

payload 结构：`{ meta, permissions, customers[] }`
- `meta`：{period 计划月份, currentLevel 首次层级, currentTab 首次维度, aiTag 标签, averageTurnoverDays 平均库存周转天数, inventoryFocus 建议关注库存}
- `permissions`：审批阶段权限数组（缺省全 5 级）
- `customers[]`：经销商（id/code/name/area/province/region/base/stage/rejected/target/thisShip/nextShip/health/healthClass/ai/skus[]）

### 页面全局状态（可读）

| 状态 | 含义 |
|---|---|
| `currentLevel` | 当前审批层级（STAGES 之一） |
| `currentTab` | 当前聚合维度（area/province/region/base/customer） |
| `customers` | 注入后的数据源数组（内置 4 条占位，注入后为真实数据） |
| `currentAdjustTitle/Dim/Leaves/Readonly` | 调整弹窗上下文 |
| `getFilters()` | 当前筛选（area/prov/region/name/status） |
| `approvalCustomers()` | 当前层级可见经销商（含已通过） |
| `pendingCustomers()` | 当前层级待审经销商（可改） |

### 页面全局函数（可调）

| 函数 | 作用 |
|---|---|
| `setApprovalWorkbenchData(payload)` | 注入业务数据（整体替换） |
| `onStageChange(level)` | 切换审批层级 |
| `switchView(tab)` | 切换聚合维度 |
| `applyFilters()` / `resetFilters()` | 应用 / 清空筛选 |
| `openAdjust(path)` / `closeAdjust()` | 打开 / 关闭调整明细弹窗 |
| `switchAdjustDim(d)` | 切换调整弹窗维度 |
| `onCustInput(id, idx, field, val)` | 客户维度编辑 qty/amt |
| `onAggInput(dim, key, sku, field, val)` | 聚合维度编辑（按 base 占比分摊） |
| `adoptCustAi(id, idx)` / `adoptAggAi(dim, key, sku)` | 采纳 AI 建议 |
| `openProgressModal()` / `closeProgressModal()` | 提报进度树形下钻 |
| `renderQueue()` / `renderStageBar()` / `renderAdjust()` | 原地重渲染 |

### Agent 端闭环示例

```
1. Runtime.evaluate: currentLevel + currentTab               // 读用户当前看到的状态
2. 用户切层级/维度/筛选后 → Runtime.evaluate 读 getFilters()/approvalCustomers()
3. 用户要求定位某组织 → Runtime.evaluate: openAdjust('/河南省省区/安阳经销分区/10154901')
4. 用户要求改某 SKU → Runtime.evaluate: onCustInput(customerId, skuIdx, 'qty', 数值)
   或采纳建议 → adoptCustAi(customerId, skuIdx)
5. 读回验证 + Page.captureScreenshot
```

### 注意

- Agent 是请求-响应模式，页面事件不会自动唤醒 Agent；需用户补一句、命令队列或主动读状态。
- 填充真实数据时只通过 `setApprovalWorkbenchData(payload)` 注入业务数据，不改渲染/聚合/建议/分摊逻辑；局部微调可改 `customers` 后调 `renderQueue()` 原地刷新。
- 新审批工作台没有 `submitUrl`/`API_CONFIG`；其「同意/不同意」为前端交互动作，真正的提报/审批提交接口仍待后端配置（见「提报接口预留」）。

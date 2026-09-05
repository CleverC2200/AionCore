---
name: sales-forecast-submit
description: AI 销售计划需求预测提报工作流。当用户要求某省区需求预测、生成审批核对工作台、核对 SKU 预测或审批销售计划时，经 query_business_data 查询预测、库存和计划执行数据，自动展开 GEA 右侧浏览器并把真实数据注入审批工作台 HTML。
---

# AI 销售计划需求预测提报

按四步流程生成可提报的 HTML 工作台。范围足够明确时直接查询并展示结果，不先展示占位数据，也不等待用户手动打开右侧面板。

## 数据源（三个 Cube 语义模型）

| Cube | 中文语义 | 事实粒度 | 提供什么 | 禁止混用 |
|---|---|---|---|---|
| `agents_sales_forecast_detail` | 销售计划预测明细 | 计划月 × 经销商 × SKU | AI 预测金额/数量、品类预算、实际销额/数量、完整性计数、价格、置信度、概率、版本解释 | 预测金额的唯一主语义；不能用预订单或计划执行替代 |
| `dealer_plan_order` | 客户预订单、预算、发货与库存快照 | 客户 × SKU × 月度/快照 | 月度预算、发货金额、待发金额、**当前库存**、在途、周转、货龄、目标库存、建议订货 | 库存/订单语义；不能把其预算或发货说成 AI 预测或 Agent 确认计划 |
| `customer_plan_execution` | 客户计划执行情况 | 客户 × SKU × 统计月 | 品类、外部计划量/金额、发货量、历史提报覆盖 | 外部执行语义；plan_quantity/plan_amount 不能替代本工作台确认计划 |

> 注意：`query_business_data` 的 `inspect` 只返回部分 Cube（当前仅 `agents_sales_forecast_detail` + `safety_stock_snapshot`）。`dealer_plan_order`、`customer_plan_execution` 等 Cube 不出现不代表不存在，需按字段全名直接发 query 验证。完整字段映射见 `references/mcp_schema.md`。

## 四步流程

### 第 1 步：经销商数据获取

1. 从用户请求解析**销售省区**和**计划月份**；月份缺省时先查询并使用最新 `plan_month`。
   - 用户说「河南」且未要求三省区合并时，直接按默认 `豫南经销省区` 查询并在结果中说明该默认值；用户明确要求全河南时才合并 `豫南经销省区`、`豫北经销直管区`、`豫陕晋直营省区`。
   - 只有销售范围无法从请求或会话上下文确定时才追问；范围已经明确时不暂停确认。
2. 经 MCP `query_business_data` 查该省区经销商列表（含预测销额、预测数量、月度经营目标，按预测销额降序）。查询示例见 `references/mcp_schema.md`。
3. 用户已指定经销商时按指定范围查询；未指定时先取预测金额最高的 10 家生成首屏工作台，不要求用户先做选择。用户要求完整范围时再分页补齐。

### 第 2 步：AI 预测生成

1. 对选定的经销商，经 MCP 查其 SKU 预测明细：sku_code、sku_name、品类、命中把握（高/中/低）、发货概率、预测金额、预测数量。查询示例见 `references/mcp_schema.md`。
2. 并行富集：`dealer_plan_order`（当前库存、在途、待发货、周转、货龄）与 `customer_plan_execution`（外部计划量/金额）。富集失败时相关字段返回 `null`/`-`，不得阻断预测主流程。
3. 数据禁止用户自由填写 —— 全部来自 MCP 查询结果。

### 第 3 步：审批工作台填报

1. 用 `assets/plan-submit-template.html`（区域经理审批工作台）作为页面骨架，把本轮真实查询结果构造成标准 payload，并保存为当前会话目录下的 `approval-workbench-payload.json`。随后调用 `scripts/open-workbench.cjs` 自动打开右侧页面、注入并验证；不要临时生成 CDP 连接脚本：
   - payload 三层：`meta`（period 计划月份 / currentLevel 首次层级 / currentTab 首次维度 / aiTag 标签 / averageTurnoverDays 平均库存周转天数 / inventoryFocus 建议关注库存）、`permissions`（审批阶段权限数组，缺省=全部 5 级）、`customers[]`（经销商）。
   - `customers[]`：id/code/name、`area` 大区/`province` 省区/`region` 区域/`base` 基地（**组织字段**）、`stage` 流程阶段、`rejected` 驳回原因（非空默认不进入队列）、`target` 预算目标金额、`thisShip` 本月同期拉货量、`nextShip` 次月同期拉货量、`health/healthClass` 健康度（healthy/warning/danger）、`ai` AI 审批意见、`skus[]`。
   - `skus[]`：sku 编码 / name / `base` 原计划量（数量）/ `price` 单价 / `qty` 新计划量（缺省=base）/ `amtBase` 原计划金额（缺省=base×price）/ `amt` 新计划金额（缺省=qty×price）。
   - 页面结构：顶部流程阶段条（5 级审批）+ AI 建议区 + 审批核对队列（按大区/省区/区域/基地/客户聚合）+ 调整明细弹窗（逐 SKU 编辑或采纳 AI 建议）+ 提报进度弹窗（树形下钻）。
2. 唯一可人工介入的是「调整明细弹窗」里的 SKU 数量/金额（或采纳 AI 建议）；修改后实时同步回审批队列，聚合维度按各客户原计划量（`base`）占比向下分摊。
3. 填充数据时**只通过 `setApprovalWorkbenchData(payload)` 注入业务数据，不改页面逻辑**——首次展示由 `open-workbench.cjs` 调用该入口，后续原地更新仍可经 `Runtime.evaluate` 调用。模板的渲染/聚合/建议/分摊逻辑必须原样保留（详见下方「审批工作台逻辑速查」与 `assets/plan-submit-template.usage.md`、`references/mcp_schema.md`）。

### 第 4 步：提交 DMS 草稿

1. 收集当前表格内容，组装提报 payload（结构见 `references/mcp_schema.md` 的「提报接口预留」）。
2. 经 `API_CONFIG.submitUrl` 提交到其他系统；接口未配置时在控制台打印 payload 模拟提报，并明确告知用户「接口预留中」。
3. AI 预测是建议计划，不是最终计划。只有服务端返回 `status=confirmed` 且确认回执非空，才能说「最终确认完成」。

## 页面 ↔ Agent 实时桥接

新模板（区域经理审批工作台）是纯前端原型，**没有 `window.__bridge`**。Agent 与页面的交互全部走 **CDP 直连**（见下一节），通过 `Runtime.evaluate` 直接调用页面里的全局函数、读写全局变量。

- **数据注入入口**：`window.setApprovalWorkbenchData(payload)` —— 整体替换（非增量），完成数据标准化、筛选项重建、清空筛选、关闭弹窗、重绘，返回 `{customerCount, currentLevel, currentTab}`；等价事件 `window.dispatchEvent(new CustomEvent('approval-workbench:data', {detail: payload}))`。
- 页面全局状态（可读）：`currentLevel`（当前审批层级）、`currentTab`（当前聚合维度）、`customers`（注入后的数据源数组）、`currentAdjustTitle/currentAdjustDim/currentAdjustLeaves/currentAdjustReadonly`（调整弹窗上下文）。
- 页面全局函数（可调）：`onStageChange(level)`、`switchView(tab)`、`applyFilters()/resetFilters()`、`openAdjust(path)/closeAdjust()`、`switchAdjustDim(d)`、`onCustInput(id,idx,field,val)`、`onAggInput(dim,key,sku,field,val)`、`adoptCustAi(id,idx)`、`adoptAggAi(dim,key,sku)`、`openProgressModal()/closeProgressModal()`。
- 当用户在工作台切换层级/维度/筛选/打开弹窗时，Agent 用 `Runtime.evaluate` 读 `currentLevel/currentTab/getFilters()/approvalCustomers()/pendingCustomers()` 感知状态；需要重新查询时经 MCP `query_business_data` 查询后，重新构造 payload 调 `setApprovalWorkbenchData(payload)`。
- 注意：Agent 是请求-响应模式，页面事件不会自动唤醒 Agent；需用户补一句、命令队列或主动读状态。

## 操作右边 HTML 工作台（本 skill 的核心交互）

这个 skill 的关键，是**通过与用户互动，把数据填报进右边的 HTML 工作台**。右边那个预览区是 **GEA 内置浏览器（AionUi in-app browser）**，不是用户的谷歌 Chrome，也不是独立文件。

### 通道选择

直接走 GEA 客户端提供的单目标 CDP 通道。它只控制右侧当前可见的内置浏览器标签，不控制用户的独立 Chrome。

### CDP 直连步骤

首次展示直接执行技能内置助手：

```bash
node .aionrs/skills/sales-forecast-submit/scripts/open-workbench.cjs \
  --url 'file:///当前会话绝对路径/.aionrs/skills/sales-forecast-submit/assets/plan-submit-template.html' \
  --payload '/当前会话绝对路径/approval-workbench-payload.json' \
  --screenshot '/当前会话绝对路径/approval-workbench.png'
```

该脚本从 `AIONUI_CDP_ACTIVE_PORT` 发现当前页面，令牌只保留在进程内，并依次完成 `Page.navigate`、等待数据入口、注入、可见内容读回和可选截图。只把 payload 与截图作为会话产物；CDP 端口、`webSocketDebuggerUrl`、token 不得写入任何脚本、日志或回复。

### 自动展开与失败边界

- 无活动目标时，首条 `Page.navigate` 或 `Runtime.evaluate` 会触发客户端展开右侧浏览器；不要要求用户先打开面板或新建标签页。
- 客户端只把当前可见的内置浏览器标签绑定给 CDP；用户切换标签后继续操作当前可见页。
- 若助手返回浏览器目标不可用或连接失败，完整重跑助手一次；仍失败时停止重试，并提示用户重启或升级 GEA 客户端。

### 读页面（理解用户当前看到 / 选中的状态）

用 `Runtime.evaluate`（`returnByValue: true`）执行：
- `currentLevel` → 当前审批层级（客户确认AI预测/区域审批/省区审批/大区审批/品类计划审核）
- `currentTab` → 当前聚合维度（area/province/region/base/customer）
- `getFilters()` → 当前筛选（area/prov/region/name/status）
- `approvalCustomers()` → 当前层级可见（含已通过）的经销商列表（含 skus）
- `pendingCustomers()` → 当前层级正待审（可改）的经销商列表
- `customers` → 注入后的真实数据源数组；注入验证时确认不存在 `placeholder-` 或“待 MCP 填充”
- `document.getElementById('queueBody').textContent` → 当前审批队列渲染结果（识别用户正看到哪些组织）
- 调整弹窗：`document.getElementById('adjustModal').classList.contains('active')` 判断是否打开；`currentAdjustTitle / currentAdjustDim / currentAdjustLeaves / currentAdjustReadonly` 读弹窗上下文

### 写页面（帮用户定位 / 填报 / 审批）

- **注入业务数据（整体替换占位）**：`window.setApprovalWorkbenchData({meta:{period,currentLevel,currentTab,aiTag,averageTurnoverDays,inventoryFocus}, permissions:[...], customers:[...]})` —— 重建筛选项、清空筛选、关闭弹窗、重绘，返回 `{customerCount, currentLevel, currentTab}`
- 切换审批层级：`onStageChange('省区审批')`（重渲染阶段条 + 维度 tab + 队列 + AI 说明）
- 切换聚合维度：`switchView('customer')`（按大区/省区/区域/基地/客户聚合队列）
- 筛选定位：先设筛选器再 `applyFilters()`：`document.getElementById('fName').value='10154901'; applyFilters();`（筛选器 id：`fArea/fProv/fRegion/fName/fStatus`）；`resetFilters()` 清空
- 打开调整弹窗：`openAdjust('<path>')`（path = 维度分组路径 + 客户 code，如 `/河南省省区/安阳经销分区/10154901`；可先从队列行 `data-path` 属性或 `findNodeByPath` 取）
- 编辑 SKU：`onCustInput(customerId, skuIdx, 'qty'|'amt', 数值)`（客户维度，qty↔amt 按单价联动）；`onAggInput(dim, key, sku, 'qty'|'amt', 数值)`（聚合维度，按 base 占比向下分摊）
- 采纳 AI 建议：`adoptCustAi(customerId, skuIdx)` / `adoptAggAi(dim, key, sku)`
- 查看提报进度：`openProgressModal()`（树形下钻下级提报/审批完成率）
- 原地更新数据（不 reload、不闪屏）：直接改 `customers` 里的 `sku.qty/amt` 后调 `renderQueue()`（队列）或 `renderAdjust()`（弹窗）；批量替换则重新调 `setApprovalWorkbenchData(payload)`
- 每次操作后，读回 `currentLevel / currentTab / approvalCustomers()` + `Page.captureScreenshot` 验证，再向用户汇报。

### 互动节奏

1. 首次触发时先完成 MCP 查询、保存 payload，再运行 `open-workbench.cjs`；只有脚本返回客户数量/层级/维度且 `hasPlaceholder=false` 后才能回复“已注入”。
2. 用户在右边 HTML 手动操作（切层级、切维度、筛选、改 SKU 数量、采纳建议）后，Agent **主动重新读状态**（`currentLevel / currentTab / getFilters() / approvalCustomers()`）。
3. 用户让 Agent 操作时，Agent 通过 CDP 执行 → 读回验证 → 截图反馈。
4. 数据变更优先原地更新（改 `customers` 后 `renderQueue()`/`renderAdjust()`，不 reload、不闪屏）；只有改 HTML 结构本身才 reload（且只 reload 预览面板，绝不碰对话区或整个 AionUi）。

## 关键字段映射

完整映射与 Cube JSON Query 示例见 `references/mcp_schema.md`。核心字段速记：

| 模板/业务字段 | MCP 字段 | Cube |
|---|---|---|
| 销售省区 | `province_region_name` / `province_region_code` | agents_sales_forecast_detail |
| 经销商 | `dealer_code` / `dealer_name` | agents_sales_forecast_detail |
| SKU | `sku_code` / `sku_name` | agents_sales_forecast_detail |
| 品类 | `product_categ_name` | agents_sales_forecast_detail |
| 预测金额 | `predicted_sales_amount` | agents_sales_forecast_detail |
| 预测数量 | `predicted_quantity` | agents_sales_forecast_detail |
| 命中把握（置信度） | `hit_confidence`（高/中/低/未知） | agents_sales_forecast_detail |
| 发货概率 | `delivery_probability`（SKU 明细，0~1，可 null） | agents_sales_forecast_detail |
| **月度经营目标** | `category_budget_amount` 按 `plan_month+dealer_code+category_basis` 去重求和 | agents_sales_forecast_detail |
| 品类预算金额 | `category_budget_amount`（SKU 行上重复，去重键 `category_basis`） | agents_sales_forecast_detail |
| 实际销额 | `actual_sales_amount`（仅完整历史月可用） | agents_sales_forecast_detail |
| **当前库存** | `stock_qty` | dealer_plan_order |
| 在途量 | `in_transit_qty` | dealer_plan_order |
| 待发货金额 | `unship_amount` | dealer_plan_order |
| 月度预算额 | `budget_amount` | dealer_plan_order |
| 已发货金额 | `ship_amount` | dealer_plan_order |
| 周转天数 | `average_turnover_days` | dealer_plan_order |
| 平均货龄 | `average_stock_age` | dealer_plan_order |
| 外部计划量/金额 | `plan_quantity` / `plan_amount` | customer_plan_execution |
| 外部发货量 | `shipped_quantity` | customer_plan_execution |
| 计划月份 | `plan_month`（YYYYMM） | agents_sales_forecast_detail |
| 审批工作台·预算目标 `target` | `category_budget_amount` 去重求和（金额） | agents_sales_forecast_detail |
| 审批工作台·本月同期拉货量 `thisShip` | 本月拉货**量**（数量口径，非金额） | dealer_plan_order |
| 审批工作台·次月同期拉货量 `nextShip` | 次月拉货**量**（数量口径，非金额） | dealer_plan_order |
| 审批工作台·健康度 `healthClass` | 由 `average_turnover_days`/`average_stock_age` 推导（healthy/warning/danger） | dealer_plan_order |
| 审批工作台·平均周转天数 `meta.averageTurnoverDays` | `average_turnover_days` | dealer_plan_order |
| 审批工作台·组织维度 `area/province/region/base` | 由经销商主数据推导（MCP 无直接字段则留占位） | 主数据 |

## 审批工作台逻辑速查（模板核心逻辑，勿改动）

新模板 `assets/plan-submit-template.html` 是「区域经理审批」工作台。Agent **只通过 `setApprovalWorkbenchData(payload)` 注入业务数据**，以下逻辑必须原样保留：

- **数据注入**：`window.setApprovalWorkbenchData(payload)` 整体替换（非增量）→ 逐条 `normalizeCustomer`/`normalizeSku` 标准化 → 重建筛选项 → 清空筛选/关闭弹窗 → 重绘；等价 `window.dispatchEvent(new CustomEvent('approval-workbench:data', {detail: payload}))`。
- **payload 结构**：`{ meta, permissions, customers[] }`；`meta` = {period, currentLevel, currentTab, aiTag, averageTurnoverDays, inventoryFocus}；`permissions` = 审批阶段权限数组（缺省全 5 级）。
- **数据模型**：`customers[]` 每条 = 经销商（id/code/name/area 大区/province 省区/region 区域/base 基地组织字段/stage 流程阶段/rejected 驳回原因/target 预算目标金额/thisShip 本月同期拉货量/nextShip 次月同期拉货量/health+healthClass 健康度/ai 审批意见/skus[]）；`skus[]` = {sku, name, base 原计划量, price 单价, qty 新计划量(默认=base), amtBase 原计划金额(默认=base×price), amt 新计划金额(默认=qty×price)}。
- **标准化规则**：文本字段转义（只传业务数据，不传 HTML/脚本）；无效数字按 0；缺失文本「待填充」；无效 `stage` 回退「客户确认AI预测」；无效 `healthClass` 回退 healthy。
- **审批链路 STAGES**：`客户确认AI预测(0) → 区域审批(1) → 省区审批(2) → 大区审批(3) → 品类计划审核(4)`；`RANK` 映射顺序，`currentLevel` 默认 `大区审批`，`permissions` 决定阶段条可点。
- **聚合维度**：`TAB_DEFS` = area/province/region/base/customer；`TAB_BY_LEVEL` 定义每层级可见维度（客户确认仅 customer；区域/省区 = region+customer；大区 = province+region+customer；品类计划审核 = 全部 5 个）；`PATHS` 定义各维度分组层级（area→[area,province,region]、province→[province,region]、region→[region]、base→[base]、customer→[]）；`DIM_LABEL` 维度中文名。
- **审批范围**：`approvalCustomers()` = 过滤后只保留 `RANK[stage] >= RANK[currentLevel]`；`pendingCustomers()` = 其中 `stage === currentLevel`（待审、可改）。`rejected` 非空默认不进入队列；`status` 筛 pending=停当前层级 / approved=已超过当前层级。
- **可编辑规则**：分组/客户「可改」当且仅当该组所有叶子客户 `stage === currentLevel`（`editable`）；调整弹窗 `readonly` 当任一客户已过审。
- **编辑与分摊**：客户维度 `onCustInput` 改 qty↔amt 按单价联动；聚合维度 `onAggInput`/`adoptAggAi` 改总量后按各客户该 SKU 的 `base` 占比向下分摊。
- **AI 建议规则**：`skuCategory` 识别品类（水饺/汤圆/面点/馄饨/丸子/通用）；客户维度优先级 danger→下调 15%（×0.85）、warning→达成率>1.05 下调 5% 否则上调 5%、healthy→达成率<0.9 上调 10% / >1.15 ×0.95 / 否则维持；聚合维度 danger 下调 15% / warning 上调 5% / healthy 维持。
- **提报进度**：`PROG_TOP` 定义每层级展示的下级维度（大区审批→省区、品类计划审核→大区…），`renderProgress` 树形下钻，`pct = reached/total`。
- **原型边界**：同意/不同意/导出按钮仅展示未接后端；复选框只维护选中样式；分页仅「每页条数」可用（上/下/页码按钮未实现）；调整不回写 MCP/DB（需 Agent 读页面状态后调独立写入接口）；刷新或浏览器重建后按当前 HTML 路径恢复 24 小时内最近一次真实注入，未提交的页面调整恢复为该次注入值。
- **填充约定**：`target` ← 月度经营目标金额、`thisShip` ← 本月同期拉货量、`nextShip` ← 次月同期拉货量、`healthClass` ← 由周转/货龄推导、`meta.averageTurnoverDays` ← 平均库存周转天数、`skus[].base` ← 预测数量、`skus[].price` ← 未税到岸价。

## 业务规则（必须遵守）

- **预测金额**：一律用 `predicted_sales_amount`，不得用实际销额、品类预算或预订单发货替代；经销商/物料组/总览都由 SKU 明细聚合。
- **月度经营目标**：= `category_budget_amount` 按「月份 + 经销商 + `category_basis`」去重后求和；不得按物料组去重或跨 SKU 直接求和。
- **把握度**：近 6 个月有进货的 SKU —— 6 个月高把握、4–5 个月中、1–3 个月低；新品无近 6 个月历史不自动分配（除非有铺货计划）。`hit_confidence` 高/中/低/其他未知展示 `-`。
- **节日修正**：只对启用且审核通过的汤圆/元宵、粽子/端午 SKU 生效；春卷只监控、系数为 1；普通、历史不足、未启用规则 SKU 也按系数 1。
- **数量换算**：有预测销额且有未税到岸价时，订货辅助量 = `max(1, round(预测销额/未税到岸价))`；数量取整差额不得反向修改预测销额；缺价格时明确数量不可得。
- **实际完整性**：实际达成只在完整历史月评价 —— `row_count > 0` 且 `actual_record_count = row_count` 且月份已结束。目标月实际为空/不完整时回答「目标月尚未完整、不能评价达成」；`0` 是实际值，只有 `null`/未全量回填是缺失。
- **差额配色**：差额 > 0 红色、< 0 绿色、= 0 中性（适用于 KPI、客户队列、SKU）。
- **金额精度**：保留原始数值及「元」，不换算万/亿，不自行改精度。
- **计划窗口**：月初计划每月 16–20 日建议下月；纠偏计划每月 10–12 日建议当月。窗口只影响建议月份与计划类型，**不作最终确认/审批强制拦截**。

## 资源

- `assets/plan-submit-template.html` — 区域经理审批工作台模板（流程阶段条 + AI 建议区 + 审批核对队列 + 调整明细弹窗 + AI 建议采纳 + 提报进度弹窗）。通过 `window.setApprovalWorkbenchData(payload)` 注入数据；内置占位 payload 只用于模板单独打开时自检，技能执行必须立即替换为本轮真实查询结果。
- `assets/plan-submit-template.usage.md` — 模板使用说明（payload 结构、字段说明、注入规则、原型边界、常见问题排查）。
- `scripts/open-workbench.cjs` — 从当前客户端动态发现 CDP 目标，在内存中完成导航、注入、验证和截图；禁止用临时脚本替代它。
- `assets/plan-submit-template.legacy.html` — 旧版提报工作台模板（四步进度条 + SKU 明细表 + `window.__bridge`），已弃用仅存档。
- `references/mcp_schema.md` — 三个 Cube 语义模型字段映射、KPI 计算口径、Cube JSON Query 示例、置信度/把握度映射、业务规则、提报接口约定、页面桥接协议。

## 注意事项

- 预测金额一律用 `predicted_sales_amount`，不得用实际销额或品类预算替代。
- 「月度经营目标」= 品类预算金额 `category_budget_amount` 去重求和；「当前库存」= `dealer_plan_order.stock_qty`。两者都在 MCP 里，勿再写「语义模型无此字段」。
- `delivery_probability` 为 null 时展示「待复核」/`-`，不要补成 0；把握度用 `hit_confidence`，不要用 `delivery_probability` 阈值去映射高/中/低。
- 富集 Cube（`dealer_plan_order`、`customer_plan_execution`）不可用时，相关字段返回 `null`/`-`，不得导致预测主流程失败。
- 当前确认计划只认本工作台确认服务结果；`customer_plan_execution.plan_quantity/plan_amount` 是外部执行参考，不得说成「Agent 已确认计划」。
- 模板内 MCP 查询由后端封装成 HTTP 接口（fetch 配置项）；前端不直接调 MCP。

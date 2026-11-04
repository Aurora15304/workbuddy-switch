# 连接器同步：可行性结论与未来实施约束

- 状态：**本文档只给结论，不实施**。本轮代码零改动，`crates/` 内对 `connectors` 的引用数为 0（`grep -rn "connectors" --include=*.rs crates | wc -l` → 0）。
- 判定依据见 `docs/adr/0002-cross-account-migration-invariant.md`，**判定原则以 `docs/adr/0001-cross-account-migration-invariants.md` 为准**（「裁决方」口径、禁写清单、highest-restriction-wins）。
- 结论时间：2026-11-04。

## 1. 结论

1. **技术上可搬**：整套凭据栈（`.master.key` + `connector-states*.json` + `.credentials.v3.json`）是对称加密 + HKDF，只要有来源账号的身份与密钥，就能解密再按目标账号重加密（re-wrap）。合成向量已实测通过（ADR 附录 C.3）。
2. **语义上不该搬 token**：OAuth token 绑定的是**授权主体**（来源账号的授权）。把它搬到目标账号，等于让目标账号的会话代表来源账号行使授权——这不是「同步配置」，是「冒用授权」。任何实现都不应默认做这件事。
3. **该同步的是「无身份语义的配置项」**：server 地址、header/env 覆盖项（剥离凭证后）、启用开关。token 由目标账号自己走一次授权即可，用户为此多点一次「连接」，换掉一整类安全事故。
4. **当前没有存量要迁**：本机三个账号的连接器状态全空，且都没有 `.credentials.v3.json`。也就是说这件事今天是「未来功能」，不是「待迁移数据」。

## 2. 现状取证（只读）

| 项 | 结果 | 证据 |
| :--- | :--- | :--- |
| 三账号连接器目录 | 各含 `.master.key`(32B) + `README-DO-NOT-DELETE.txt` + `connector-states.json` + `connector-states.v3.json` | `ls -a ~/.workbuddy/connectors/<uid>/` |
| 是否有已授权令牌 | **无**：三个目录都没有 `.credentials.v3.json` | 同上 |
| 连接器状态内容 | v4 文件 `connectors: {}`；v3 文件 `enabled: []`、`everConnected: []`、`userDisabled: {}` | `python -c` 读键（见 §7） |
| 归属声明 | 文件内 `accountIdentityKey = "<uid>||enterprise"`，即数据自己声明属于谁 | 同上 |
| 三账号主密钥 | 互不相同，各 32 字节（sha256 前 12 位 `c8a0e7e5b5f1` / `b8bef7610636` / `f85a38827b77`） | `sha256sum ~/.workbuddy/connectors/*/.master.key` |
| 校验和绑定 | `userIdCheck` / `keyCheck` 在三账号上均可用公式复算通过 | ADR 附录 C.2 |
| 剥离标记 | 两个文件都带 `headerOverridesBearerStripped: true`、`staleManagedAuthHeadersPurged: true`（客户端做过一次性「剥离 Bearer / 清理陈旧托管头」迁移） | 同上 |
| 备份密钥 | `~/.workbuddy-key-fallback/connector-keys/` 共 5 个 `*.key`：3 个与当前账号一一对应且与主副本**字节相同**，2 个无对应账号目录（孤儿） | `ls ~/.workbuddy-key-fallback/connector-keys/` |

## 3. 为什么不能原样拷贝（一条命令就能复现的坑）

```text
connectors/<来源 uid>/.master.key             ← 密钥按账号独立随机
connectors/<来源 uid>/connector-states.json   ← header.salt 随机；userIdCheck/keyCheck 由
                                                 「userId‖salt」与「masterKey‖salt」算出；
                                                 AAD 里还钉着 userId|serverKey|fieldName
        │
        └─ cp -r 到 connectors/<目标 uid>/ ──► 读时 verifyHeader：
             userIdCheck 不符 → 抛 `invalid header (userId-mismatch)`
             keyCheck    不符 → 抛 `invalid header (key-mismatch)`
             两者都"凑"上去 → AES-GCM 解包 `InvalidTag`（因为 AAD 与 KDF 输入里的 userId 变了）
```

**三处失败中任何一处都不是静默的**（客户端在这条路径上区分了三种 reason），但用户看到的结果一样：连接器状态变成空/需重新连接。所以不要试图「让文件看起来对」——只能 re-wrap。

## 4. 推荐形态：同步「配置模板」，不搬「授权凭据」

把「配置」和「授权」分开：导出一份与账号无关的配置模板，目标账号导入后自己完成一次授权。

| 字段 | 同步？ | 理由 |
| :--- | :--- | :--- |
| `connectors.<serverKey>` 下的 `server` / URL / 端点 | 同步 | 无身份语义 |
| `headerOverrides`（**已剥离** `authorization` / `cookie` / `x-api-key` 等凭证头） | 同步 | 剩余部分是自定义路由/租户参数 |
| `envOverrides`（黑名单剔除 `*_TOKEN` / `*_KEY` / `*_SECRET` / `*_PASSWORD`） | 同步 | 同上 |
| `enabled` 启用开关、`userDisabled` | 同步 | 纯偏好 |
| `everConnected`、任何「最近使用」时间戳 | **不同步** | 使用痕迹，搬过去等于伪造记录（同 `experts.rs` / `memory.rs` 的取舍） |
| `connectors.<serverKey>.accessToken` / `refreshToken` / `apiKey` | **不同步** | 授权主体绑定，见 §1 第 2 条 |
| `.master.key`、`encryption{salt, userIdCheck, keyCheck}` | **不同步** | 目标账号自己的密钥材料，只允许它的客户端生成 |
| `accountIdentityKey` | 必须重写 | 必须等于目标 `"<uid>||enterprise"`，否则数据自我声明属于别人 |

实现要点（若将来做）：

- **落点**：模板写成独立的、不含密文的 JSON（例如 `~/.wb-switch/connector-profile.json`），而不是去改 `~/.workbuddy/connectors/` 下的加密文件。这样「同步配置」与「客户端私有凭据」彻底解耦，客户端升级也不会把我们的写入当成损坏数据。
- **双态兼容**：用户可能先手动在新账号里配了一半，再导入模板。合并规则按 `serverKey` 取并集、同键以目标已有值为准（或显式让用户选），**不删任何目标已有项**——与 `experts.rs` 的「只增不减」一致。
- **幂等**：以模板内容（解析后的规范化 JSON）比对，一致则不写、不留备份。
- **可回滚**：写目标前把原文件备份成 `<name>.bak-<毫秒时间戳>`。

## 5. 若未来真要做 token 搬迁：必须同时满足的 5 条安全约束

以下不是建议，是「少一条就不该合并」的门槛。

1. **明文只活在内存**：解密后的 token 不得写任何临时文件、日志、诊断包、崩溃转储；进程内用完即清零（Rust 侧用 `zeroize`，禁止 `String` 中转拷贝）。
2. **不搬 `refresh_token`**：`refresh_token` 能在未来任意时刻续出新 `access_token`，搬它等于把来源账号的长期授权交给目标账号。只允许搬短生命周期 `access_token`，并在写入前记录其 `expires_at`，过期即不搬。
3. **授权主体可辨识且可撤销**：同步后在目标账号必须能看清「这条连接用的是谁授权的 token」并提供一键撤销；技术上必须能对上目标账号主体（无接口可核对时**不搬**）。跨账号使用 token 本身违反多数平台条款，必须由用户显式确认一次，不能默认开启。
4. **原子重绑 + 崩溃一致**：header（`salt` / `userIdCheck` / `keyCheck`）与密文必须同源同一次写入完成——先落密文再落 header 会产生「header 说这把钥匙，密文却是另一把」的半写态。要求：整文件走 `临时文件 + fsync + rename` 原子替换（同 `session_backup::durable_write` 的做法），并在启动自检发现不一致时**隔离上报**，绝不清空用户数据。
5. **互斥与防重入**：同步是一次跨账号写操作，必须带跨进程锁（同 `session_link.rs` 的档位锁约定）+ 幂等键（`sourceUid|targetUid|serverKey`），运行期间拦截重复触发；报告逐条列出「成功 / 跳过 / 需重新授权」三态，禁止用空集合表示失败（见 ADR 附录 B）。

## 6. 非目标（本次明确不做）

- 不新增任何写入 `~/.workbuddy/connectors/` 的代码路径。
- 不实现 token re-wrap，不提供「一键把连接器搬过去」的按钮。
- 不修改 `.master.key`，不读取任何真实令牌内容。
- 不在 UI 增加连接器同步入口。

一句话给后来者：**看到「连接器同步」这四个字，先分清是「搬配置」还是「搬授权」。前者是功能，后者是安全事故。**

## 7. 只读取证命令

```bash
# 1) 三账号目录与文件清单（应无 .credentials.v3.json）
for d in ~/.workbuddy/connectors/*/; do echo "== $d"; ls -a "$d"; done

# 2) 状态是否为空 + 归属声明（不打印任何密钥/salt 字节）
python - <<'PY'
import json, glob, os
for p in glob.glob(os.path.expanduser("~/.workbuddy/connectors/*/connector-states*.json")):
    j = json.load(open(p, encoding="utf-8")); e = j.get("encryption", {})
    print(os.path.basename(os.path.dirname(p))[:8], os.path.basename(p),
          "version=", j.get("version"),
          "connectors=", len(j.get("connectors", {}) or {}),
          "enabled=", len(j.get("enabled", []) or []),
          "everConnected=", len(j.get("everConnected", []) or []),
          "identity=", (j.get("accountIdentityKey") or "")[:8] + "…",
          "encFields=", sorted(e.keys()))
PY

# 3) 主密钥互不相同（只看摘要前缀）
for f in ~/.workbuddy/connectors/*/.master.key; do
  printf "%s bytes=%s sha12=%s\n" "$(basename "$(dirname "$f")")" "$(wc -c < "$f")" "$(sha256sum "$f" | cut -c1-12)"
done
```

## 8. 先例检索与可重放边界（正文由叶探真提供，顾明析-2 原样转录）

### 8.1 检索范围与结论量词

截至 2026-11-04，基于约 14 次 GitHub 仓库检索、约 20 个仓库元数据核查、8 个文档 / 页面
精读，**未发现**可复用实现（域 = 开源生态，无界且随时间变化，故结论只能为**暂定**）。

判定标准**不是词形**，而是逻辑形式：
> 这个结论要成立，需要对某个集合做穷举吗？需要 → 那个集合就是它的域 → 再问域是否有界。

按词形（「无 / 不存在 / 没有」）过滤会漏掉最要紧的一条：**「四方向零同构 / 不需要引外部库 /
只能自建」都不含否定词，但全都是负存在断言**。

**若发现任一实现：删除本节结论，并把方案中「因无范式故自建」改为「参考该实现」。
不要保留「无先例」表述再补一句「除外」。**

### 8.2 失效方式声明（缺此则本结论成为高风险动作）

| 情况 | 真实性质 | 归责 |
| --- | --- | --- |
| 域内本来就有实现，当时没搜到 | 结论被**推翻** | 检索方漏了 |
| 当时确实没有，之后才出现 | 结论**过期** | 无人错，时点变了 |

不区分这两种，就会把「诚实降级」与「拍胸脯」的事后风险拉到同一档 —— **那没人有动机降级**。
本声明的作用是让「诚实降级」从高风险动作变成零风险动作。

### 8.3 可重放边界

检索式只保证**方法可复现**（同一 `q`、同一源），不保证**结果可复现**——时变域的搜索结果会变。
所以它只是**部分**替代「在场否证者」，且仅在**有界域**（仓库@某 commit、本机、客户端版本）
完整成立。复核者重跑得到不同结果时，按 8.2 判性质，**不要默认视为原检索方漏检**。

### 8.4 逐条对标（相关但据公开文档判断不解决本问题）

| 项目 | star | License | 与本问题的关系 |
| --- | --- | --- | --- |
| `farion1231/cc-switch` | 151,584 | MIT | Tauri2 + Rust 同构；MCP/Skills/Prompts 按工具勾选同步，但同步的是**配置**，token 明文存本地，不做跨账号搬运 |
| `jlcodes99/cockpit-tools` | 19,687 | **无 License（代码不可复用）** | 已支持 CodeBuddy 多账号；导出刻意脱敏、不含 token |
| `Lampese/codex-switcher` | 919 | MIT | 「Safe Account Switching」：切号前先回存当前账号最新 token；刷新与切号串行化。**记录 rotation 互踢这一坑** |
| `nicepkg/vsync` | 63 | MIT | source→target 单向 + Safe/Prune 双模 + hash manifest。跨工具，非跨账号 |
| `punkpeye/mcp-remote` | 1,661 | MIT | OAuth token 缓存 `~/.mcp-auth`；缓存键 = server URL × resource × headers × authorize-param，**不含客户端账号** |

证据强度：以上均为**读 README / 公开文档**所得，非实测其运行行为。
数据时点：上述 star 数与 2026-11-04 的 API 返回一致，会随时间变化，**引用时不得省略时点**。

> **编号警告（恢复后仍有效）**：ADR-0001 正文里的「§4」指**本文档被覆盖前**的「先例检索/样本量」小节，与本文当前的 §4（推荐形态）**不是同一节**。ADR-0001 引用该节时应改为「§8.1 / §8.3」，避免指错。

## 待验证

| 项 | 已知 | 缺什么 |
| :--- | :--- | :--- |
| `headerOverrides` 的剥离是否完备 | 两个状态文件都带 `headerOverridesBearerStripped: true` 标记，说明客户端做过一次性迁移 | 本机三账号 `headerOverrides` 均为空对象，无法实证「任意历史写入路径是否都可能残留 `Authorization`」。若将来做 §4，**必须自己再剥一次**，不能只信这个标记 |
| `connector-states.json` 与 `connector-states.v3.json` 的读取优先级 | 两文件并存，v4 有 `connectors` 字段、v3 有 `enabled/everConnected` | 客户端读哪一个为准、旧文件何时淘汰。做同步前必须先确认单一真相源 |
| 新版非对称信封是否是未来方向 | 客户端包内已有 `WBEF1` / `asym-v1` / `developerPublicKey` / `fileKeyId` 的整文件信封实现 | 若连接器凭据切到该信封，`master.key` 不再是唯一信任根，本文「re-wrap 成本」结论需重新评估 |
| 备份密钥中的 2 个孤儿条目 | `~/.workbuddy-key-fallback/connector-keys/` 有 5 个 key，2 个无对应账号目录 | 来源（下线账号 / 历史迁移）无法从文件本身判断；若将来做「清理孤儿」，需先确认客户端不会用它们恢复 |

# ADR 0002：跨账号数据能不能搬，判据只有一条

- 状态：Accepted（2026-11-04）
- owner：顾明析（`顾明析-2`；本文结论下次需要复核时的第一联系人）
- 复核方式：本文每条结论后附证据来源（`文件:行号` 或可直接重跑的命令）。复核者若发现任一处与现场不符，**先按附录 C 重跑命令**，再改正文；不要只改措辞。
- 行号基准：**本文行号对应 commit `031d245`（release 0.1.70）**；实现变更后以符号名（函数名 / 结构体名）定位，**不追行号**。
- 语境：wb-switch 的所有跨账号能力（会话复制/同步、专家清单合并、记忆合并、导入导出，以及尚未实施的连接器同步）都在回答同一个问题：**A 账号本地这份数据，B 账号能不能用。**
- 相关实现：`crates/wb-switch-core/src/modules/{session,session_sidecar,session_backup,experts,memory,session_pinned}.rs`
- **与 ADR-0001 的关系**：判定原则（「裁决方」口径、禁写清单、highest-restriction-wins、证据强度分级）**以 `0001-cross-account-migration-invariants.md` 为准**。本文是它的应用层配套：把判据落到逐资产矩阵、把「静默失败」落成可执行检查清单，并附可复现的只读取证命令。**两文冲突时以 0001 为准，不得各说各话。**

## 0. 一句话

能不能搬，不看文件格式像不像，只取决于**接收方能否在不了解来源账号的前提下重新解释这份数据**。能重新解释 → 搬（但要收敛语义）；不能 → 只能按目标身份重算（re-wrap），或者不搬。

这是 ADR-0001「裁决方」判据的白话形态，两者等价：

| 本文说法 | ADR-0001 的说法 | 落点 |
| :--- | :--- | :--- |
| 重新解释需要「来源账号的密钥/身份」 | 存在**密码学裁决方**（AEAD authentication tag） | 连接器凭据 |
| 重新解释需要「来源账号的授权主体」 | 存在**显式裁决方**（服务端 / 对端 / 网关） | OAuth token、Authorization 头、`acpConnectionId` |
| 重新解释不需要任何外部裁决 | **本版本客户端内未发现裁决方**（须先缩域，见 0001 注 1） | 会话正文、sidecar |

## 1. 判据

对任一新资产，先回答三个问句，全部为「否」才可以原样搬：

| # | 问句 | 若为「是」的后果 |
| :-- | :--- | :--- |
| 1 | 数据里有字段是**按来源账号的密钥或身份算出来的**吗？（校验和、KDF 输出、密钥派生） | 换账号后校验必然不匹配，读失败或读到空 |
| 2 | 客户端读它时会**带当前账号的标识去校验**吗？（文件头 header、AAD、SQL `WHERE user_id = ?`） | 校验或查询直接落空，数据在盘上但读者看不见 |
| 3 | 校验失败时，读者是**显式报错**，还是**静默降级**？ | 静默降级会被当成「本来就没有」，问题拖到用户发现「东西丢了」才暴露 |

第 3 条最贵：1、2 只让搬家失败，3 让失败不可见。识别公式见附录 B。

---

## 2. 两约束

> 注：ADR-0001 用「两约束」指另外两件事（生命周期副作用禁写、覆盖率与可迁移性正交）。本文这两条是**实现方式**层面的归纳，对应 0001 附录 A 第 1-4 条；两边不冲突，只是粒度不同。

### 约束一：凭据类按身份密钥**重绑（re-wrap）**，不是拷贝

释义：解密成明文（在内存里），再用**目标账号自己的身份与密钥**重新加密、重算校验和、落在目标账号的目录下。

- 不能做的：整目录 `cp -r`、整个文件覆盖、把 `.master.key` 一起搬（密钥跟账号绑定，搬过去反而让目标账号的旧数据全解不开）。
- 必须做的：目标侧的 `salt`、`userIdCheck`、`keyCheck` 与密文**同源、同一次写入**——`key = HKDF(masterKey_target ‖ userId_target, salt_target, …)`，`userIdCheck = sha256(userId_target‖salt_target)[:16]`，`keyCheck = sha256(masterKey_target‖salt_target)[:16]`，AAD 用 `userId_target|serverKey|fieldName`。改动任一项而不同步改动其余三项，都会退化成 `InvalidTag`（ADR-0001 附录 A 第 1 条）。
- 判定依据：`salt` 是**每个账号独立随机**的，`userIdCheck = base64(sha256(userId‖salt)[:16])`。拷贝后这两个值必然对不上（实测：三账号 `.master.key` 各 32 字节，sha256 前 12 位分别为 `c8a0e7e5b5f1` / `b8bef7610636` / `f85a38827b77`，互不相同；命令见附录 C）。

### 约束二：内容类只做**语义收敛**，不搬身份

释义：内容本身（聊天记录、记忆文本、专家名字清单）跨账号是同一份东西，但里面所有「指向身份/指向旧 id」的字段必须就地重写，让它在目标账号里自洽。

本仓库已有的三个落地先例（照抄它们的取舍即可）：

| 先例 | 收敛动作 | 证据 |
| :--- | :--- | :--- |
| 会话行复制 | `user_id` → 目标 uid；`project_id` → NULL（目标账号没有这个项目，客户端会整组丢弃）；`status` 运行态 → completed（否则永远卡「生成中」）；`created_at` 保留源值 | `session.rs:630`、`session.rs:639-642`、`session.rs:645-650` |
| 专家清单合并 | 只搬「名字清单」的并集，插件本体全局共享；使用痕迹（最近使用、置顶）不搬 | `experts.rs` 模块文档第 1-19 行 |
| 个人记忆合并 | 只合并 `MEMORY.md`；`SOUL/IDENTITY/USER` 属于人格设定，**仅在目标缺失时写入**，绝不覆盖 | `memory.rs` 模块文档第 1-14 行 |

反例（不可搬）也在同一处：Claw 工作区绑定当前账号渠道，`session.rs:621-626` 直接返回错误「Claw 工作区绑定当前账号渠道，不支持复制」——**明确拒绝好过搬过去变砖**。

---

## 3. 为什么：客户端凭据栈的绑定方式（已实证）

WorkBuddy 连接器凭据的实现位于客户端包 `C:\Program Files\WorkBuddy\resources\app.asar`（符号名 `CredentialFieldCipher` / `deriveAesKey` / `buildAad` / `computeCheck` / `CredentialMasterKeyStore`；检索命令见附录 C）。算法还原如下：

```text
                       ┌─ .master.key  (32B, 每账号独立随机)
                       │
HKDF-SHA256(key = masterKey ‖ userId,
            salt = header.salt(16B, 每账号独立随机),
            info = "workbuddy-oauth-credentials-v1",
            len  = 32)  ──────────────►  AES-256-GCM 单一密钥
                                              │
           AAD = "userId|serverKey|fieldName" ┴─► 每条凭据密文 {iv, tag, ct}

header 自校验（与密钥无关 / 与密钥有关各一）：
  userIdCheck = base64( sha256( userId ‖ salt )[:16] )   ← 只绑定身份，不含密钥
  keyCheck    = base64( sha256( masterKey ‖ salt )[:16] ) ← 只绑定密钥
```

三条硬事实（均在本机三账号上复算通过）：

1. **身份进了密钥**：`userId` 是 HKDF 输入的一部分，不是元数据。同一份密文，换 userId 解不出来（合成向量实测：`InvalidTag`）。
2. **字段名进了 AAD**：`serverKey|fieldName` 参与认证，所以密文不能在不同连接器/不同字段之间挪用（合成向量实测：换 `serverKey` → `InvalidTag`）。
3. **两把锁各管一段**：`userIdCheck` 拦「这份数据不是我的」，`keyCheck` 拦「密钥不是这把」。读之前先验 header，任一不符即抛错，且错误原因区分 `userId-mismatch` / `key-mismatch`。

结论：`.credentials.v3.json` / `connector-states*.json` 属**约束一**，只能 re-wrap；`.master.key` 属账号私有材料，不参与迁移。

> 与早期口头结论的差异（以本节为准）：曾以为「主密钥走 `info="master-key"`、字段走 `info="field-key"`」，源码里不存在这两个 info 串。实际只有一个 info 常量 `workbuddy-oauth-credentials-v1`，字段隔离由 AAD 承担。

---

## 4. 新资产照此走一遍（判定流程）

```text
拿到资产
  ├─ 里面有 KDF/校验和/密钥派生字段？ ── 是 ─► 约束一：re-wrap（解密→按目标身份重加密）
  ├─ 客户端带当前身份校验/查询？      ── 是 ─┬─ 能就地改写身份字段？ ── 是 ─► 约束二：改写后再搬
  │                                          └─ 改写会破坏语义（如 OAuth 授权主体）─► 不搬，让目标自行产生
  └─ 与身份无关（纯设备级/纯内容）    ────────► 可原样搬；仍要问「搬过来对目标有意义吗」
```

---

## 附录 A：逐资产表

「最小验证」列给的是**只读**命令，全部不写数据。

| # | 资产 | 位置 | 可行性 | 理由 | 最小验证 |
| :-- | :--- | :--- | :--- | :--- | :--- |
| 1 | 会话正文 | `~/.workbuddy/projects/<工作区 slug>/<cid>.jsonl` | 可搬（需改会话 id 与内嵌路径） | 正文不含任何账号字段：全量扫描 `projects/` 下 `*.jsonl` 对 `"userId"` / `"user_id"` / `"accountId"` **零命中** | `grep -rl --include="*.jsonl" '"accountId"' ~/.workbuddy/projects \| wc -l` → **0**（`userId` / `user_id` 同为 0）。**必须带 `--include`**：不带时同一命令返回 **1**，命中的是 sidecar 里的 `tool-results/call_*.txt`（大输出渲染文件，含对话正文），不是 transcript |
| 2 | 会话首行 `session-meta` | 同上，jsonl 第一行 | 可搬 | 形如 `{"type":"session-meta","meta":{...}}`，本机 meta 仅 `codebuddy.ai/hostKind`，无身份字段 | 读最近 3 个 jsonl 的首行键名（附录 C 脚本） |
| 2b | `<cid>.meta.json` / `<cid>.file-rollback.ndjson` | 与会话正文同级 | **不搬** | `.meta.json` 内含 `acpConnectionId`（网关侧会话租约，不可迁移），而单文件是原子落盘单位、不能拆半个 → 按 ADR-0001 §三 highest-restriction-wins 整文件不搬 | `python -c "import json,glob;print(sorted(json.load(open(glob.glob('$HOME/.workbuddy/projects/*/*.meta.json')[0]))))"` |
| 3 | 工具结果 / 子 agent 记录 | `<cid>/tool-results/*`、`<cid>/subagents/*.jsonl` | 可搬，三条硬约束 | 目录必须按**目标**会话 id 建；内容里的源会话 id 必须替换（正文内嵌绝对路径指向它）；**绝不覆盖**目标已有文件 | `session_sidecar.rs:30-37`、白名单 `:63-66`；单测 `copy_sidecar_replaces_session_id_in_agent_files` |
| 4 | 会话行 `sessions` 表 | `~/.workbuddy/workbuddy.db` | 可搬（重写 `user_id`/清 `project_id`/复位 `status`） | 见 §2 约束二；`INSERT` 而非 `INSERT OR REPLACE`，新 id 撞库宁可失败也不覆盖 | `session.rs:574-670`；读接口按 `WHERE user_id = ?1` 过滤（`session.rs:444-456`） |
| 5 | 置顶 / 分组状态 | `storage/user-<uid>-personal/global/conversations.json` | 可搬，但**必须按 id 重映射** | 复制会生成新 sessionId，置顶列表不会自己跟着变 → 置顶静默丢失；按 `session_links.json` 的关联组单向追加 | `session_pinned.rs` 模块文档第 1-13 行（**本机尚未生成该文件，schema 未实测**，见待验证） |
| 6 | 滚动备份 / 事务临时目录 | `~/.workbuddy/backup/**`、`backups/session-transactions/{variant}/{opId}/`、`*.bak-<ms>`、`*.file-rollback.ndjson` | **不搬** | 备份只用于操作安全与中断恢复，「不做长期留档」；把备份当迁移素材会把半成品一起带走 | `session_backup.rs:1-9`；`ls ~/.workbuddy/backup` |
| 7 | 连接器状态 `connector-states*.json` | `~/.workbuddy/connectors/<uid>/` | 不可整体搬；可 re-wrap | header 绑定 `userId+salt+key`，AAD 绑定 `userId`；且文件自带 `accountIdentityKey = "<uid>\|\|enterprise"` 归属声明 | 见 §3 与附录 C 合成向量 |
| 8 | 连接器令牌 `.credentials.v3.json` | 同上 | **不搬**（即便技术上能 re-wrap） | OAuth token 绑定授权主体，搬到另一个账号等于冒用他人授权 → 见 `docs/connector-sync-feasibility.md` | 三账号当前**均无此文件**（`ls -a ~/.workbuddy/connectors/<uid>/`） |
| 9 | 自定义专家清单 | `~/.workbuddy/experts/custom/<uid>/experts.json` | 可搬（并集、只增不减） | 插件本体在全局 `plugins/marketplaces/` 跨账号共享，跟着账号走的只是名字清单 | `experts.rs` 文档注释；`ls ~/.workbuddy/experts/custom/*/experts.json` → 三份 |
| 10 | 个人记忆 / 人格 | `~/.workbuddy/user-<uid>-personal/{MEMORY,SOUL,IDENTITY,USER,BOOTSTRAP}.md`、`~/.workbuddy/memory/<uid>_memory.md` | 部分可搬 | 只合并 `MEMORY.md`；人格三件套仅在目标缺失时写入（合并会生成互相矛盾的人格） | `memory.rs` 文档注释；`ls ~/.workbuddy/memory` 只有 `78ae…_memory.md` |
| 11 | 设备级配置 / 资产索引 | `~/.workbuddy/app/app-config.json`、`~/.workbuddy/unified-asset/local-assets.json`、`settings.json` | 无需搬 / 只能分片搬 | 前两者不含 uid（整机一份）；`settings.json` 里 `claw.users.<uid>.channels` 是按 uid 分片，整文件覆盖会连带其它账号配置 | `python -c "import json;print(sorted(json.load(open('settings.json',encoding='utf-8'))['claw']['users']))"`（在 `~/.workbuddy` 下执行）→ 只列出 uid 键 |
| 12 | `wb-switch` 自己的关联元数据 | `~/.wb-switch/session_links.json`、`session-links/**`、`locks/**` | 不搬 | 是本工具侧的关联与锁，不是 WorkBuddy 的业务资产；跨机器搬过去基线会全部失配 | `session_link.rs` 模块文档第 1-13 行 |

---

## 附录 B：「静默失败」方法论

0.1.70 那批缺陷的共同形态只有一句话：**失败被写成了空集**。

- 客户端读子 agent 记录：文件缺失 → `loadTailSummary` 抛错 → 被 `catch` 成 `{records: [], hasMore: false}` → **子 agent 面板空白且不报错**，用户只看到「过程丢了」（`session_sidecar.rs:24-26` 明确记录了这条上游行为）。
- 迁移侧同形：拿不到源数据 → 返回空列表 → 上层判定「没有要搬的」→ 报告全绿。

**识别公式**：任何 `catch` / `else` 分支返回**空集合、空字符串、`None`、`0`** 的地方，都追加一问：

> 这个空，是**真的空**，还是**失败伪装成的空**？

三个可执行的对治手段（本仓库已在用，直接复用）：

1. **把「没有」和「读不到」拆成两个值**：`DbCopyOutcome::{Inserted, SourceRowMissing, NoSessionsTable, NoDb}`，注释写明「`No*` 与 `SourceRowMissing` 都不允许被当成成功」（`session.rs:546-553`）；侧车用 `source_present` 区分「源侧没有附属目录」（正常）与「附属目录读不了」（记 `errors`，`session_sidecar.rs:166-173`）。
2. **白名单外的条目必须计数并留名**：`ignored_entries` + `notes`，客户端将来新增 `plans/` 目录时会跳起来，而不是长期漏搬一半却永远绿灯（`session_sidecar.rs:84-89`）。
3. **失败必须进调用方的报告**：`recover_sidecar_at` 的注释原话——「结果必须由调用方带进恢复报告，否则等于把失败伪装成成功」（`session.rs:4466`，函数 `recover_sidecar_at`）。

反向自查清单（写迁移代码时逐条过）：

- [ ] 每个 `unwrap_or_default()` / `ok()` / `?` 被吞成 `None` 的地方，上层会不会判成「什么都没发生」？
- [ ] 报告的「0 项」能否区分「源侧确实没有」与「源侧读不到 / 定位不到」？
- [ ] 单个文件失败是否中断了整体，或被静默跳过？
- [ ] 复制完成后，「目标侧看不见」会不会被当成「复制成功」？（读者侧还有 `WHERE user_id = ?` 这道过滤）

---

## 附录 C：最小验证方法（合成向量，不碰真实凭据）

### C.1 只读确认三账号密钥与校验和互不相同

```bash
# 三个目录名即三个 userId
ls -d ~/.workbuddy/connectors/*/ | grep -v default
for d in ~/.workbuddy/connectors/*/; do
  printf "%s bytes=%s sha12=%s\n" "${d}" "$(wc -c < "$d/.master.key")" "$(sha256sum "$d/.master.key" | cut -c1-12)"
done
```

### C.2 复算 `userIdCheck` / `keyCheck`（全部命中即说明公式成立）

```python
# 依赖：python3（无需第三方库）
import json, hashlib, base64, os
uid_dir = os.path.expanduser("~/.workbuddy/connectors/<userId>")
j = json.load(open(f"{uid_dir}/connector-states.json", encoding="utf-8"))
e = j["encryption"]; salt = base64.b64decode(e["salt"])
mk = open(f"{uid_dir}/.master.key", "rb").read()
chk = lambda x: base64.b64encode(hashlib.sha256(x + salt).digest()[:16]).decode()
print("userIdCheck", chk(j["accountIdentityKey"].split("||")[0].encode()) == e["userIdCheck"])
print("keyCheck   ", chk(mk) == e["keyCheck"])
```

### C.3 复现「拷贝必炸 / re-wrap 可用」（合成数据，不读真实凭据）

```python
# 依赖：pip install cryptography
import os, hashlib, base64
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

INFO = b"workbuddy-oauth-credentials-v1"
chk   = lambda x, s: base64.b64encode(hashlib.sha256(x + s).digest()[:16]).decode()
key   = lambda mk, uid, s: HKDF(algorithm=hashes.SHA256(), length=32, salt=s, info=INFO).derive(mk + uid.encode())
aad   = lambda uid, sk, f: f"{uid}|{sk}|{f}".encode()
def enc(k, uid, sk, f, pt):
    n = os.urandom(12); ct = AESGCM(k).encrypt(n, pt, aad(uid, sk, f)); return n, ct[-16:], ct[:-16]
def dec(k, uid, sk, f, n, tag, ct): return AESGCM(k).decrypt(n, ct + tag, aad(uid, sk, f))

A, B   = "user-a", "user-b"
kA, kB = os.urandom(32), os.urandom(32)
sA     = os.urandom(16)
env    = enc(key(kA, A, sA), A, "qq-mail", "accessToken", b"synthetic")
print("源账号自解           ", dec(key(kA, A, sA), A, "qq-mail", "accessToken", *env))
print("把 header 拷给 B：userIdCheck 对不上", chk(B.encode(), sA) == chk(A.encode(), sA))
try: dec(key(kB, B, sA), B, "qq-mail", "accessToken", *env)
except Exception as e: print("用 B 身份解 A 的密文  ", type(e).__name__)      # 期望 InvalidTag
sB = os.urandom(16)
re = enc(key(kB, B, sB), B, "qq-mail", "accessToken", dec(key(kA, A, sA), A, "qq-mail", "accessToken", *env))
print("re-wrap 后 B 能解     ", dec(key(kB, B, sB), B, "qq-mail", "accessToken", *re))
```

### C.4 定位客户端凭据实现（版本会变，用符号名而不用偏移）

```bash
python - <<'PY'
import re
data = open(r"C:\Program Files\WorkBuddy\resources\app.asar", "rb").read()
for sym in (b"CredentialFieldCipher", b"deriveAesKey", b"buildAad", b"computeCheck",
            b"workbuddy-oauth-credentials-v1", b"CredentialMasterKeyStore", b"hashUserId"):
    print(sym.decode(), [m.start() for m in re.finditer(re.escape(sym), data)][:3])
PY
```

---

## 待验证（缺什么证据，写清楚不猜）

| 项 | 现状证据 | 缺什么 |
| :--- | :--- | :--- |
| `wb.conversations.fork` 的**服务端**归属语义 | 客户端只是 `POST {api}/{conversationId}/fork` 并返回新 id（`app.asar` 内 `forkConversation`），本地不做任何所有权改写 | 服务端是否沿用源会话的 owner。另：本轮未能在客户端包与本地数据中找到 `owningAccount` 字段，也未找到 `wb:conversation:fork` 这一写法（真实标识为 `wb.conversations.fork`） |
| 会话索引是否落本地 sqlite | 客户端包内含 `CREATE TABLE cloud_conversations (... user_id TEXT NOT NULL ...)` 字面量 | 本机 185 个 `*.db` 中均未找到该表实例，实际落盘位置待确认（可能在 IndexedDB / 运行期创建） |
| re-wrap 后的写回路径与备份 | 客户端 `CredentialMasterKeyStore` 主副本 `connectors/<uid>/.master.key` + 备份 `~/.workbuddy-key-fallback/connector-keys/<sha256(uid)[:32]>.key` | 写回是否会被客户端下次启动的 header 校验拒绝；备份 key 目录按 uid 哈希命名，re-wrap 后是否需要同步更新 |
| 备份密钥目录中的孤儿条目 | 现有 5 个 `*.key`，其中 3 个与当前三账号一一对应且与主副本字节相同，另 2 个（`5e0d…`、`f696…`）无对应账号目录 | 孤儿来源（已下线账号 / 历史迁移）无法从文件本身判断 |
| 新版 at-rest 信封是否换根 | 客户端包内存在 `WBEF1`（sym-v1）/ `asym-v1` / `developerPublicKey` / `fileKeyId` 的整文件信封实现，`WBEA1` 是其中的 legacy 分支 | 本机三账号仍停留在旧路径；未来版本若切到非对称信封，`master.key` 不再是唯一信任根，本 ADR 的「约束一」需重新取证 |
| 置顶状态文件的实际 schema | 只有模块文档描述的形态 `{"pinned":[{"id","groupKey"}]}`（`session_pinned.rs:1-13`） | 本机 `storage/user-<uid>-personal/global/conversations.json` 尚未生成，无法实测字段与边界（与 ADR-0001「残余风险」同一项） |
| 未闭合前提：连接器同步**无法用真实密文做端到端回归** | 三账号 `connectors` 均为空（`connectors:{}` / `enabled:[]` / `everConnected:[]`）、`.credentials.v3.json` 不存在，唯一可用的验证是合成向量（附录 C.3） | 缺一份真实连接器密文样本。**该前提在实施阶段才生效，本轮不实施故不阻塞**；一旦决定实施，第一步必须先补真实样本回归，否则「re-wrap 可用」只有合成证据 |
| `experts.json` 之外的专家资产 | 只搬名字清单的结论来自 `experts.rs` 文档注释，且本机三账号各有 `experts.json` | 「最近使用 / 置顶」这两类痕迹文件的确切路径与字段未逐一核对（模块注释点名 `storage/user-<uid>-personal/global/expert.json`、`scoped/<hash>/expert-picker.json`，本机未验证其存在） |

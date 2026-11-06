# 整包备份 / 迁移（archive）

换机器、重装、给同事配环境时，账号、设置、日志、记忆、会话关联散在 `~/.wb-switch` 与
`~/.workbuddy` 两棵树下，逐文件拷容易漏。这里把它们打成一个 zip，导入时**按组勾选**，
并由 core 统一做「先备份、失败回滚」。

## 接线图

```
Tauri commands.rs::archive_*        server api.rs GET/POST /api/archive/*      CLI main.rs::archive
                │                                   │                                  │
                └───────────────┬───────────────────┴──────────────────────────────────┘
                                ▼
              crates/wb-switch-core/src/modules/archive
                                │
        ┌───────────────┬───────┴────────┬──────────────────┐
        ▼               ▼                ▼                  ▼
    mod.rs          group.rs         manifest.rs          zip.rs
  编排/备份/回滚   分组+白名单       archive-manifest    压缩/校验/限额
```

宿主只做「取路径 / base64 过线 / 转 JSON」，任何一行业务逻辑都不应落在宿主里；
新增一类数据只改 `group.rs`（加 struct + 注册一行），三个宿主零改动。

## 分组清单

| id | 收什么 | 敏感 |
|---|---|---|
| `accounts` | `accounts.json`（含 access_token） | 是 |
| `settings` | 签到 / 旅行 / 限流 / GitHub 更新 / 自动轮换配置 | 否 |
| `logs` | 签到、每日对话、自动轮换日志 + `notifications.json` | 否 |
| `stats` | `credit_usage_snapshots.json`、`official_usage_cache.json` | 否 |
| `memory` | **两个档位根**（`.workbuddy` / `.workbuddy-ai`）下 `user-*-personal/` 的 `*.md` 与 `memory/**/*.md` | 否 |
| `experts` | 两个档位根下 `experts/custom/<uid>/experts.json`（只带名字，不带 GB 级插件本体） | 否 |
| `links` | 三份 `*session_links.json` + `session-links/baselines/`（不含中断操作/预览） | 否 |

`groups()` 返回的 `files` / `bytes` 是**当前磁盘现状**，前端据此决定勾选面板；
`inspect()` 里的数字则来自清单，是**导出那一刻**的值。

两个易踩的数据范围：

- **记忆/专家团跨档位**：只玩国际版的用户数据全在 `.workbuddy-ai`，因此 zip 条目名带命名空间
  前缀（`memory/workbuddy/...`、`memory/workbuddy-ai/...`，专家团同理）。去掉前缀会让两个根的
  同名账号目录在 `BTreeMap` 里互相覆盖，导入侧也会因「重复的目标文件」直接拒绝。
- **会话关联只带主表与基线**：`operations/*.json` 的 `backup` 字段是本机绝对路径，换机后
  指向不存在的目录；未完成的 sync 操作会因此永久卡在「需要恢复」。预览与 backup-lifecycle
  同理属于过程态。基线本身只有行摘要，需配合会话正文才有意义，单独勾选请如实告知用户。

## 导入流程（关键顺序）

```
read_archive（校验每个条目名 + 限额）
      → 读 manifest（版本/分组）
      → plan_targets（白名单映射 + 重复目标检测，此时零写盘）
      → create_backup_dir + backup_targets（全量备份，失败即中止）
      → 逐组 apply
             ├─ 成功：返回 backupDir 与账号 added/updated
             └─ 失败：rollback（有备份的拷回，新建的删除）
                        ├─ 回滚成功：错误文案带「已回滚到导入前状态」
                        └─ 回滚失败：错误文案带备份绝对路径，提示手动恢复
```

账号组不走文件覆盖：`apply` 调用 `account::with_accounts_write`，按 uid 合并
（同 uid 覆盖、新 uid 追加），避免把导入的旧快照盖掉并发刷新。

## 为什么镜像条目只按白名单落盘

zip 里的条目名是**不可信输入**。直接 `join` 条目名会踩两类坑：`../` 逃出目标目录、
`C:\x` / 反斜杠在 Windows 上变成绝对路径。因此：

1. `zip::validate_entry_name` 统一拒绝绝对路径、盘符、`..`、`.`、空段、反斜杠、控制字符；
2. 条目名只用于**匹配** `group::relative_target` 的静态白名单（固定文件名 / `session-links/`
   下每段 `[A-Za-z0-9._-]` / `user-*-personal` 下的 Markdown）；
3. 真实写盘路径 = `group_root(env, id)` + 白名单返回的相对路径。

白名单还挡住「归档里混进 `settings/evil.exe`」这类越权条目：备份阶段就会报错退出，
此时磁盘一个字节都没改。

## 五条避坑

1. **别用环境变量做测试隔离**：`WB_SWITCH_HOME` 是进程级全局状态，会和并行测试互相踩。
   core 内部用注入的 `ArchiveEnv { store, home }`，测试各自落在临时目录。
2. **Windows 的 `rename` 不覆盖**：导出先删旧包再换名；`atomic_write` 同理依赖 tmp+rename。
3. **备份必须全量先做**：备份与写盘交错会出现「备份到一半改了文件」，回滚清单也会失真。
4. **限额在导出侧也要拦**：若只在解压时拦，就可能产出自己都读不回的包（64 MB / 5000 条 / 16 MB）。
   webui 的 body 上限必须 ≥「64 MB × 4/3 + JSON 余量」（现为 96 MB），否则大包会先被 413。
5. **`memory` 只收 Markdown、且两个根都要收**：`workspace-state.json` 是客户端缓存，
   `*.md.bak-*` 是历史备份，一起迁移只会把旧内容带回来；而漏掉 `.workbuddy-ai` 会让
   国际版用户导出空组，换机后记忆静默丢失。

## 文件索引

| 位置 | 作用 |
|---|---|
| `crates/wb-switch-core/src/modules/archive/mod.rs` | `groups/export/inspect/import`、备份、回滚 |
| `crates/wb-switch-core/src/modules/archive/group.rs` | `DataGroupSpec`、6 个组、白名单 |
| `crates/wb-switch-core/src/modules/archive/manifest.rs` | 清单结构与版本校验 |
| `crates/wb-switch-core/src/modules/archive/zip.rs` | 条目名校验、限额、zip 读写 |
| `src-tauri/src/commands.rs` | 桌面端 `archive_*` 命令 |
| `crates/wb-switch-server/src/api.rs` | webui 路由 `/api/archive/*`（base64 过线） |
| `crates/wb-switch-server/src/main.rs` | CLI `archive list/export/import` |

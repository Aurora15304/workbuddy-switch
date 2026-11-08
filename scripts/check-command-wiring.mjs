#!/usr/bin/env node
// 接线自检：新增命令/路由时最容易漏的那一层，靠这个脚本在门禁里报错。
//
// 覆盖两类「编译能过、点下去必炸」的漏接线：
//   1) `#[tauri::command]` 定义了但没进 `lib.rs` 的 `generate_handler![]`
//      → 桌面端报 `Command xxx not found`（0.1.82 的 deleted_sync_preview 就是这么漏的）。
//   2) `src/lib/api.ts` 的 ROUTES 里有路径，但 `crates/wb-switch-server/src/api.rs`
//      没注册同路径 → webui 端 404。
//
// 用法：node scripts/check-command-wiring.mjs
// 退出码非 0 即接线不完整；输出逐条列出漏项与修法。
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const REPO = process.cwd();
const problems = [];

function walk(dir, out = []) {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) walk(path, out);
    else if (name.endsWith(".rs")) out.push(path);
  }
  return out;
}

// ---- 1) Tauri 命令：定义 ⊆ 注册 ----
const registered = new Set();
const libRs = readFileSync(join(REPO, "src-tauri/src/lib.rs"), "utf8");
const handlerStart = libRs.indexOf("generate_handler![");
const handlerBlock =
  handlerStart < 0
    ? ""
    : libRs.slice(handlerStart, libRs.indexOf("])", handlerStart));
for (const match of handlerBlock.matchAll(/([a-z_][a-z0-9_]*)::([a-z0-9_]+)/g)) {
  registered.add(`${match[1]}::${match[2]}`);
}
if (handlerStart < 0) problems.push("src-tauri/src/lib.rs 找不到 generate_handler![ … ]");

const defined = [];
for (const file of walk(join(REPO, "src-tauri/src"))) {
  const moduleName = file.split(/[\\/]src[\\/]/).pop().replace(/\.rs$/, "").replace(/[\\/]/g, "::");
  const source = readFileSync(file, "utf8");
  for (const match of source.matchAll(/#\[tauri::command[^\]]*\]\s*(?:pub\s+)?(?:async\s+)?fn\s+([a-z0-9_]+)/g)) {
    defined.push({ key: `${moduleName}::${match[1]}`, name: match[1], file });
  }
}
for (const { key, name, file } of defined) {
  if (!registered.has(key)) {
    problems.push(
      `未注册的 Tauri 命令：${key}（定义在 ${file.replace(REPO + "\\", "").replace(REPO + "/", "")}）` +
        `\n    → 修法：加到 src-tauri/src/lib.rs 的 generate_handler![ … ] 里，否则桌面端 invoke 报 "Command ${name} not found"`,
    );
  }
}

// ---- 2) webui 路由：api.ts 的 ROUTES ⊆ server 的 .route(...) ----
const apiTs = readFileSync(join(REPO, "src/lib/api.ts"), "utf8");
const apiPaths = [...apiTs.matchAll(/path:\s*"(\/api\/[^"]+)"/g)].map((m) => m[1]);
const serverRs = readFileSync(join(REPO, "crates/wb-switch-server/src/api.rs"), "utf8");
// `.route(` 与路径字面量之间允许换行：rustfmt 会把长行折成两行。
const serverPaths = new Set([...serverRs.matchAll(/\.route\(\s*"([^"]+)"/g)].map((m) => m[1]));
for (const path of apiPaths) {
  if (!serverPaths.has(path)) {
    problems.push(
      `未注册的 webui 路由：${path}（src/lib/api.ts 的 ROUTES 有，crates/wb-switch-server/src/api.rs 没有）` +
        `\n    → 修法：在 api.rs 的路由表里加 .route("${path}", …) 并实现对应 handler`,
    );
  }
}

if (problems.length > 0) {
  console.error("[error] 接线自检未通过：");
  for (const problem of problems) console.error(`  - ${problem}`);
  process.exit(1);
}

console.log(
  `[ok] 接线自检通过：Tauri 命令 ${defined.length} 个全部已注册，webui 路由 ${apiPaths.length} 条全部有服务端实现`,
);

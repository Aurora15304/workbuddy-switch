/**
 * 最近一次镜像备份的路径备忘。
 *
 * 为什么需要单独持久化：镜像成功后切号会重启 WorkBuddy，弹窗随即关闭；如果只把路径放在
 * 组件 state 里，一关就丢，用户再也找不到「还原」入口——等于"承诺可还原"只是空话。
 *
 * 只记路径字符串，不记任何账号 / 会话内容。读回后仍需用户主动点「还原」；
 * 备份若已被清理，后端会返回明确错误，界面照实提示，绝不伪装成功。
 *
 * **已知边界（未解决，不伪装）**：本机制只闭合「成功返回 report」这条路径。
 * 若镜像已软删、但在返回 report 前进程崩溃（窗口极窄），备份目录是 `pending` 状态，
 * 前端拿不到路径，也就不会出现在这里。后端的 `scan_mirror_backups()` 已能列出这类
 * pending 备份，但**尚未接入 UI**；接线点就在本模块旁边，见 0.1.79 spec 的 Out of Scope。
 *
 * node 环境（测试）没有 window 时全部降级为「没有备份」，不会让测试崩掉。
 */

const STORAGE_KEY = "wb-switch-mirror-last-backup";

/** 记住最近一次成功镜像的备份目录绝对路径。写不进去只是下次看不到入口，不报错。 */
export function rememberMirrorBackup(path: string): void {
  const value = path.trim();
  if (!value) return;
  try {
    window.localStorage.setItem(STORAGE_KEY, value);
  } catch {
    /* 忽略：持久化失败不影响本次操作 */
  }
}

/** 读回最近一次备份路径；没有、为空或存储不可用时一律返回 null。 */
export function lastMirrorBackup(): string | null {
  try {
    const value = window.localStorage.getItem(STORAGE_KEY);
    return value && value.trim() ? value : null;
  } catch {
    return null;
  }
}

/** 还原成功或用户明确丢弃后清除备忘，避免长期挂着一条失效路径。 */
export function forgetMirrorBackup(): void {
  try {
    window.localStorage.removeItem(STORAGE_KEY);
  } catch {
    /* 忽略 */
  }
}

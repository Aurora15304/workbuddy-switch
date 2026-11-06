import type { ArchiveGroupInfo } from "./types";

/**
 * 数据迁移卡片的纯逻辑：体积格式化、勾选计算、文件名、base64 转换。
 *
 * 放在 lib 里是因为这些都不碰 React 与 Tauri，桌面 / webui 两条导出导入路径
 * 共用同一份规则，也便于单独跑 vitest。
 */

const KB = 1024;
const MB = KB * 1024;

/** 人类可读体积：1.2 MB / 340 KB / 512 B。 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  if (bytes < KB) return `${Math.round(bytes)} B`;
  if (bytes < MB) return `${trimZero(bytes / KB)} KB`;
  return `${trimZero(bytes / MB)} MB`;
}

function trimZero(value: number): string {
  return value.toFixed(1).replace(/\.0$/, "");
}

/** 有内容的组才值得勾选：空组导不进任何文件，只是噪音。 */
export function selectableGroups(groups: ArchiveGroupInfo[]): ArchiveGroupInfo[] {
  return groups.filter((group) => group.files > 0);
}

/** 默认勾选：全部有内容的组（整包备份的语义）。 */
export function defaultArchiveSelection(groups: ArchiveGroupInfo[]): string[] {
  return selectableGroups(groups).map((group) => group.id);
}

/** 是否已勾满全部有内容的组（「全选 / 全不选」按钮的文案与行为都看它）。 */
export function isAllArchiveSelected(
  groups: ArchiveGroupInfo[],
  selected: readonly string[],
): boolean {
  const all = defaultArchiveSelection(groups);
  return all.length > 0 && all.every((id) => selected.includes(id));
}

/** 全选 / 全不选：已全选则清空，否则勾上全部有内容的组。 */
export function toggleAllArchiveSelection(
  groups: ArchiveGroupInfo[],
  selected: readonly string[],
): string[] {
  return isAllArchiveSelected(groups, selected) ? [] : defaultArchiveSelection(groups);
}

/** 勾选组的文件数与总体积（导出按钮旁的总量提示）。 */
export function summarizeSelection(
  groups: ArchiveGroupInfo[],
  selected: readonly string[],
): { files: number; bytes: number } {
  return groups.reduce(
    (sum, group) => {
      if (!selected.includes(group.id)) return sum;
      return { files: sum.files + group.files, bytes: sum.bytes + group.bytes };
    },
    { files: 0, bytes: 0 },
  );
}

/** 默认导出文件名：wb-switch-backup-YYYYMMDD.zip。 */
export function archiveBackupFileName(date: Date = new Date()): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `wb-switch-backup-${date.getFullYear()}${pad(date.getMonth() + 1)}${pad(date.getDate())}.zip`;
}

/**
 * core 的整包上限：解压后内容总量 64 MiB，与 `archive_groups.bytes` 同口径。
 *
 * 导出入两用同一个数：勾选合计超它 core 会拒绝导出；zip 文件本身超它，
 * 解压总量必然也超（deflate 对不可压缩数据只膨胀不缩小），导入同样会被拒。
 */
export const ARCHIVE_MAX_RAW_BYTES = 64 * 1024 * 1024;

/**
 * 导出预检的余量（64 KiB）：core 的 total 从 `archive-manifest.json` 开始累加，
 * 所以真实比较值 ≈ Σ(groups.bytes) + manifest。留这点余量才不会出现
 * 「按钮能点 → 点了被 400」，同时远小于正常用户的数据量，不会误伤。
 */
export const ARCHIVE_EXPORT_MARGIN_BYTES = 64 * 1024;

/** 前端导出预检实际使用的门槛：64 MiB 减去 manifest 余量。 */
export const ARCHIVE_EXPORT_LIMIT_BYTES = ARCHIVE_MAX_RAW_BYTES - ARCHIVE_EXPORT_MARGIN_BYTES;

/** 导入预检的软提示门槛：离 64 MiB 上限只剩 8 MiB 就提醒一句。 */
export const ARCHIVE_WARN_ZIP_BYTES = 56 * 1024 * 1024;

/** 勾选内容是否已超出 core 的导出上限（超了后端必然拒绝，前端提前拦）。 */
export function isExportOverLimit(
  groups: ArchiveGroupInfo[],
  selected: readonly string[],
): boolean {
  return summarizeSelection(groups, selected).bytes > ARCHIVE_EXPORT_LIMIT_BYTES;
}

/**
 * 从体积最大的勾选组开始撤，直到合计不超过上限；返回剩下的勾选与被撤掉的组 id。
 *
 * 只做「用户点一下才发生」的显式操作：默认勾选保持全量，避免让人误以为备份是完整的。
 */
export function trimSelectionToLimit(
  groups: ArchiveGroupInfo[],
  selected: readonly string[],
  maxBytes: number = ARCHIVE_EXPORT_LIMIT_BYTES,
): { selected: string[]; removed: string[] } {
  const picked = groups.filter((group) => selected.includes(group.id));
  let total = picked.reduce((sum, group) => sum + group.bytes, 0);
  const removed: string[] = [];
  for (const group of [...picked].sort((a, b) => b.bytes - a.bytes)) {
    if (total <= maxBytes) break;
    removed.push(group.id);
    total -= group.bytes;
  }
  return {
    selected: selected.filter((id) => !removed.includes(id)),
    removed,
  };
}

/** 导入时的本地体积预检：ok 放行 / warn 放行但提醒 / block 直接拦（不惊动后端）。 */
export function checkArchiveZipSize(bytes: number): "ok" | "warn" | "block" {
  if (bytes > ARCHIVE_MAX_RAW_BYTES) return "block";
  if (bytes > ARCHIVE_WARN_ZIP_BYTES) return "warn";
  return "ok";
}

/** base64 → 字节（zip 是二进制，不能按文本解）。 */
export function base64ToBytes(base64: string): Uint8Array {
  const binary = atob(base64.replace(/\s+/g, ""));
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i) & 0xff;
  return bytes;
}

/** 字节 → base64（webui 选中的文件要整段发给后端）。 */
export function bytesToBase64(bytes: Uint8Array): string {
  // 分块拼接：一次展开几十万个参数会爆调用栈。
  const chunkSize = 0x8000;
  let binary = "";
  for (let i = 0; i < bytes.length; i += chunkSize) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunkSize));
  }
  return btoa(binary);
}

/** 导出时间（后端给的是 epoch **秒**，不是毫秒），格式化到分钟。 */
export function formatArchiveTime(epochSeconds: number): string {
  if (!Number.isFinite(epochSeconds) || epochSeconds <= 0) return "未知";
  try {
    return new Date(epochSeconds * 1000).toLocaleString("zh-CN", {
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
    });
  } catch {
    return "未知";
  }
}

import { describe, expect, it } from "vitest";

import {
  ARCHIVE_EXPORT_LIMIT_BYTES,
  ARCHIVE_MAX_RAW_BYTES,
  ARCHIVE_WARN_ZIP_BYTES,
  archiveBackupFileName,
  base64ToBytes,
  bytesToBase64,
  checkArchiveZipSize,
  defaultArchiveSelection,
  formatArchiveTime,
  formatBytes,
  isAllArchiveSelected,
  isExportOverLimit,
  isGroupTooLarge,
  summarizeSelection,
  toggleAllArchiveSelection,
  trimSelectionToLimit,
} from "./archive";
import type { ArchiveGroupInfo } from "./types";

function group(overrides: Partial<ArchiveGroupInfo> & { id: string }): ArchiveGroupInfo {
  return {
    title: overrides.id,
    description: "",
    sensitive: false,
    files: 1,
    bytes: 100,
    largestFileBytes: 0,
    ...overrides,
  };
}

const GROUPS: ArchiveGroupInfo[] = [
  group({ id: "accounts", sensitive: true, files: 2, bytes: 2048 }),
  group({ id: "settings", files: 5, bytes: 512 }),
  group({ id: "logs", files: 0, bytes: 0 }),
];

describe("formatBytes", () => {
  it("按 B / KB / MB 分级并保留一位小数", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1024)).toBe("1 KB");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(1024 * 1024)).toBe("1 MB");
    expect(formatBytes(1024 * 1024 * 5.25)).toBe("5.3 MB");
  });

  it("非法值按 0 处理", () => {
    expect(formatBytes(-1)).toBe("0 B");
    expect(formatBytes(Number.NaN)).toBe("0 B");
  });
});

describe("勾选计算", () => {
  it("默认只勾有内容的组", () => {
    expect(defaultArchiveSelection(GROUPS)).toEqual(["accounts", "settings"]);
  });

  it("全选 / 全不选互为反操作，空组不进结果", () => {
    expect(toggleAllArchiveSelection(GROUPS, [])).toEqual(["accounts", "settings"]);
    expect(toggleAllArchiveSelection(GROUPS, ["accounts"])).toEqual(["accounts", "settings"]);
    expect(toggleAllArchiveSelection(GROUPS, ["accounts", "settings"])).toEqual([]);
  });

  it("全选判定不看空组", () => {
    expect(isAllArchiveSelected(GROUPS, ["accounts", "settings"])).toBe(true);
    expect(isAllArchiveSelected(GROUPS, ["accounts"])).toBe(false);
    // 全是空组时不存在「全选」状态，否则按钮会显示成全不选。
    expect(isAllArchiveSelected([group({ id: "logs", files: 0 })], [])).toBe(false);
  });

  it("汇总只统计勾选组", () => {
    expect(summarizeSelection(GROUPS, ["settings"])).toEqual({ files: 5, bytes: 512 });
    expect(summarizeSelection(GROUPS, ["accounts", "settings"])).toEqual({
      files: 7,
      bytes: 2560,
    });
    expect(summarizeSelection(GROUPS, [])).toEqual({ files: 0, bytes: 0 });
  });
});

describe("archiveBackupFileName", () => {
  it("按 YYYYMMDD 生成 zip 名", () => {
    expect(archiveBackupFileName(new Date(2026, 10, 6))).toBe("wb-switch-backup-20261106.zip");
  });
});

describe("体积上限预检", () => {
  const MiB = 1024 * 1024;

  it("导出预检扣掉 manifest 余量：贴着 64 MiB 也会提前拦", () => {
    const justUnder = [group({ id: "accounts", bytes: ARCHIVE_EXPORT_LIMIT_BYTES })];
    const intoMargin = [group({ id: "accounts", bytes: ARCHIVE_EXPORT_LIMIT_BYTES + 1 })];
    expect(isExportOverLimit(justUnder, ["accounts"])).toBe(false);
    expect(isExportOverLimit(intoMargin, ["accounts"])).toBe(true);
    // 「UI 放行、core 加上 manifest 后超限」的窗口必须被提前拦掉。
    const nearLimit = [group({ id: "accounts", bytes: ARCHIVE_MAX_RAW_BYTES - 1024 })];
    expect(isExportOverLimit(nearLimit, ["accounts"])).toBe(true);
    expect(isExportOverLimit(intoMargin, [])).toBe(false);
  });

  it("导入预检按 64 MiB 硬拦、56 MiB 起提醒", () => {
    expect(checkArchiveZipSize(0)).toBe("ok");
    expect(checkArchiveZipSize(ARCHIVE_WARN_ZIP_BYTES)).toBe("ok");
    expect(checkArchiveZipSize(ARCHIVE_WARN_ZIP_BYTES + 1)).toBe("warn");
    // 64 MiB 正好等于 core 上限，仍放行（超了才必然被拒）。
    expect(checkArchiveZipSize(ARCHIVE_MAX_RAW_BYTES)).toBe("warn");
    expect(checkArchiveZipSize(ARCHIVE_MAX_RAW_BYTES + 1)).toBe("block");
    // 37 MB 的真实整包必须一路放行。
    expect(checkArchiveZipSize(37 * MiB)).toBe("ok");
    expect(checkArchiveZipSize(65 * MiB)).toBe("block");
  });

  it("超限时从最大的组开始撤，撤够就停", () => {
    const big = group({ id: "links", bytes: 40 * MiB });
    const mid = group({ id: "memory", bytes: 20 * MiB });
    const small = group({ id: "settings", bytes: 6 * MiB });
    const groups = [small, big, mid];
    // 66 MiB 超限：撤掉最大的 links 后 26 MiB，够用即停，不会继续撤 memory。
    expect(trimSelectionToLimit(groups, ["settings", "links", "memory"])).toEqual({
      selected: ["settings", "memory"],
      removed: ["links"],
    });
  });

  it("未超限 / 空勾选时不动勾选", () => {
    const groups = [group({ id: "settings", bytes: 1024 })];
    expect(trimSelectionToLimit(groups, ["settings"])).toEqual({
      selected: ["settings"],
      removed: [],
    });
    expect(trimSelectionToLimit(groups, [])).toEqual({ selected: [], removed: [] });
  });

  it("逐个撤到能导出为止，保序且只撤勾选内的组", () => {
    const groups = [
      group({ id: "a", bytes: 40 * MiB }),
      group({ id: "b", bytes: 30 * MiB }),
      group({ id: "c", bytes: 5 * MiB }),
      group({ id: "d", bytes: 100 * MiB }),
    ];
    // 上限压到 10 MiB，逼它连撤两个：a(40) → b(30) → 剩 5 达标
    const { selected, removed } = trimSelectionToLimit(groups, ["a", "b", "c"], 10 * MiB);
    expect(removed).toEqual(["a", "b"]);
    expect(selected).toEqual(["c"]);
    // 未勾选的 d 再大也不参与，也不出现在 removed 里。
    expect(removed).not.toContain("d");
  });
});

describe("base64 往返", () => {
  it("二进制字节不被文本化破坏", () => {
    const bytes = new Uint8Array(256);
    for (let i = 0; i < bytes.length; i += 1) bytes[i] = i;
    const roundTrip = base64ToBytes(bytesToBase64(bytes));
    expect(Array.from(roundTrip)).toEqual(Array.from(bytes));
  });

  it("超过单块大小的数据也能编码（不爆调用栈）", () => {
    const bytes = new Uint8Array(200_000);
    bytes[0] = 0x50;
    bytes[bytes.length - 1] = 0x4b;
    const decoded = base64ToBytes(bytesToBase64(bytes));
    expect(decoded.length).toBe(bytes.length);
    expect(decoded[0]).toBe(0x50);
    expect(decoded[decoded.length - 1]).toBe(0x4b);
  });

  it("忽略换行等空白，可直接解后端多行 base64", () => {
    const text = bytesToBase64(new Uint8Array([1, 2, 3, 4, 5, 6]));
    const wrapped = `${text.slice(0, 4)}\n${text.slice(4)}`;
    expect(Array.from(base64ToBytes(wrapped))).toEqual([1, 2, 3, 4, 5, 6]);
  });
});

describe("单文件上限", () => {
  it("组内最大文件超过 16 MiB 时不可勾选", () => {
    const huge = group({
      id: "logs",
      files: 1,
      bytes: 20 * 1024 * 1024,
      largestFileBytes: 20 * 1024 * 1024,
    });
    const normal = group({ id: "settings", files: 1, bytes: 1024, largestFileBytes: 1024 });
    expect(isGroupTooLarge(huge)).toBe(true);
    expect(isGroupTooLarge(normal)).toBe(false);
    // 默认勾选要跳过必然失败的组，否则用户点导出必定吃 400。
    expect(defaultArchiveSelection([huge, normal])).toEqual(["settings"]);
  });
});

describe("formatArchiveTime", () => {
  it("缺失时间戳给「未知」", () => {
    expect(formatArchiveTime(0)).toBe("未知");
    expect(formatArchiveTime(Number.NaN)).toBe("未知");
  });

  it("按 epoch 秒解析（后端 manifest 用的是秒，不是毫秒）", () => {
    const seconds = Math.floor(new Date(2026, 10, 6, 9, 5).getTime() / 1000);
    const text = formatArchiveTime(seconds);
    expect(text).toContain("2026");
    // 毫秒误当秒会变成 1970 年。
    expect(text).not.toContain("1970");
  });
});

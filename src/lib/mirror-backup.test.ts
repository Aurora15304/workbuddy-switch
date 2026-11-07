import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { forgetMirrorBackup, lastMirrorBackup, rememberMirrorBackup } from "@/lib/mirror-backup";

/** 最小内存版 Storage：node 环境没有 localStorage，用它验证读写往返。 */
function memoryStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (key: string) => map.get(key) ?? null,
    key: (index: number) => [...map.keys()][index] ?? null,
    removeItem: (key: string) => {
      map.delete(key);
    },
    setItem: (key: string, value: string) => {
      map.set(key, value);
    },
  } as Storage;
}

describe("镜像备份路径备忘", () => {
  beforeAll(() => {
    (globalThis as unknown as { window: unknown }).window = { localStorage: memoryStorage() };
  });

  afterAll(() => {
    delete (globalThis as unknown as { window?: unknown }).window;
  });

  it("记住后能读回，且首尾空白被去掉", () => {
    rememberMirrorBackup("  C:/data/transactions/cn/op-1  ");
    expect(lastMirrorBackup()).toBe("C:/data/transactions/cn/op-1");
  });

  it("空路径不覆盖已有备忘", () => {
    rememberMirrorBackup("C:/keep/me");
    rememberMirrorBackup("   ");
    expect(lastMirrorBackup()).toBe("C:/keep/me");
  });

  it("清除后读回 null", () => {
    rememberMirrorBackup("C:/data/op-2");
    forgetMirrorBackup();
    expect(lastMirrorBackup()).toBeNull();
  });

  it("没有 localStorage 时全部降级，不抛错", () => {
    delete (globalThis as unknown as { window?: unknown }).window;
    expect(() => rememberMirrorBackup("C:/x")).not.toThrow();
    expect(lastMirrorBackup()).toBeNull();
    expect(() => forgetMirrorBackup()).not.toThrow();
    (globalThis as unknown as { window: unknown }).window = { localStorage: memoryStorage() };
  });
});

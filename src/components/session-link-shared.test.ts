import { describe, expect, it } from "vitest";

import {
  archiveOnly,
  buildSelections,
  isActionable,
  isDirectlySyncable,
  partitionDeleted,
  primaryMode,
  shouldDefaultSelect,
  summarySentence,
  unarchiveOnly,
} from "./session-link-shared";
import type { SessionLinkPreviewGroup, SessionSyncMode } from "@/lib/types";

/**
 * 「关联会话」共享勾选逻辑的纯函数单测（#143）。
 *
 * 只覆盖不依赖 DOM 的纯逻辑：可勾选判定、提交组装、模式优先级与摘要文案。
 */

function group(
  overrides: Partial<SessionLinkPreviewGroup> & {
    availableModes?: SessionSyncMode[];
  } = {},
): SessionLinkPreviewGroup {
  return {
    groupId: "g-1",
    title: "标题一",
    cwd: "/ws/a",
    verdict: "identical",
    extraA: 0,
    extraB: 0,
    common: 2,
    defaultChecked: false,
    availableModes: [],
    reason: "无需同步",
    recordCount: { source: 2, target: 2, baseline: 2 },
    source: null,
    target: null,
    ...overrides,
  };
}

describe("isActionable", () => {
  it("有正文模式且有凭据时可勾选", () => {
    expect(isActionable(group({ availableModes: ["fastForward"], previewToken: "t" }))).toBe(true);
  });

  it("只归档项（模式为空 + archiveAction）有凭据时可勾选", () => {
    expect(
      isActionable(group({ archiveAction: "statusOnly", previewToken: "t" })),
    ).toBe(true);
  });

  it("只归档项没有凭据时不可勾选", () => {
    expect(isActionable(group({ archiveAction: "statusOnly" }))).toBe(false);
  });

  it("取消归档项（模式为空 + unarchive）有凭据时可勾选", () => {
    expect(isActionable(group({ archiveAction: "unarchive", previewToken: "t" }))).toBe(true);
  });

  it("取消归档项没有凭据时不可勾选", () => {
    expect(isActionable(group({ archiveAction: "unarchive" }))).toBe(false);
  });

  it("字面全勾：取消归档与覆盖都进全选", () => {
    const unarchive = group({ archiveAction: "unarchive", previewToken: "t" });
    expect(isActionable(unarchive)).toBe(true);
    expect(isDirectlySyncable(unarchive)).toBe(true);
    expect(
      isDirectlySyncable(group({ archiveAction: "statusOnly", previewToken: "t" })),
    ).toBe(true);
    expect(
      isDirectlySyncable(group({ availableModes: ["overwrite"], previewToken: "t" })),
    ).toBe(true);
  });

  it("既无正文模式也无 archiveAction 时不可勾选", () => {
    expect(isActionable(group({ previewToken: "t" }))).toBe(false);
  });

  it("正文模式存在但没有凭据时不可勾选", () => {
    expect(isActionable(group({ availableModes: ["overwrite"] }))).toBe(false);
  });
});

describe("archiveOnly / unarchiveOnly 互斥", () => {
  it("statusOnly 只算归档，不算取消归档", () => {
    const archived = group({ archiveAction: "statusOnly" });
    expect(archiveOnly(archived)).toBe(true);
    expect(unarchiveOnly(archived)).toBe(false);
  });

  it("unarchive 只算取消归档，不算归档", () => {
    const unarchived = group({ archiveAction: "unarchive" });
    expect(archiveOnly(unarchived)).toBe(false);
    expect(unarchiveOnly(unarchived)).toBe(true);
  });

  it("没有 archiveAction 时两者都为 false", () => {
    expect(archiveOnly(group())).toBe(false);
    expect(unarchiveOnly(group())).toBe(false);
  });
});

describe("buildSelections", () => {
  it("正文模式优先：不会误选 statusOnly", () => {
    const target = group({
      availableModes: ["fastForward"],
      archiveAction: "statusOnly",
      previewToken: "t",
    });
    expect(buildSelections([target], new Set(["g-1"]))).toEqual([
      { groupId: "g-1", previewToken: "t", mode: "fastForward" },
    ]);
  });

  it("只归档项被勾选时提交 statusOnly", () => {
    const target = group({ archiveAction: "statusOnly", previewToken: "t" });
    expect(buildSelections([target], new Set(["g-1"]))).toEqual([
      { groupId: "g-1", previewToken: "t", mode: "statusOnly" },
    ]);
  });

  it("取消归档项被勾选时同样提交 statusOnly（方向由预览凭据决定）", () => {
    const target = group({ archiveAction: "unarchive", previewToken: "t" });
    expect(buildSelections([target], new Set(["g-1"]))).toEqual([
      { groupId: "g-1", previewToken: "t", mode: "statusOnly" },
    ]);
  });

  it("正文模式优先：取消归档项仍只提交正文模式", () => {
    const target = group({
      availableModes: ["fastForward"],
      archiveAction: "unarchive",
      previewToken: "t",
    });
    expect(buildSelections([target], new Set(["g-1"]))).toEqual([
      { groupId: "g-1", previewToken: "t", mode: "fastForward" },
    ]);
  });

  it("diverge 项带 unarchive 时只提交 overwrite，解档交给后端副作用", () => {
    const target = group({
      verdict: "diverge",
      availableModes: ["overwrite"],
      archiveAction: "unarchive",
      previewToken: "t",
    });
    expect(buildSelections([target], new Set(["g-1"]))).toEqual([
      { groupId: "g-1", previewToken: "t", mode: "overwrite" },
    ]);
  });

  it("未勾选则不产生提交", () => {
    const target = group({ archiveAction: "statusOnly", previewToken: "t" });
    expect(buildSelections([target], new Set())).toEqual([]);
  });

  it("不可勾选的项即使被勾也不提交", () => {
    const target = group({ availableModes: [], previewToken: "t" });
    expect(buildSelections([target], new Set(["g-1"]))).toEqual([]);
  });

  it("镜像切换是独立通道：全选产出的 selections 不含任何删除计划字段", () => {
    const groups = [
      group({ groupId: "ff", availableModes: ["fastForward"], previewToken: "t1" }),
      group({ groupId: "ow", availableModes: ["overwrite"], previewToken: "t3" }),
    ];
    const checked = new Set(groups.filter(isActionable).map((item) => item.groupId));
    const selections = buildSelections(groups, checked);
    expect(selections).toHaveLength(2);
    for (const item of selections) {
      // 镜像切换绝不复用 syncSelections / buildSelections 通道。
      expect(Object.keys(item).sort()).toEqual(["groupId", "mode", "previewToken"]);
      expect(item).not.toHaveProperty("mirrorPlan");
      expect(item).not.toHaveProperty("targets");
      expect(item).not.toHaveProperty("motherUid");
      expect(item).not.toHaveProperty("expectedDigest");
    }
  });
});

describe("模式优先级与全选口径（字面全勾）", () => {
  it("primaryMode 在模式为空时仍返回 null", () => {
    expect(primaryMode(group({ archiveAction: "statusOnly" }))).toBeNull();
    expect(primaryMode(group({ availableModes: ["overwrite"] }))).toBe("overwrite");
  });

  it("全选字面全勾：正文快进、纯归档、覆盖、取消归档都进全选", () => {
    expect(isDirectlySyncable(group({ availableModes: ["fastForward"], previewToken: "t" }))).toBe(
      true,
    );
    expect(
      isDirectlySyncable(group({ availableModes: [], archiveAction: "statusOnly", previewToken: "t" })),
    ).toBe(true);
    expect(isDirectlySyncable(group({ availableModes: ["overwrite"], previewToken: "t" }))).toBe(
      true,
    );
    expect(
      isDirectlySyncable(group({ availableModes: [], archiveAction: "unarchive", previewToken: "t" })),
    ).toBe(true);
  });

  it("字面全勾仍不收录没有凭据或没有动作的项", () => {
    expect(
      isDirectlySyncable(group({ availableModes: [], archiveAction: "statusOnly" })),
    ).toBe(false);
    expect(isDirectlySyncable(group({ availableModes: [] }))).toBe(false);
  });

  it("全选收录取消归档项（会把历史会话放回主列表，提交前必须二次确认）", () => {
    expect(
      isDirectlySyncable(group({ availableModes: [], archiveAction: "unarchive", previewToken: "t" })),
    ).toBe(true);
  });

  it("全选后逐条提交快进 + 覆盖 + 仅归档 + 取消归档，清单不折叠", () => {
    const groups = [
      group({ groupId: "ff", availableModes: ["fastForward"], previewToken: "t1" }),
      group({ groupId: "arch", archiveAction: "statusOnly", previewToken: "t2" }),
      group({ groupId: "ow", availableModes: ["overwrite"], previewToken: "t3" }),
      group({ groupId: "unarch", archiveAction: "unarchive", previewToken: "t4" }),
    ];
    const checked = new Set(groups.filter(isDirectlySyncable).map((item) => item.groupId));
    expect(checked).toEqual(new Set(["ff", "arch", "ow", "unarch"]));
    expect(buildSelections(groups, checked)).toEqual([
      { groupId: "ff", previewToken: "t1", mode: "fastForward" },
      { groupId: "arch", previewToken: "t2", mode: "statusOnly" },
      { groupId: "ow", previewToken: "t3", mode: "overwrite" },
      { groupId: "unarch", previewToken: "t4", mode: "statusOnly" },
    ]);
  });
});

describe("summarySentence", () => {
  it("本侧副本已删时说明是哪一侧没了，不再说「暂时无法确认两边内容」", () => {
    expect(summarySentence(group({ verdict: "unknown", sourceDeleted: true }), "账号B")).toBe(
      "当前账号里这个会话已删除，本次不会同步",
    );
  });

  it("只归档（identical）说明仅同步归档、保留正文", () => {
    expect(summarySentence(group({ verdict: "identical", archiveAction: "statusOnly" }), "账号B")).toBe(
      "仅同步归档，保留正文",
    );
  });

  it("只归档（ahead）同样说明仅同步归档，不再是「本次不同步」", () => {
    expect(summarySentence(group({ verdict: "ahead", archiveAction: "statusOnly" }), "账号B")).toBe(
      "仅同步归档，保留正文",
    );
  });

  it("快进附带归档时保留正文句并写明一并归档", () => {
    const sentence = summarySentence(
      group({ verdict: "fastForward", availableModes: ["fastForward"], archiveAction: "statusOnly" }),
      "账号B",
    );
    expect(sentence).toContain("将当前账号的新内容同步到「账号B」");
    expect(sentence).toContain("一并归档");
  });

  it("纯取消归档（identical）说明仅取消归档、保留正文", () => {
    expect(
      summarySentence(group({ verdict: "identical", archiveAction: "unarchive" }), "账号B"),
    ).toBe("仅取消归档，保留正文");
  });

  it("纯取消归档（ahead）同样说明仅取消归档，不会说成「本次不同步」", () => {
    expect(summarySentence(group({ verdict: "ahead", archiveAction: "unarchive" }), "账号B")).toBe(
      "仅取消归档，保留正文",
    );
  });

  it("正文快进附带取消归档时，句尾写明目标会话一并取消归档", () => {
    expect(
      summarySentence(
        group({
          verdict: "fastForward",
          availableModes: ["fastForward"],
          archiveAction: "unarchive",
        }),
        "账号B",
      ),
    ).toBe("将当前账号的新内容同步到「账号B」，目标会话一并取消归档");
  });

  it("没有归档动作时文案与旧行为逐字一致", () => {
    expect(summarySentence(group({ verdict: "identical" }), "账号B")).toBe("无需同步");
    expect(summarySentence(group({ verdict: "ahead" }), "账号B")).toBe("保留目标内容，本次不同步");
    expect(
      summarySentence(group({ verdict: "fastForward", availableModes: ["fastForward"] }), "账号B"),
    ).toBe("将当前账号的新内容同步到「账号B」");
    expect(summarySentence(group({ verdict: "unknown" }), "账号B")).toBe(
      "暂时无法确认两边内容，本次不会同步",
    );
  });
});

describe("shouldDefaultSelect — 默认全关", () => {
  it("偏好关闭时：纯归档项默认不勾选", () => {
    const pureArchive = group({
      archiveAction: "statusOnly",
      previewToken: "tok-1",
      availableModes: [],
      defaultChecked: false,
    });
    expect(shouldDefaultSelect(pureArchive, false)).toBe(false);
  });

  it("后端显式 defaultChecked=true 也不再默认勾选（默认全关）", () => {
    const ff = group({
      availableModes: ["fastForward"],
      previewToken: "tok-1",
      defaultChecked: true,
    });
    expect(shouldDefaultSelect(ff, false)).toBe(false);
  });

  it("偏好开启时：纯归档项也不再自动预选（autoArchive 已取消）", () => {
    const pureArchive = group({
      archiveAction: "statusOnly",
      previewToken: "tok-1",
      availableModes: [],
      defaultChecked: true,
    });
    expect(shouldDefaultSelect(pureArchive, true)).toBe(false);
  });

  it("偏好开启时：无凭据的纯归档项不预选（非 actionable）", () => {
    const unActionable = group({
      archiveAction: "statusOnly",
      previewToken: undefined,
      availableModes: [],
      defaultChecked: true,
    });
    expect(shouldDefaultSelect(unActionable, true)).toBe(false);
  });

  it("防覆盖铁律：Diverge 冲突项附带 statusOnly 时绝不自动预选", () => {
    const divergeGroup = group({
      verdict: "diverge",
      availableModes: ["overwrite"],
      archiveAction: "statusOnly",
      previewToken: "tok-1",
      defaultChecked: true,
    });
    expect(shouldDefaultSelect(divergeGroup, true)).toBe(false);
  });

  it("取消归档项绝不预选", () => {
    const pureUnarchive = group({
      archiveAction: "unarchive",
      previewToken: "tok-1",
      availableModes: [],
      defaultChecked: true,
    });
    expect(shouldDefaultSelect(pureUnarchive, true)).toBe(false);
    expect(shouldDefaultSelect(pureUnarchive, false)).toBe(false);
  });

  it("正文快进附带取消归档时也不预选", () => {
    const ff = group({
      availableModes: ["fastForward"],
      archiveAction: "unarchive",
      previewToken: "tok-1",
      defaultChecked: true,
    });
    expect(shouldDefaultSelect(ff, true)).toBe(false);
  });

  it("Diverge 冲突项附带 unarchive 时同样不预选", () => {
    const divergeGroup = group({
      verdict: "diverge",
      availableModes: ["overwrite"],
      archiveAction: "unarchive",
      previewToken: "tok-1",
      defaultChecked: true,
    });
    expect(shouldDefaultSelect(divergeGroup, true)).toBe(false);
  });

  it("无归档动作的普通未改动项不预选", () => {
    const unchanged = group({
      previewToken: "tok-1",
      availableModes: [],
      defaultChecked: true,
    });
    expect(shouldDefaultSelect(unchanged, true)).toBe(false);
  });

  it("正文快进项沿用默认全关：defaultChecked 真/假都不预选", () => {
    const ffChecked = group({
      availableModes: ["fastForward"],
      previewToken: "tok-1",
      defaultChecked: true,
    });
    const ffUnchecked = group({
      availableModes: ["fastForward"],
      previewToken: "tok-1",
      defaultChecked: false,
    });
    expect(shouldDefaultSelect(ffChecked, true)).toBe(false);
    expect(shouldDefaultSelect(ffUnchecked, true)).toBe(false);
  });
});

describe("partitionDeleted", () => {
  it("本侧已删的组单独归到 deleted，其余留在主列表", () => {
    const live = group({ groupId: "g-live", availableModes: ["fastForward"], previewToken: "t" });
    const dead = group({ groupId: "g-dead", verdict: "unknown", sourceDeleted: true });
    const { live: kept, deleted } = partitionDeleted([live, dead]);

    expect(kept.map((item) => item.groupId)).toEqual(["g-live"]);
    expect(deleted.map((item) => item.groupId)).toEqual(["g-dead"]);
  });

  it("没有任何已删项时主列表原样保留、deleted 为空", () => {
    const items = [group({ groupId: "g-1" }), group({ groupId: "g-2" })];
    const result = partitionDeleted(items);

    expect(result.live).toHaveLength(2);
    expect(result.deleted).toHaveLength(0);
  });

  it("已删项永远不可勾选：即使后端误发了模式与凭据", () => {
    const dead = group({
      groupId: "g-dead",
      verdict: "unknown",
      sourceDeleted: true,
      availableModes: ["fastForward"],
      previewToken: "t",
    });
    const { live, deleted } = partitionDeleted([dead]);

    expect(live).toHaveLength(0);
    expect(isActionable(deleted[0])).toBe(false);
    expect(isDirectlySyncable(deleted[0])).toBe(false);
  });
});

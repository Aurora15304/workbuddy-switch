import { displayName } from "@/lib/account-display";
import type {
  AccountMeta,
  MirrorPlan,
  MirrorPreview,
  MemoryMergeReport,
} from "@/lib/types";

/**
 * 工作记忆三选一的前端单一决策源：提交参数、母本默认值、摘要文案与结果 toast 都从这里出。
 *
 * 与 Rust 侧的死契约：不传 `memoryMode` = 不同步；`"merge"` = 合并；`"overwrite"` = 复制覆盖
 * （必须同时带 `memorySourceUid`）。母本 uid 必须是账号库里的 `uid` 字段，因为后端以
 * `user-{uid}-personal/MEMORY.md` 定位文件；`id` / `email` 只用来「认出」当前账号，不能当 value。
 *
 * 本模块是纯函数：不碰 DOM、不订阅 store、不发起请求，可在 node 环境下完整测试。
 */

/** 三选一。`off` 是默认，破坏性的 `overwrite` 永远不会被默认选中。 */
export type MemoryMode = "off" | "merge" | "overwrite";

/** 弹窗打开与初始状态共用的默认值：不同步。单独成常量便于测试守住「默认不选覆盖」。 */
export const DEFAULT_MEMORY_MODE: MemoryMode = "off";

/**
 * 每次打开弹窗的工作记忆选择：一律回到「不同步 + 跟随默认母本」。
 * 上次选的覆盖绝不能静默延续到下一次切号，否则会连续替换其他账号的记忆。
 */
export function initialMemorySelection(): { mode: MemoryMode; sourceUid: string | null } {
  return { mode: DEFAULT_MEMORY_MODE, sourceUid: null };
}

/** 会真正写入后端的两种模式。 */
type ActiveMode = Exclude<MemoryMode, "off">;

/** 提交给 `api.switchAccount` 的记忆参数；空对象 = 不同步（旧版「不勾选」）。 */
export interface MemorySwitchArgs {
  memoryMode?: ActiveMode;
  memorySourceUid?: string;
}

/** mode → 提交参数。边界一次性归一化，组件里不再散落条件判断。 */
export function toMemorySwitchArgs(mode: MemoryMode, sourceUid: string): MemorySwitchArgs {
  if (mode === "off") return {};
  if (mode === "merge") return { memoryMode: "merge" };
  // 破坏性操作的最后一道安全网：没母本就退化成「不同步」，绝不发带空 uid 的覆盖请求。
  if (!canSubmitMemory(mode, sourceUid)) return {};
  return { memoryMode: "overwrite", memorySourceUid: sourceUid };
}

/** 覆盖必须带母本才能提交；无账号可选 / 无法确定母本时拦下，不让破坏性请求裸奔。 */
export function canSubmitMemory(mode: MemoryMode, sourceUid: string): boolean {
  if (mode !== "overwrite") return true;
  return sourceUid.trim().length > 0;
}

/** 只有覆盖模式需要母本下拉。 */
export function showsSourcePicker(mode: MemoryMode): boolean {
  return mode === "overwrite";
}

/** 底部摘要 / 「附加同步」徽标用的资产名；不同步返回 null 表示不计入。 */
export function memoryAssetLabel(mode: MemoryMode): string | null {
  if (mode === "off") return null;
  return mode === "overwrite" ? "工作记忆（覆盖）" : "工作记忆";
}

/** 覆盖模式的醒目摘要警示：明说会替换他人记忆 + 自动备份可还原。 */
export function memorySummaryWarning(mode: MemoryMode): string | null {
  if (mode !== "overwrite") return null;
  return "将用母本账号的记忆替换其他账号原有的工作记忆（覆盖前自动备份，可还原）";
}

// ---------------------------------------------------------------------------
// 母本账号解析
// ---------------------------------------------------------------------------

/** 母本下拉的一项。`uid` 是提交给后端的 value，`label` 是显示名。 */
export interface MemorySourceOption {
  uid: string;
  label: string;
  /** 是否就是当前登录账号（仅用于下拉里的「当前登录」标注）。 */
  isCurrent: boolean;
}

export interface MemorySourceResolution {
  options: MemorySourceOption[];
  /** 默认选中的 uid；账号库无可用母本时为 null。 */
  defaultUid: string | null;
  /** 没能认出当前账号：已退到列表第一项，界面需提醒用户确认母本。 */
  needsConfirm: boolean;
}

/** 当前账号能否被该条记录认出来：对齐账号页 `isWorkbuddyCurrent` 的 uid / id / email 口径。 */
function matchesCurrent(account: AccountMeta, hint: string): boolean {
  return account.uid === hint || account.id === hint || account.email === hint;
}

/**
 * 账号库 + 一组「当前账号」候选标识（按优先级排列，可含 null）→ 母本选项与默认选中。
 *
 * 边界归一化在这里一次做完：过滤掉后端无法定位的无 uid 账号、按优先级认当前账号、
 * 认不出时退到第一项并置 `needsConfirm`，调用方（组件）只消费结果，不再自己判断。
 */
export function resolveMemorySources(
  accounts: AccountMeta[],
  currentHints: (string | null)[],
): MemorySourceResolution {
  const eligible = accounts.filter((a): a is AccountMeta & { uid: string } =>
    Boolean(a.uid && a.uid.trim()),
  );
  if (eligible.length === 0) {
    return { options: [], defaultUid: null, needsConfirm: true };
  }

  for (const hint of currentHints) {
    if (!hint) continue;
    const matched = eligible.find((a) => matchesCurrent(a, hint));
    if (!matched) continue;
    return {
      options: eligible.map((a) => ({
        uid: a.uid,
        label: displayName(a),
        isCurrent: a.uid === matched.uid,
      })),
      defaultUid: matched.uid,
      needsConfirm: false,
    };
  }

  // 认不出当前账号：不静默猜，默认第一项并把「请确认母本」的责任交回界面。
  return {
    options: eligible.map((a) => ({ uid: a.uid, label: displayName(a), isCurrent: false })),
    defaultUid: eligible[0].uid,
    needsConfirm: true,
  };
}

// ---------------------------------------------------------------------------
// 结果 toast
// ---------------------------------------------------------------------------

/** 与 sonner 的 `toast[level](title, { description })` 一一对应。 */
export type MemoryToastLevel = "success" | "error" | "warning" | "info";
export interface MemoryToast {
  level: MemoryToastLevel;
  title: string;
  description: string;
}

/** 表驱动：随 mode 变化的固定文案，避免函数里堆 `if (mode === ...)`。 */
const COPY: Record<ActiveMode, {
  noReportTitle: string;
  noReportDescription: string;
  issueTitle: string;
  noteTitle: string;
}> = {
  merge: {
    noReportTitle: "工作记忆未合并",
    noReportDescription: "没有收到合并结果：当前版本可能不支持该功能，账号已切换，但记忆没有改动。",
    issueTitle: "工作记忆没有全部合并",
    noteTitle: "工作记忆未合并",
  },
  overwrite: {
    noReportTitle: "工作记忆未覆盖",
    noReportDescription: "没有收到覆盖结果：当前版本可能不支持该功能，账号已切换，但记忆没有改动。",
    issueTitle: "工作记忆没有全部覆盖",
    noteTitle: "工作记忆未覆盖",
  },
};

/** 成功文案按 mode 分流：合并报「几份合成几字」，覆盖报「把谁的记忆复制到几个账号」。 */
function successToast(mode: ActiveMode, r: MemoryMergeReport): MemoryToast {
  if (mode === "overwrite") {
    const sourceLabel = r.sources[0]?.label || "母本账号";
    return {
      level: "success",
      title: `已把「${sourceLabel}」的记忆复制到 ${r.written.length} 个账号`,
      description: "其他账号原有记忆已被替换，原文备份为 MEMORY.md.bak-时间戳，可随时还原。",
    };
  }
  return {
    level: "success",
    title: `工作记忆已合并并写入 ${r.written.length} 个账号`,
    description: `${r.sources.length} 份记忆合成 ${r.mergedChars} 字；被覆盖的原文已备份为 MEMORY.md.bak-时间戳`,
  };
}

/**
 * 报告 → 需要弹出的 toast 列表。文案按**请求 mode** 决定（用户选了什么就报什么），
 * 不依赖报告回填的 `mode` 字段，老宿主缺字段时照样正确分级。
 */
export function memoryToasts(
  mode: MemoryMode,
  r: MemoryMergeReport | undefined,
): MemoryToast[] {
  if (mode === "off") return [];
  const copy = COPY[mode];
  if (!r) {
    return [{ level: "warning", title: copy.noReportTitle, description: copy.noReportDescription }];
  }

  const issues = [
    ...r.errors.map((item) => `${item.label}：${item.error}`),
    ...(r.persona?.errors ?? []).map((item) => `${item.label} / ${item.file}：${item.error}`),
  ];
  const toasts: MemoryToast[] = [];
  if (issues.length > 0) {
    toasts.push({ level: "error", title: copy.issueTitle, description: issues.join("；") });
  }
  if (r.written.length > 0) {
    toasts.push(successToast(mode, r));
  } else if (issues.length === 0 && r.note) {
    toasts.push({ level: "info", title: copy.noteTitle, description: r.note });
  } else if (issues.length === 0 && r.unchanged.length > 0) {
    toasts.push({ level: "info", title: "工作记忆无需更新", description: "所有账号的记忆内容已经一致。" });
  }
  return toasts;
}

// ---------------------------------------------------------------------------
// 弹窗「打开 / 重开」的单一决策源
// ---------------------------------------------------------------------------

/** 切号弹窗里所有非会话勾选状态的初值。初始 useState 与每次重开重置共用它，避免两者不一致。 */
export interface SwitchExtras {
  memoryMode: MemoryMode;
  memorySourceUid: string | null;
  mergeExperts: boolean;
  syncPersona: boolean;
  personaMode: "safe" | "overwrite";
  mirror: MirrorSelection;
}

/** 默认全关：人设 / 专家团 / 记忆 / 镜像切换全部不预选，母本 uid 由登录态确定。 */
export function initialSwitchExtras(): SwitchExtras {
  const memory = initialMemorySelection();
  return {
    memoryMode: memory.mode,
    memorySourceUid: memory.sourceUid,
    mergeExperts: false,
    syncPersona: false,
    personaMode: "safe",
    mirror: initialMirrorSelection(),
  };
}

// ---------------------------------------------------------------------------
// 镜像切换：母本 = 当前登录账号 + 已知会先备份 + 二次确认，独立于工作记忆三选一
// ---------------------------------------------------------------------------

/** 镜像切换的用户选择状态。母本 uid 由登录态确定，绝不从 `defaultUid` 派生。 */
export interface MirrorSelection {
  /** 母本 uid（当前登录账号）；null = 预览未返回，禁止提交。 */
  motherUid: string | null;
  /** 勾选「我已知会先备份」。 */
  acknowledged: boolean;
  /** 二次确认（复述母本名 + 删除数）已通过。 */
  confirmed: boolean;
}

/** 每次打开弹窗的干净初值：默认全关，破坏性选择绝不静默延续。 */
export function initialMirrorSelection(): MirrorSelection {
  return { motherUid: null, acknowledged: false, confirmed: false };
}

/**
 * 提交门禁：母本 uid 已绑定 + 已知会先备份 + 二次确认 + 预览凭证有效且绑定同一母本。
 * `defaultUid` 不在参数里——从源头杜绝「默认母本代入镜像切换」。
 */
export function canSubmitMirror(
  selection: MirrorSelection,
  preview: MirrorPreview | null,
): boolean {
  if (!selection.motherUid || !selection.acknowledged || !selection.confirmed) return false;
  if (!preview || !preview.previewToken.trim() || preview.targets.length === 0) return false;
  if (preview.motherUid !== selection.motherUid) return false;
  return true;
}

/** 边界一次性归一化：只输出冻结的四字段 MirrorPlan，不夹带任何别的通道。 */
export function toMirrorSwitchArgs(
  selection: MirrorSelection,
  preview: MirrorPreview | null,
): { mirrorPlan?: MirrorPlan } {
  if (!canSubmitMirror(selection, preview) || !preview) return {};
  return {
    mirrorPlan: {
      motherUid: preview.motherUid,
      targets: preview.targets,
      expectedDigest: preview.expectedDigest,
      previewToken: preview.previewToken,
    },
  };
}

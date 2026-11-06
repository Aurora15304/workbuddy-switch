import { useState } from "react";
import { ChevronRight, CircleAlert } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import type {
  SessionLinkPreviewGroup,
  SessionLinksPreview,
  SessionSyncMode,
  SessionSyncSelection,
  SessionSyncVerdict,
} from "@/lib/types";

/**
 * 「关联会话」区块的纯展示件与勾选逻辑（WorkBuddy 与 VS Code 插件共用）。
 *
 * 只放两边**完全一致**的部分：判定文案表、列表骨架、卡片、勾选组装与一句摘要。
 * 数据加载仍留在各自的 section（数据源命令与档位约定不同）。
 */

/** 关联会话区块对父组件暴露的状态（tab 徽标 / 常驻提示用）。 */
export interface SessionLinksMeta {
  /** 该区块是否应渲染（不支持时为 false，父组件不渲染本 tab） */
  available: boolean;
  /** 可同步的会话数量（tab 徽标数字，0 时父组件不显示徽标） */
  groupCount: number;
  /** 预览请求失败的原因；非空时父组件在 tab 之上常驻提示 */
  error: string;
  /** 关联存储状态；unavailable 时父组件常驻提示原因 */
  storeStatus: SessionLinksPreview["storeStatus"] | null;
  storeError: string;
}

/** 判定结果的中文标签（与 core 的 verdict 一一对应，只表达状态，动作交给摘要句）。 */
export const VERDICT_LABEL: Record<SessionSyncVerdict, string> = {
  fastForward: "可同步",
  diverge: "双方都有更新",
  ahead: "目标账号有更新",
  identical: "内容一致",
  unknown: "无法确认",
};

export const VERDICT_BADGE: Record<SessionSyncVerdict, "success" | "warning" | "outline" | "secondary"> = {
  fastForward: "success",
  diverge: "warning",
  ahead: "warning",
  identical: "secondary",
  unknown: "outline",
};

/**
 * 列表区最小高度：加载骨架、空态与「少量会话」的结果态共用同一下沿。
 *
 * 弹窗是垂直居中定位（`translate-y-[-50%]` 按自身高度算），内容高度一跳弹窗就上下撑开。
 * 打开时先渲染加载态、数据到达后换成结果，两端高度差越大跳得越明显；固定下沿让这段跳变收窄。
 */
export const LIST_MIN_H = "min-h-[min(7.5rem,26vh)]";
/** 状态块高度：约等于结果态「全选行 + 列表 + 提示行」的最小高度，供空态与骨架对齐。 */
export const STATUS_MIN_H = "min-h-[min(11rem,34vh)]";

/** 加载骨架：结构与结果态一致（全选行 + 列表 + 提示行），避免数据到达时弹窗高度跳变。 */
export function LinksSkeleton() {
  return (
    <div className={cn("space-y-2", STATUS_MIN_H)} aria-hidden>
      <div className="flex items-center gap-2.5 px-1">
        <Skeleton className="size-3.5 shrink-0 rounded-sm" />
        <Skeleton className="h-5 w-40" />
        <Skeleton className="ml-auto h-5 w-16" />
      </div>
      <div className={cn("divide-y rounded-md border", LIST_MIN_H)}>
        {[0, 1].map((index) => (
          <div key={index} className="flex items-start gap-2.5 px-3 py-2.5">
            <Skeleton className="mt-0.5 size-3.5 shrink-0 rounded-sm" />
            <div className="min-w-0 flex-1 space-y-2">
              <Skeleton className="h-4 w-1/2" />
              <Skeleton className="h-3 w-2/3" />
            </div>
          </div>
        ))}
      </div>
      <Skeleton className="h-4 w-56" />
    </div>
  );
}

/** 可勾选的模式：判定只允许一个模式，取后端给出的第一个。 */
export function primaryMode(group: SessionLinkPreviewGroup): SessionSyncMode | null {
  return group.availableModes.length > 0 ? group.availableModes[0] : null;
}

/**
 * 归档动作方向：后端一次只给一个值。
 *
 * 数据异常同时出现两种取值时按「归档」优先——文案与默认勾选都不能因为脏数据打架。
 */
type ArchiveDirection = "archive" | "unarchive";

function archiveDirection(group: SessionLinkPreviewGroup): ArchiveDirection | null {
  if (group.archiveAction === "statusOnly") return "archive";
  if (group.archiveAction === "unarchive") return "unarchive";
  return null;
}

/** 仅同步归档：源已归档 → 目标归档。`archiveAction` 由后端给出，不随用户勾选变化。 */
export function archiveOnly(group: SessionLinkPreviewGroup): boolean {
  return archiveDirection(group) === "archive";
}

/** 仅取消归档：源已恢复 → 目标取消归档。方向同样由后端给出。 */
export function unarchiveOnly(group: SessionLinkPreviewGroup): boolean {
  return archiveDirection(group) === "unarchive";
}

/**
 * 该项能否勾选：有正文写入模式，或后端授权了仅同步状态（归档 / 取消归档，须带预览凭据）。
 *
 * 纯状态项的 `availableModes` 为空，必须靠 `archiveAction` 放行，否则内容一致的
 * 会话永远传导不到目标的归档状态。
 */
export function isActionable(group: SessionLinkPreviewGroup): boolean {
  if (!group.previewToken) return false;
  return primaryMode(group) !== null || archiveDirection(group) !== null;
}

/**
 * 全选口径：正文可快进，或仅同步归档（把目标收起来）。
 *
 * 纯归档项必须进全选——它没有正文模式，逐条勾选在批量归档（一次上百个）时不可用，
 * 导致源账号的归档状态在目标账号里一直对不上。
 *
 * 两个动作排除在外，都必须逐条显式授权：
 * - 覆盖（overwrite）：唯一会改写目标正文的动作；
 * - 取消归档（unarchive）：会把大量历史会话一次性放回目标账号主列表，事后难复原。
 */
export function isDirectlySyncable(group: SessionLinkPreviewGroup): boolean {
  // 取消归档会让目标账号的主列表一次性涌入大量历史会话（updatedAt 全被顶到最近），
  // 事后很难逐条恢复归档；按「影响面大的动作逐条授权」处理，与 overwrite 同侧：
  // 它可勾选，但不进全选。
  return (
    isActionable(group) &&
    primaryMode(group) !== "overwrite" &&
    !unarchiveOnly(group)
  );
}

/**
 * 决定切号弹窗初始化时该关联会话是否默认预先勾选（#165 方案 3）。
 *
 * 规则：
 * 1. 项本身必须可勾选且有凭据（isActionable）；
 * 2. 满足下列其一即可预先勾选：
 *    - 后端显式指定默认勾选（如 fastForward 正文快进）；
 *    - 用户开启了自动同步归档偏好（autoArchive），且该项为纯归档项（具备 archiveAction 且 primaryMode === null）。
 *
 * 绝对边界：Diverge（覆盖模式，primaryMode === "overwrite"）无论是否具备归档资格，绝不自动预选！
 * 取消归档（unarchive）同样绝不预选：源账号刚恢复，不代表目标账号也该跟着解档。
 */
export function shouldDefaultSelect(
  group: SessionLinkPreviewGroup,
  autoArchive: boolean,
): boolean {
  return (
    isActionable(group) &&
    (group.defaultChecked ||
      (autoArchive && archiveOnly(group) && primaryMode(group) === null))
  );
}

/**
 * 组装提交给后端的同步选择：只有「用户勾选 + 后端给出模式与预览凭据」的组才发送。
 * 前端不推断模式，也不为禁选项补默认值。
 */
export function buildSelections(
  groups: SessionLinkPreviewGroup[],
  checked: Set<string>,
): SessionSyncSelection[] {
  return groups.flatMap((group) => {
    if (!checked.has(group.groupId)) return [];
    if (!group.previewToken) return [];
    // 正文模式优先：有正文写入就提交该模式，归档状态作为它的副作用一并由后端处理。
    const mode = primaryMode(group);
    if (mode) return [{ groupId: group.groupId, previewToken: group.previewToken, mode }];
    // 没有正文模式时，仅同步状态（归档 / 取消归档）是可提交的独立动作；
    // 方向已由预览凭据绑定，提交口只有一个 statusOnly，前端不额外传方向。
    if (archiveDirection(group) !== null) {
      return [
        { groupId: group.groupId, previewToken: group.previewToken, mode: "statusOnly" as const },
      ];
    }
    return [];
  });
}

/** 两个归档方向在文案里的说法：独立动作说「同步归档」，顺带动作说「一并归档」。 */
const ARCHIVE_SENTENCE: Record<ArchiveDirection, { only: string; along: string }> = {
  archive: { only: "仅同步归档，保留正文", along: "目标会话一并归档" },
  unarchive: { only: "仅取消归档，保留正文", along: "目标会话一并取消归档" },
};

/** 卡片摘要句：一句人话说清发生了什么；条数与原始原因收进「查看详情」。 */
export function summarySentence(group: SessionLinkPreviewGroup, targetLabel: string): string {
  const direction = archiveDirection(group);
  // 纯状态项：正文判定会说「无需同步/本次不同步」，但本次确实要改状态，必须说清。
  if (direction && primaryMode(group) === null) {
    return ARCHIVE_SENTENCE[direction].only;
  }
  const base = bodySummarySentence(group, targetLabel);
  // 正文同步附带改状态时，在正文句之后补一句，避免用户以为只同步了正文。
  return direction ? `${base}，${ARCHIVE_SENTENCE[direction].along}` : base;
}

/** 纯按正文判定给出的摘要句（不含归档补充）。 */
function bodySummarySentence(group: SessionLinkPreviewGroup, targetLabel: string): string {
  switch (group.verdict) {
    case "fastForward":
      return `将当前账号的新内容同步到「${targetLabel}」`;
    case "diverge":
      return "勾选将用当前账号内容覆盖目标全文。";
    case "ahead":
      return "保留目标内容，本次不同步";
    case "identical":
      return "无需同步";
    case "unknown":
      return "暂时无法确认两边内容，本次不会同步";
  }
}

export const RECORD_COUNT_HINT = "按会话内容的条数统计，不是对话轮数；条数相同也不代表内容顺序完全一致。";

/** 会话行：勾选框 + 标题 + 一句摘要 + 判定徽标 + 折叠箭头（路径 / 条数 / 原因）。 */
export function SessionLinkCard({
  group,
  targetLabel,
  checked,
  disabled,
  onToggle,
}: {
  group: SessionLinkPreviewGroup;
  targetLabel: string;
  checked: boolean;
  disabled?: boolean;
  onToggle: (next: boolean) => void;
}) {
  const [open, setOpen] = useState(false);
  const canSelect = isActionable(group);
  const overwrite = primaryMode(group) === "overwrite";
  const title = group.title || "(无标题)";

  return (
    <Collapsible open={open} onOpenChange={setOpen} className="px-3 py-2.5">
      <div className="flex items-start gap-2.5">
        {canSelect ? (
          <Checkbox
            className="mt-0.5"
            checked={checked}
            disabled={disabled}
            onCheckedChange={(state) => onToggle(state === true)}
            aria-label={`同步会话 ${title}`}
          />
        ) : (
          <span className="mt-0.5 size-3.5 shrink-0" aria-hidden />
        )}
        <div className="min-w-0 flex-1 space-y-0.5">
          <div className="flex items-center gap-2">
            <span className="min-w-0 flex-1 truncate text-sm font-medium" title={group.title}>
              {title}
            </span>
            <Badge variant={VERDICT_BADGE[group.verdict]} className="shrink-0 text-[10px]">
              {VERDICT_LABEL[group.verdict]}
            </Badge>
            <CollapsibleTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                className="size-6 shrink-0 text-muted-foreground"
                aria-label="查看详情"
              >
                <ChevronRight className={cn("size-4 transition-transform", open && "rotate-90")} />
              </Button>
            </CollapsibleTrigger>
          </div>
          <p className="text-xs text-muted-foreground">{summarySentence(group, targetLabel)}</p>
          {overwrite && (
            <p className="flex items-start gap-1 text-xs text-amber-700 dark:text-amber-400">
              <CircleAlert className="mt-px size-3.5 shrink-0" />
              <span>{`目标独有 ${group.extraB} 条记录将被替换，无法通过本工具撤销。`}</span>
            </p>
          )}
        </div>
      </div>

      <CollapsibleContent className="space-y-1.5 pt-1 text-xs text-muted-foreground">
        {group.cwd && (
          <span className="block truncate" title={group.cwd}>
            {group.cwd}
          </span>
        )}
        <Tooltip>
          <TooltipTrigger asChild>
            <span className="block w-fit cursor-help" tabIndex={0}>
              {`内容条数：当前账号 ${group.recordCount.source} 条 · 目标账号 ${group.recordCount.target} 条`}
              {group.recordCount.baseline !== null && ` · 上次一致 ${group.recordCount.baseline} 条`}
              {group.extraB > 0 && ` · 目标账号独有 ${group.extraB} 条`}
            </span>
          </TooltipTrigger>
          <TooltipContent side="top" className="max-w-xs">
            {RECORD_COUNT_HINT}
          </TooltipContent>
        </Tooltip>
        <span className="block">{group.reason}</span>
      </CollapsibleContent>
    </Collapsible>
  );
}

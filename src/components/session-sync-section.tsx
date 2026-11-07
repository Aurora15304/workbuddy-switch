import { useEffect, useLayoutEffect, useState } from "react";
import { Link2, RotateCw } from "lucide-react";
import { listen } from "@tauri-apps/api/event";

import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  buildSelections,
  isActionable,
  LinksSkeleton,
  LIST_MIN_H,
  selectAllSummaryText,
  SessionLinkCard,
  STATUS_MIN_H,
  type SessionLinksMeta,
} from "@/components/session-link-shared";
import * as api from "@/lib/api";
import { displayName } from "@/lib/account-display";
import { cn } from "@/lib/utils";
import { accountVariant } from "@/lib/variant";
import type {
  AccountMeta,
  SessionLinkPreviewGroup,
  SessionLinksPreview,
  SessionSyncSelection,
} from "@/lib/types";

export type { SessionLinksMeta };

interface Props {
  /** 目标账号（来源账号身份由后端从该档位登录态读取，前端不传） */
  account: AccountMeta | null;
  /** 弹窗是否打开：打开时拉取预览，关闭时取消回写 */
  open: boolean;
  /** 切换进行中：禁止继续交互 */
  disabled?: boolean;
  /** 勾选结果变化：父组件据此提交 `syncSelections`，并用于结果反馈里回显会话名 */
  onChange: (state: { selections: SessionSyncSelection[]; groups: SessionLinkPreviewGroup[] }) => void;
  /** 预览状态上报：父组件用于 tab 徽标与常驻提示，避免把错误藏进 tab 里 */
  onMetaChange?: (meta: SessionLinksMeta) => void;
}

/**
 * 切号弹窗「关联会话」tab 的内容：说明卡 + 会话列表（判定徽标 + 一句摘要 + 折叠详情）。
 *
 * - 默认全关：打开时勾选集合恒为空，后端 `defaultChecked` 与 autoArchive 偏好都不再
 *   自动预选（避免「切号后删掉的内容又被复制回来」）。
 * - 一键全选是字面全勾：所有 `isActionable` 项（含覆盖与取消归档）都进全选，
 *   但必须先经「汇总复核 + 二次确认」才提交，影响面逐字写明。
 * - 错误与存储不可用由父组件在 tab 之上常驻提示，本组件只保留对应的空态与重试入口。
 * - 展示件与勾选逻辑与 VS Code 插件侧共用（`session-link-shared.tsx`）。
 */
export function SessionSyncSection({ account, open, disabled, onChange, onMetaChange }: Props) {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [preview, setPreview] = useState<SessionLinksPreview | null>(null);
  /** 预览逐组进度；`total` 由后端给，没有分母就不画进度条（不猜百分比）。 */
  const [progress, setProgress] = useState<{ done: number; total: number } | null>(null);
  /** 用户逐项勾选状态；仅在可勾选项上生效。打开时恒为空（默认全关）。 */
  const [checked, setChecked] = useState<Set<string>>(new Set());
  /** 手动重试计数：用于「重新检查」。 */
  const [reloadToken, setReloadToken] = useState(0);
  /** 全选的汇总复核弹层：确认前绝不写入勾选结果。 */
  const [selectAllOpen, setSelectAllOpen] = useState(false);

  const accountId = account?.id;
  const variant = accountVariant(account);

  useLayoutEffect(() => {
    // 退出动画期间保留内容；重新打开或更换目标时在绘制前清除旧预览。
    if (!open || !accountId) return;
    let cancelled = false;
    setLoading(true);
    setError("");
    setPreview(null);
    setProgress(null);
    setChecked(new Set());
    setSelectAllOpen(false);
    onChange({ selections: [], groups: [] });
    api
      .sessionLinksPreview(accountId, variant)
      .then((res) => {
        if (cancelled) return;
        setPreview(res);
        // 默认全关：不再按后端 defaultChecked 或 autoArchive 偏好预选任何项。
        setChecked(new Set());
        onChange({ selections: [], groups: res.groups });
      })
      .catch((e) => {
        if (cancelled) return;
        setPreview(null);
        setError(api.asError(e));
        onChange({ selections: [], groups: [] });
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
    // 预览拉取只看「弹窗开关 / 目标账号 / 手动重试」；onChange 只做状态回写，
    // 不进依赖，否则父组件每次重渲染都会重新拉预览。
  }, [open, accountId, variant, reloadToken]);

  const groups = preview?.groups ?? [];
  // 国际版能力判定不通过：整块不可用（后端执行时仍会强制检查能力）。
  const unsupported = Boolean(preview && (!preview.supported || preview.storeStatus === "unsupported"));
  const targetLabel = account ? displayName(account) : "目标账号";

  // 状态上报：父组件据此渲染 tab 徽标与常驻提示。
  useEffect(() => {
    onMetaChange?.({
      available: !unsupported,
      groupCount: groups.length,
      error,
      storeStatus: preview?.storeStatus ?? null,
      storeError: preview?.storeError ?? "",
    });
    // onMetaChange 只做状态回写，不进依赖。
  }, [unsupported, groups.length, error, preview?.storeStatus, preview?.storeError]);

  // 预览进度：桌面端订阅后端事件，webui 轮询。只在预览未落定时挂载，避免空转。
  // 分母（待核对组数）由后端给，前端不按时间估算百分比。
  useEffect(() => {
    if (!open || preview || error) return;
    if (api.isWebui()) {
      let cancelled = false;
      const timer = setInterval(() => {
        void api
          .sessionLinksProgress()
          .then((state) => {
            // 预览可能已落定 / 弹窗已关闭：迟到的进度帧不能再回写状态。
            if (cancelled) return;
            if (state.running && state.total > 0) {
              setProgress({ done: state.done, total: state.total });
            }
          })
          .catch(() => {
            /* 进度是尽力而为：拿不到就退回骨架，不影响预览本身 */
          });
      }, 400);
      return () => {
        cancelled = true;
        clearInterval(timer);
      };
    }
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<{ done: number; total: number }>("session-links-progress", (event) => {
      setProgress(event.payload);
    }).then((fn) => {
      // StrictMode / 预览落定时 Promise 可能晚于清理才 resolve：立即解绑，避免残留监听。
      if (disposed) {
        fn();
        return;
      }
      unlisten = fn;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [open, preview, error]);

  // 字面全勾：所有可勾选项都进全选（含覆盖与取消归档）。
  const actionable = groups.filter(isActionable);
  const selectedCount = actionable.filter((group) => checked.has(group.groupId)).length;
  const allSelected = actionable.length > 0 && selectedCount === actionable.length;

  /** 汇总复核通过后才真正提交勾选；取消全选不需要复核。 */
  function commitSelectAll() {
    const updated = new Set(checked);
    actionable.forEach((group) => updated.add(group.groupId));
    setChecked(updated);
    onChange({ selections: buildSelections(groups, updated), groups });
  }

  function toggleAll() {
    if (allSelected) {
      const updated = new Set(checked);
      actionable.forEach((group) => updated.delete(group.groupId));
      setChecked(updated);
      onChange({ selections: buildSelections(groups, updated), groups });
      return;
    }
    setSelectAllOpen(true);
  }

  function toggleGroup(group: SessionLinkPreviewGroup, next: boolean) {
    const updated = new Set(checked);
    if (next) updated.add(group.groupId);
    else updated.delete(group.groupId);
    setChecked(updated);
    onChange({ selections: buildSelections(groups, updated), groups });
  }

  if (unsupported) return null;

  const selectAllSummary = selectAllSummaryText(groups);

  const pending = loading || (!preview && !error);
  const storeUnavailable = preview?.storeStatus === "unavailable";

  return (
    <section className="space-y-3" aria-label="关联会话">
      <AlertDialog open={selectAllOpen} onOpenChange={setSelectAllOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>确认一键全选？</AlertDialogTitle>
            <AlertDialogDescription>{selectAllSummary}。执行前每项仍会重新校验预览凭据，内容已变化会跳过而不是覆盖。</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={disabled}>取消</AlertDialogCancel>
            <AlertDialogAction
              disabled={disabled}
              onClick={() => {
                commitSelectAll();
                setSelectAllOpen(false);
              }}
            >
              确认全选
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <div className="flex items-start gap-3 rounded-md border bg-muted/30 px-3 py-3">
        <span className="flex size-8 shrink-0 items-center justify-center rounded-md border bg-background text-muted-foreground">
          <Link2 className="size-4" />
        </span>
        <div className="min-w-0 space-y-0.5">
          <div className="text-sm font-medium">什么是关联会话？</div>
          <p className="text-xs text-muted-foreground">
            通过本工具复制到其他账号的会话，会自动建立关联。切换时，可选择将当前账号的后续内容同步到对应会话。
          </p>
        </div>
      </div>

      {pending && (
        <div className="space-y-2">
          {progress && progress.total > 0 && (
            <div className="space-y-1">
              <div
                className="h-1 overflow-hidden rounded-full bg-muted"
                role="progressbar"
                aria-label="正在核对关联会话"
                aria-valuemin={0}
                aria-valuemax={progress.total}
                aria-valuenow={progress.done}
              >
                <div
                  className="h-full rounded-full bg-primary transition-[width] duration-150 ease-out"
                  style={{ width: `${Math.round((progress.done / progress.total) * 100)}%` }}
                />
              </div>
              <p className="text-[11px] tabular-nums text-muted-foreground">
                正在核对会话 {progress.done}/{progress.total}
              </p>
            </div>
          )}
          <LinksSkeleton />
        </div>
      )}

      {!pending && (error || storeUnavailable) && (
        <div
          className={cn(
            "flex items-center justify-between gap-3 rounded-md border px-3 py-2.5",
            STATUS_MIN_H,
          )}
        >
          <span className="min-w-0 flex-1 text-xs text-muted-foreground">
            {error ? "暂时无法检查会话，本次不能同步" : "同步记录不可用，本次不会同步"}
          </span>
          <Button
            variant="outline"
            size="sm"
            className="shrink-0"
            disabled={disabled}
            onClick={() => setReloadToken((token) => token + 1)}
          >
            <RotateCw />
            重新检查
          </Button>
        </div>
      )}

      {!pending && !error && preview?.storeStatus === "missing" && (
        <div
          className={cn(
            "flex items-center justify-center rounded-md border px-3 py-2.5",
            STATUS_MIN_H,
          )}
        >
          <p className="text-center text-xs text-muted-foreground">
            还没有可以同步的会话：先在「复制会话」里复制一次，之后切换回来就能在这里同步新内容。
          </p>
        </div>
      )}

      {!pending && !error && !storeUnavailable && preview?.storeStatus === "ready" && groups.length === 0 && (
        <div
          className={cn(
            "flex items-center justify-center rounded-md border px-3 py-2.5",
            STATUS_MIN_H,
          )}
        >
          <p className="text-center text-xs text-muted-foreground">
            这两个账号还没有共同复制过的会话（只处理双方都有的，不涉及其他账号）。
          </p>
        </div>
      )}

      {!pending && !error && groups.length > 0 && (
        <div className="space-y-2">
          <div className="flex items-center gap-2.5 px-1">
            <Checkbox
              checked={allSelected ? true : selectedCount > 0 ? "indeterminate" : false}
              disabled={disabled || actionable.length === 0}
              onCheckedChange={() => toggleAll()}
              aria-label="全选可同步的会话"
            />
            <span className="min-w-0 flex-1 text-sm font-medium">全选可同步的会话</span>
            <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
              {`已选 ${selectedCount} / ${groups.length}`}
            </span>
          </div>
          <div
            className={cn(
              "max-h-[min(24rem,50vh)] divide-y overflow-y-auto rounded-md border",
              LIST_MIN_H,
            )}
          >
            {groups.map((group) => (
              <SessionLinkCard
                key={group.groupId}
                group={group}
                targetLabel={targetLabel}
                checked={checked.has(group.groupId) && isActionable(group)}
                disabled={disabled}
                onToggle={(next) => toggleGroup(group, next)}
              />
            ))}
          </div>
          <p className="px-1 text-xs text-muted-foreground">
            全选是字面全勾：包含正文快进、覆盖、归档与取消归档；点击后会先弹出汇总复核，确认才生效。
          </p>
        </div>
      )}
    </section>
  );
}

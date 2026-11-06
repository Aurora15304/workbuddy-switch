import { useEffect, useRef, useState, type ChangeEvent, type ReactNode } from "react";
import { Archive, FileArchive, Loader2, ShieldAlert, Upload } from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  ARCHIVE_MAX_FILE_BYTES,
  ARCHIVE_MAX_RAW_BYTES,
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
} from "@/lib/archive";
import * as api from "@/lib/api";
import type { ArchiveGroupInfo, ArchiveInspect } from "@/lib/types";
import { cn } from "@/lib/utils";
import { useAccountsStore } from "@/stores/accounts";

/** 待确认的一次导入：来源（桌面为路径、webui 为 base64）+ 清单 + 勾选。 */
interface PendingImport {
  source: { path?: string; data?: string };
  inspect: ArchiveInspect;
  selected: string[];
}

/** 导出 / 导入区的分组小标题。 */
function SectionHeader({
  title,
  description,
  actions,
}: {
  title: string;
  description: string;
  actions?: ReactNode;
}) {
  return (
    <div className="flex min-w-0 flex-col gap-2 px-4 py-3 sm:flex-row sm:items-center sm:justify-between sm:px-5">
      <div className="min-w-0">
        <div className="text-[13px] font-medium leading-4">{title}</div>
        <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">{description}</p>
      </div>
      {actions && <div className="flex shrink-0 flex-wrap items-center gap-2">{actions}</div>}
    </div>
  );
}

/** 一行数据组：复选框 + 标题 + 说明 + 文件数 / 体积（含凭据的红标）。 */
function GroupRow({
  group,
  checked,
  onToggle,
}: {
  group: ArchiveGroupInfo;
  checked: boolean;
  onToggle: () => void;
}) {
  const empty = group.files <= 0;
  // 单文件超过 core 上限：整组必然导不出去，直接禁用比让用户吃 400 好。
  const tooLarge = isGroupTooLarge(group);
  const disabled = empty || tooLarge;
  const inputId = `archive-group-${group.id}`;
  return (
    <div
      className={cn(
        "flex min-w-0 items-start gap-2.5 px-4 py-2.5 sm:px-5",
        disabled ? "opacity-50" : "hover:bg-foreground/[0.03]",
      )}
    >
      <Checkbox
        id={inputId}
        className="mt-0.5"
        checked={checked}
        disabled={disabled}
        onCheckedChange={onToggle}
        aria-label={group.title}
      />
      <label
        htmlFor={inputId}
        className={cn("min-w-0 flex-1", disabled ? "cursor-not-allowed" : "cursor-pointer")}
      >
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          <span className="text-[13px] font-medium leading-4">{group.title}</span>
          {group.sensitive && (
            <Badge variant="destructive" className="h-4 px-1.5 text-[10px] leading-none">
              <ShieldAlert className="size-2.5" />
              含登录凭据
            </Badge>
          )}
        </div>
        <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">{group.description}</p>
        <p className="mt-0.5 text-[11px] leading-4 text-muted-foreground/60">
          {empty
            ? "暂无可打包内容"
            : tooLarge
              ? `单文件超过 ${formatBytes(ARCHIVE_MAX_FILE_BYTES)}，无法打包`
              : `${group.files} 个文件 · ${formatBytes(group.bytes)}`}
        </p>
      </label>
    </div>
  );
}

/**
 * 数据迁移：按组勾选导出 / 导入整包 zip。
 *
 * 两个宿主的差异只在「文件怎么给」：桌面端走系统对话框拿绝对路径、后端直接写盘；
 * webui 没有文件系统，导出整包以 base64 回传后由浏览器下载，导入则把选中的 zip
 * 读成 base64 发给后端。勾选逻辑与文案两侧完全一致。
 */
export default function DataMigrationCard() {
  const refreshAccounts = useAccountsStore((s) => s.fetchAll);
  const [groups, setGroups] = useState<ArchiveGroupInfo[] | null>(null);
  const [groupsError, setGroupsError] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  const [exporting, setExporting] = useState(false);
  const [inspecting, setInspecting] = useState(false);
  const [importing, setImporting] = useState(false);
  const [pending, setPending] = useState<PendingImport | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  /** 首次加载才套默认全选：之后重载（导入完刷新）不能覆盖用户手动改过的勾选。 */
  const selectionInitialized = useRef(false);

  useEffect(() => {
    void loadGroups();
  }, []);

  async function loadGroups() {
    setGroupsError("");
    try {
      const res = await api.archiveGroups();
      setGroups(res.groups);
      setSelected((prev) => {
        if (!selectionInitialized.current) {
          selectionInitialized.current = true;
          return defaultArchiveSelection(res.groups);
        }
        const ids = res.groups.map((group) => group.id);
        return prev.filter((id) => ids.includes(id));
      });
    } catch (e) {
      setGroups(null);
      setGroupsError(api.asError(e));
    }
  }

  function toggleGroup(id: string, checked: boolean) {
    setSelected((prev) => {
      if (checked) return prev.includes(id) ? prev : [...prev, id];
      return prev.filter((item) => item !== id);
    });
  }

  /** 超限时的一键解救：从最大的组开始撤，撤了什么明确说出来（默认勾选不隐式裁剪）。 */
  function autoTrimSelection() {
    if (!groups) return;
    const { selected: next, removed } = trimSelectionToLimit(groups, selected);
    if (removed.length === 0) return;
    setSelected(next);
    const dropped = removed.map((id) => groups.find((item) => item.id === id));
    const bytes = dropped.reduce((sum, group) => sum + (group?.bytes ?? 0), 0);
    const label =
      dropped.length === 1
        ? `${dropped[0]?.title ?? removed[0]}（${formatBytes(bytes)}）`
        : `${dropped.map((group, i) => group?.title ?? removed[i]).join("、")}（共 ${formatBytes(bytes)}）`;
    toast.success(
      next.length > 0
        ? `已取消 ${label}，其余分组仍勾选，可直接导出`
        : `已取消 ${label}，它单独就顶到了 64 MB 上限`,
      {
        description:
          next.length > 0
            ? "被取消的组随时可以重新勾上。"
            : "请勾选其它更小的分组再导出。",
      },
    );
  }

  const accountsSelected = selected.includes("accounts");
  const summary = groups ? summarizeSelection(groups, selected) : { files: 0, bytes: 0 };
  // 勾选内容超过 core 的导出上限时后端必然拒绝，提前把按钮关掉并说明原因。
  const overLimit = groups ? isExportOverLimit(groups, selected) : false;

  async function doExport() {
    if (exporting || selected.length === 0 || overLimit) return;
    setExporting(true);
    try {
      if (api.isWebui()) {
        const res = await api.archiveExport(selected);
        if (!res.data) throw new Error("导出内容为空");
        downloadArchive(res.fileName, res.data);
        toast.success(`已导出 ${res.groups.length} 组数据`, {
          description: `${res.fileName} · ${formatBytes(res.bytes)}`,
        });
      } else {
        // 桌面端：系统保存对话框给绝对路径，由后端写盘（WebView 不支持 <a download>）。
        const { save } = await import("@tauri-apps/plugin-dialog");
        const path = await save({
          title: "导出数据备份",
          defaultPath: archiveBackupFileName(),
          filters: [{ name: "zip", extensions: ["zip"] }],
        });
        if (!path) return;
        const res = await api.archiveExport(selected, path);
        toast.success(`已导出 ${res.groups.length} 组数据`, {
          description: `${res.path ?? path} · ${formatBytes(res.bytes)}`,
        });
      }
    } catch (e) {
      toast.error("导出失败", { description: api.asError(e) });
    } finally {
      setExporting(false);
    }
  }

  /** 选文件：桌面端用系统打开对话框拿路径，webui 交给隐藏的 file input。 */
  async function pickArchive() {
    if (inspecting) return;
    if (api.isWebui()) {
      fileInputRef.current?.click();
      return;
    }
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const picked = await open({
        title: "选择数据备份",
        multiple: false,
        directory: false,
        filters: [{ name: "zip", extensions: ["zip"] }],
      });
      const path = Array.isArray(picked) ? picked[0] : picked;
      if (!path) return;
      await inspectArchive({ path });
    } catch (e) {
      toast.error("无法读取备份", { description: api.asError(e) });
    }
  }

  /** webui：把选中的 zip 读成 base64 再交给后端解析。 */
  async function onFileInputChange(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    // 先清空 value，否则再次选择同一个文件不会触发 change。
    event.target.value = "";
    if (!file) return;
    // 64 MiB 是 core 的解压总量上限：zip 本身超它，解压后必然也超（deflate 只会微膨胀），
    // 发过去一定被拒，干脆本地拦掉，省一次上传。
    const sizeCheck = checkArchiveZipSize(file.size);
    if (sizeCheck === "block") {
      toast.error("备份超过 64 MB 上限", {
        description: "请减少导出时勾选的分组，重新导出一个更小的包再导入。",
      });
      return;
    }
    if (sizeCheck === "warn") {
      toast.warning("这个备份接近 64 MB 上限", {
        description: "如果导入被拒，请减少导出时勾选的分组后重新导出。",
      });
    }
    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      await inspectArchive({ data: bytesToBase64(bytes) });
    } catch (e) {
      toast.error("无法读取备份", { description: api.asError(e) });
    }
  }

  /** 解析清单后弹确认框；默认勾选备份里有内容的组。 */
  async function inspectArchive(source: { path?: string; data?: string }) {
    setInspecting(true);
    try {
      const inspect = await api.archiveInspect(source);
      setPending({
        source,
        inspect,
        selected: defaultArchiveSelection(inspect.groups),
      });
    } catch (e) {
      toast.error("这个备份读不出来", { description: api.asError(e) });
    } finally {
      setInspecting(false);
    }
  }

  function togglePendingGroup(id: string, checked: boolean) {
    setPending((prev) => {
      if (!prev) return prev;
      const next = checked
        ? [...prev.selected, id]
        : prev.selected.filter((item) => item !== id);
      return { ...prev, selected: next };
    });
  }

  async function doImport() {
    if (!pending || importing || pending.selected.length === 0) return;
    setImporting(true);
    try {
      const res = await api.archiveImport(pending.source, pending.selected);
      setPending(null);
      toast.success(
        `已导入 ${res.groups.length} 组，新增 ${res.accountsAdded} 个账号，更新 ${res.accountsUpdated} 个`,
        { description: `导入前备份在 ${res.backupDir}`, duration: 12_000 },
      );
      // 账号库与各组体积都变了：重拉账号列表与组清单。
      void refreshAccounts();
      void loadGroups();
    } catch (e) {
      // core 的失败文案自带「已回滚 / 备份目录」，一字不改放进 description：
      // 标题只做分类（与导出、与全仓其它 toast 的约定一致），绝对路径也不会把标题挤成两三行。
      const message = api.asError(e);
      // 413 时 axum 回的是 text/plain，`httpCall` 的 `res.json()` 解析失败只剩状态码，
      // 所以实际能命中的是「请求失败 (413)」这一句。
      const tooLarge = /\(413\)|length limit exceeded|payload too large|body limit/i.test(message);
      const hint = tooLarge ? " —— 备份包过大，无法上传，请减少导出时勾选的分组后重试。" : "";
      toast.error("导入失败", { description: `${message}${hint}`, duration: 15_000 });
    } finally {
      setImporting(false);
    }
  }

  const pendingSensitive = (pending?.selected ?? []).includes("accounts");

  return (
    <section className="min-w-0 space-y-2.5" aria-labelledby="settings-data-migration">
      <div className="px-1">
        <h2 id="settings-data-migration" className="text-[13px] font-medium leading-5">
          数据迁移
        </h2>
      </div>
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardContent className="space-y-0 p-0">
          {/* 导出 */}
          <SectionHeader
            title="导出备份"
            description="勾选要带走的数据组，打包成一个 zip。换机或重装后可从「导入备份」还原。"
            actions={
              groups && groups.length > 0 ? (
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={exporting}
                  onClick={() => setSelected(toggleAllArchiveSelection(groups, selected))}
                >
                  {isAllArchiveSelected(groups, selected) ? "全不选" : "全选"}
                </Button>
              ) : null
            }
          />

          {groupsError ? (
            <div className="border-t border-border/50 px-4 py-3 text-xs text-muted-foreground sm:px-5">
              <span>数据组读取失败：{groupsError}</span>
              <Button size="sm" variant="ghost" className="ml-1 h-6 px-1.5" onClick={() => void loadGroups()}>
                重试
              </Button>
            </div>
          ) : !groups ? (
            <p className="border-t border-border/50 px-4 py-3 text-xs text-muted-foreground sm:px-5">
              正在读取可备份的内容…
            </p>
          ) : groups.length === 0 ? (
            <p className="border-t border-border/50 px-4 py-3 text-xs text-muted-foreground sm:px-5">
              本机还没有可备份的数据。
            </p>
          ) : (
            <div className="divide-y divide-border/40 border-t border-border/50">
              {groups.map((group) => (
                <GroupRow
                  key={group.id}
                  group={group}
                  checked={selected.includes(group.id)}
                  onToggle={() => toggleGroup(group.id, !selected.includes(group.id))}
                />
              ))}
            </div>
          )}

          {accountsSelected && (
            <Alert variant="warning" className="mx-4 my-3 sm:mx-5">
              <ShieldAlert />
              <AlertDescription>
                zip 里是<b>明文登录凭据</b>，等同密码，别外传、别放网盘或聊天群。
              </AlertDescription>
            </Alert>
          )}

          <div className="flex flex-wrap items-center justify-between gap-2 border-t border-border/50 px-4 py-3 sm:px-5">
            <span
              className={cn(
                "min-w-0 flex-1 text-xs",
                overLimit ? "text-destructive" : "text-muted-foreground",
              )}
            >
              {selected.length === 0
                ? "至少勾选一组才能导出"
                : overLimit
                  ? `已选 ${formatBytes(summary.bytes)}，加上打包开销会超过 ${formatBytes(ARCHIVE_MAX_RAW_BYTES)} 上限，请取消勾选部分分组`
                  : `已选 ${selected.length} 组 · ${summary.files} 个文件 · ${formatBytes(summary.bytes)}`}
            </span>
            <div className="flex shrink-0 items-center gap-1.5">
              {overLimit && (
                <Button size="sm" variant="ghost" className="h-8 px-2" onClick={autoTrimSelection}>
                  自动取消部分分组
                </Button>
              )}
              <Button
                size="sm"
                onClick={() => void doExport()}
                disabled={exporting || selected.length === 0 || overLimit}
              >
                {exporting ? <Loader2 className="animate-spin" /> : <Archive />}
                {exporting ? "正在打包…" : "导出 zip"}
              </Button>
            </div>
          </div>

          {/* 导入 */}
          <div className="border-t border-border/50 bg-muted/25">
            <SectionHeader
              title="导入备份"
              description="选择之前导出的 zip，确认里面有什么之后再决定恢复哪几组。导入前会自动备份当前数据。"
              actions={
                <Button size="sm" variant="outline" onClick={() => void pickArchive()} disabled={inspecting}>
                  {inspecting ? <Loader2 className="animate-spin" /> : <Upload />}
                  {inspecting ? "读取中…" : "选择 zip 文件"}
                </Button>
              }
            />
            <input
              ref={fileInputRef}
              type="file"
              accept=".zip,application/zip"
              className="hidden"
              onChange={(event) => void onFileInputChange(event)}
            />
            <p className="border-t border-border/50 px-4 py-2.5 text-[11px] leading-4 text-muted-foreground/75 sm:px-5">
              <FileArchive className="mr-1 inline size-3 align-[-2px]" />
              只接受本工具导出的 zip；导入不会删除本机其它账号，同 uid 的账号会被备份里的记录覆盖。
            </p>
          </div>
        </CardContent>
      </Card>

      <Dialog open={pending !== null} onOpenChange={(open) => (!open ? setPending(null) : undefined)}>
        <DialogContent className="min-w-0">
          <DialogHeader>
            <DialogTitle>导入数据备份</DialogTitle>
            <DialogDescription>
              导出时间 {formatArchiveTime(pending?.inspect.exportedAt ?? 0)} · 来源版本 v
              {pending?.inspect.appVersion || "未知"} · 格式 v{pending?.inspect.formatVersion ?? "?"}
            </DialogDescription>
          </DialogHeader>

          {pending && pending.inspect.groups.length === 0 ? (
            <p className="py-2 text-sm text-muted-foreground">这个备份里没有可导入的数据组。</p>
          ) : (
            <div className="max-h-64 min-w-0 overflow-y-auto rounded-lg border">
              {pending?.inspect.groups.map((group) => {
                const checked = pending.selected.includes(group.id);
                const inputId = `import-group-${group.id}`;
                return (
                  <div
                    key={group.id}
                    className="flex items-start gap-2.5 border-b border-border/40 px-3 py-2.5 last:border-b-0 hover:bg-foreground/[0.03]"
                  >
                    <Checkbox
                      id={inputId}
                      className="mt-0.5"
                      checked={checked}
                      onCheckedChange={() => togglePendingGroup(group.id, !checked)}
                      aria-label={group.title}
                    />
                    <label htmlFor={inputId} className="min-w-0 flex-1 cursor-pointer">
                      <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                        <span className="text-[13px] font-medium leading-4">{group.title}</span>
                        {group.sensitive && (
                          <Badge variant="destructive" className="h-4 px-1.5 text-[10px] leading-none">
                            含登录凭据
                          </Badge>
                        )}
                      </div>
                      <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">
                        {group.description}
                      </p>
                      <p className="mt-0.5 text-[11px] leading-4 text-muted-foreground/60">
                        {group.files} 个文件 · {formatBytes(group.bytes)}
                        {group.id === "accounts" && " · 同 uid 覆盖、新账号追加"}
                      </p>
                    </label>
                  </div>
                );
              })}
            </div>
          )}

          {pendingSensitive && (
            <Alert variant="warning">
              <ShieldAlert />
              <AlertDescription>
                将写入备份里的登录凭据，覆盖本机同 uid 账号的登录态；新账号会追加到账号库。
              </AlertDescription>
            </Alert>
          )}

          <DialogFooter>
            <Button variant="outline" onClick={() => setPending(null)} disabled={importing}>
              取消
            </Button>
            <Button
              onClick={() => void doImport()}
              disabled={importing || (pending?.selected.length ?? 0) === 0}
            >
              {importing ? <Loader2 className="animate-spin" /> : <Upload />}
              {importing ? "正在导入…" : `导入选中的 ${pending?.selected.length ?? 0} 组`}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}

/** webui 导出：base64 → Blob → 浏览器下载。 */
function downloadArchive(fileName: string, base64: string) {
  const blob = new Blob([base64ToBytes(base64)], { type: "application/zip" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = fileName;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
}

import { useCallback, useEffect, useState } from "react";
import { Loader2, RefreshCw } from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import * as api from "@/lib/api";
import { displayName } from "@/lib/account-display";
import type { AccountMeta, TaskCenterItem, TaskCenterList } from "@/lib/types";

/**
 * 成长任务（beta）：只做真实的列表 / 接受 / 领取。
 *
 * 需要真实行为才能达成的任务（对话、邀请等）只显示进度，不会被代劳——
 * 本页不向上游上报任何行为事件。
 */
export default function TaskCenterPage() {
  const [accounts, setAccounts] = useState<AccountMeta[]>([]);
  const [accountId, setAccountId] = useState("");
  const [list, setList] = useState<TaskCenterList | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    void api
      .getAccounts()
      .then((res) => {
        setAccounts(res.accounts);
        if (res.accounts.length > 0) setAccountId(res.accounts[0].id);
      })
      .catch((e) => setError(api.asError(e)));
  }, []);

  const load = useCallback(
    async (id: string) => {
      if (!id) return;
      setLoading(true);
      setError("");
      try {
        setList(await api.listTaskCenter(id));
      } catch (e) {
        setError(api.asError(e));
        setList(null);
      } finally {
        setLoading(false);
      }
    },
    [],
  );

  useEffect(() => {
    if (accountId) void load(accountId);
  }, [accountId, load]);

  async function acceptAll() {
    if (!accountId) return;
    setBusy(true);
    try {
      const res = await api.acceptTaskCenter(accountId);
      const failed = res.failed ?? [];
      if (res.error) {
        toast.error("接受任务失败", { description: res.error });
      } else if (res.accepted.length === 0) {
        toast.info(res.note ?? "没有需要接受的任务");
      } else if (failed.length > 0) {
        // 外层成功但个别任务被判 not found，必须如实说明，不能报「全部成功」。
        toast.success(`已接受 ${res.accepted.length} 个，${failed.length} 个被上游拒绝`, {
          description: failed.map((f) => `${f.code}：${f.reason}`).join("；"),
        });
      } else {
        toast.success(`已接受 ${res.accepted.length} 个任务`);
      }
      await load(accountId);
    } catch (e) {
      toast.error("接受任务失败", { description: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  async function claimAll() {
    if (!accountId) return;
    setBusy(true);
    try {
      const res = await api.claimTaskCenter(accountId);
      if (res.error) {
        toast.error("领取奖励失败", { description: res.error });
      } else {
        if (res.claimed.length > 0) {
          toast.success(`已领取 ${res.claimed.length} 个奖励`, {
            description: `共 ${res.creditTotal ?? 0} 积分`,
          });
        } else {
          toast.info("没有可领取的奖励", {
            description: res.skipped.length > 0 ? "未达标的任务需要先在客户端里完成任务" : undefined,
          });
        }
        if (res.errors.length > 0) {
          toast.error(`${res.errors.length} 个奖励领取失败`, {
            description: res.errors.map((item) => `${item.title ?? item.code}：${item.error}`).join("；"),
          });
        }
      }
      await load(accountId);
    } catch (e) {
      toast.error("领取奖励失败", { description: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  const tasks = list?.tasks ?? [];
  const claimable = tasks.filter((task) => task.claimable).length;

  return (
    <div className="mx-auto w-full max-w-4xl space-y-4 p-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <div className="flex items-center gap-2">
            <h1 className="text-xl font-semibold">成长任务</h1>
            <Badge variant="secondary">beta</Badge>
          </div>
          <p className="mt-1 text-sm text-muted-foreground">
            列出当前账号的全部成长任务，可一键接受与领取已达标的奖励。需要真实行为的任务只显示进度，不会被代劳。
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Select value={accountId} onValueChange={setAccountId}>
            <SelectTrigger className="w-56">
              <SelectValue placeholder="选择账号" />
            </SelectTrigger>
            <SelectContent>
              {accounts.map((account) => (
                <SelectItem key={account.id} value={account.id}>
                  {displayName(account)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button variant="outline" size="sm" onClick={() => void load(accountId)} disabled={loading || busy}>
            {loading ? <Loader2 className="size-4 animate-spin" /> : <RefreshCw className="size-4" />}
            刷新
          </Button>
        </div>
      </div>

      {error && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">无法读取任务</CardTitle>
            <CardDescription>{error}</CardDescription>
          </CardHeader>
        </Card>
      )}

      {list?.error && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">无法读取任务</CardTitle>
            <CardDescription>{list.error}</CardDescription>
          </CardHeader>
        </Card>
      )}

      {list?.errors && list.errors.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">部分口径取不到</CardTitle>
            <CardDescription>{list.errors.join("；")}</CardDescription>
          </CardHeader>
        </Card>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <Button onClick={() => void acceptAll()} disabled={busy || !accountId}>
          一键接受任务
        </Button>
        <Button variant="secondary" onClick={() => void claimAll()} disabled={busy || !accountId}>
          一键领取奖励{claimable > 0 ? `（${claimable}）` : ""}
        </Button>
        {busy && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">任务列表</CardTitle>
          <CardDescription>
            共 {tasks.length} 项，可领取 {claimable} 项。未接受的任务上游不返回进度——先点「一键接受任务」，之后才会开始计数。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-1">
          {loading && tasks.length === 0 && (
            <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" />
              正在读取…
            </div>
          )}
          {!loading && tasks.length === 0 && !list?.error && (
            <p className="py-6 text-sm text-muted-foreground">该账号暂无可显示的成长任务。</p>
          )}
          {tasks.map((task) => (
            <TaskRow key={task.code} task={task} />
          ))}
        </CardContent>
      </Card>
    </div>
  );
}

function TaskRow({ task }: { task: TaskCenterItem }) {
  const notAccepted = task.acceptStatus === "not_accepted" || task.acceptStatus === "";
  const progress = task.progressKnown ? `${task.current}/${task.target}` : "—";
  const state = task.claimed
    ? "已领取"
    : task.claimable
      ? "可领取"
      : notAccepted
        ? "未接受"
        : task.progressKnown
          ? "进行中"
          : "暂无进度";

  return (
    <div className="flex items-start justify-between gap-3 rounded-md px-2 py-2 hover:bg-foreground/[0.03]">
      <div className="min-w-0">
        <div className="truncate text-sm font-medium">{task.title}</div>
        {task.hint && <div className="truncate text-xs text-muted-foreground">{task.hint}</div>}
      </div>
      <div className="flex shrink-0 items-center gap-3 text-xs text-muted-foreground">
        <span className="tabular-nums">{progress}</span>
        {task.credit > 0 && <span className="tabular-nums">+{task.credit} 积分</span>}
        <Badge variant={task.claimable ? "default" : "secondary"}>{state}</Badge>
      </div>
    </div>
  );
}

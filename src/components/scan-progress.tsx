/**
 * 扫描进度条：分母必须是后端枚举出的真实总数（文件数 / 组数），
 * 前端不得按时间或已加载条数估算百分比。
 *
 * `total <= 0` 时不渲染——后端首帧到达前保留骨架，避免闪一下 0/0。
 */
export function ScanProgress({
  done,
  total,
  label,
  className,
}: {
  done: number;
  total: number;
  /** 进度文案前缀，例如「正在扫描会话文件」。渲染为「前缀 done/total」。 */
  label: string;
  className?: string;
}) {
  if (total <= 0) return null;
  const percent = Math.min(100, Math.round((done / total) * 100));
  return (
    <div className={className ? `space-y-1 ${className}` : "space-y-1"}>
      <div
        className="h-1 overflow-hidden rounded-full bg-muted"
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={total}
        aria-valuenow={done}
      >
        <div
          className="h-full rounded-full bg-primary transition-[width] duration-150 ease-out"
          style={{ width: `${percent}%` }}
        />
      </div>
      <p className="text-[11px] tabular-nums text-muted-foreground">
        {label} {done}/{total}
      </p>
    </div>
  );
}

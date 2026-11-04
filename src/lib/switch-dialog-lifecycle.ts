/** 一帧 webui 切号进度（与后端 `switch_progress` 返回同形）。 */
export interface ProgressFrame {
  running: boolean;
  progress: string | null;
}

/** 轮询所需外部能力，全部由调用方注入：核心生命周期不依赖 DOM / Tauri / 时钟。 */
export interface ProgressPollerDeps {
  fetchProgress: () => Promise<ProgressFrame>;
  onMessage: (message: string) => void;
  intervalMs: number;
  setInterval: (cb: () => void, ms: number) => number;
  clearInterval: (id: number) => void;
}

/**
 * 把一帧进度归一化为「应写入 UI 的文案」，可丢弃时返回 null。
 *
 * 三类帧必须丢弃：代际失效（订阅卸载后才到达）、running=false（本次切号已结束，
 * 残留文案不能覆盖新状态）、空文案。空白裁剪在边界一次完成，消费方只拿干净文案。
 */
export function resolveSwitchProgressMessage(
  frame: ProgressFrame,
  isCurrentGeneration = true,
): string | null {
  if (!isCurrentGeneration) return null;
  if (!frame.running) return null;
  const message = frame.progress?.trim();
  return message ? message : null;
}

/**
 * 切号进行中禁止关闭弹窗：后端 spawn_blocking 不可取消，关掉只会让 UI 与后端
 * 状态脱节，并在重开时残留「正在处理中」。空闲时放行正常关闭。
 */
export function shouldBlockDismiss(busy: boolean): boolean {
  return busy;
}

/**
 * webui 切号进度轮询：返回幂等 dispose。
 *
 * 三条边界由模块内部消化，React effect 只需在 `open && busy` 时创建、卸载时 dispose：
 * - 每帧判 running / 代际 / 空文案，陈旧帧不覆盖 UI；
 * - 请求失败静默保留上一帧，不产生 unhandledrejection；
 * - dispose 后停止定时器，且已发出的请求迟到返回时不再 setState。
 */
export function createSwitchProgressPoller(deps: ProgressPollerDeps): () => void {
  let disposed = false;
  const timer = deps.setInterval(() => {
    void deps
      .fetchProgress()
      .then((frame) => {
        const message = resolveSwitchProgressMessage(frame, !disposed);
        if (message) deps.onMessage(message);
      })
      .catch(() => {
        // 进度是尽力而为：服务不可达时保留上一帧文案，不影响切号本身。
      });
  }, deps.intervalMs);

  return () => {
    if (disposed) return;
    disposed = true;
    deps.clearInterval(timer);
  };
}

import { describe, expect, it, vi } from "vitest";

import {
  createSwitchProgressPoller,
  resolveSwitchProgressMessage,
  shouldBlockDismiss,
  type ProgressFrame,
  type ProgressPollerDeps,
} from "./switch-dialog-lifecycle";

/** 让所有已排队的 microtask（含 .then / .catch 链）跑完。 */
const flushMicrotasks = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

/** 手工驱动的定时器：把轮询回调握在手里，由测试决定何时触发一帧。 */
function fakeTimers() {
  let tick: (() => void) | undefined;
  const clearInterval = vi.fn((_id: number) => {});
  const setInterval = vi.fn((cb: () => void, _ms: number) => {
    tick = cb;
    return 42;
  });
  return {
    setInterval,
    clearInterval,
    fire: () => {
      if (!tick) throw new Error("轮询未注册定时器");
      tick();
    },
  };
}

function makePoller(fetchProgress: () => Promise<ProgressFrame>) {
  const timers = fakeTimers();
  const onMessage = vi.fn((_message: string) => {});
  const dispose = createSwitchProgressPoller({
    fetchProgress,
    onMessage,
    intervalMs: 600,
    setInterval: timers.setInterval,
    clearInterval: timers.clearInterval,
  } satisfies ProgressPollerDeps);
  return { timers, onMessage, dispose };
}

describe("resolveSwitchProgressMessage — 切号进度帧归一化", () => {
  const cases: {
    name: string;
    frame: ProgressFrame;
    isCurrent?: boolean;
    expected: string | null;
  }[] = [
    { name: "运行中的有效帧返回原文案", frame: { running: true, progress: "正在备份…" }, expected: "正在备份…" },
    { name: "文案两端空白被裁剪", frame: { running: true, progress: "  正在备份…  " }, expected: "正在备份…" },
    { name: "running=false 的已停止帧忽略", frame: { running: false, progress: "上一轮残留" }, expected: null },
    { name: "progress=null 忽略", frame: { running: true, progress: null }, expected: null },
    { name: "空文案忽略", frame: { running: true, progress: "" }, expected: null },
    { name: "纯空白文案忽略", frame: { running: true, progress: "   " }, expected: null },
    {
      name: "代际失效的帧忽略（卸载后到达）",
      frame: { running: true, progress: "陈旧帧" },
      isCurrent: false,
      expected: null,
    },
  ];

  for (const c of cases) {
    it(c.name, () => {
      expect(resolveSwitchProgressMessage(c.frame, c.isCurrent)).toBe(c.expected);
    });
  }
});

describe("shouldBlockDismiss — busy 期间拦截关闭", () => {
  it("切号进行中拦截 ESC / 遮罩关闭", () => {
    expect(shouldBlockDismiss(true)).toBe(true);
  });

  it("空闲时放行正常关闭", () => {
    expect(shouldBlockDismiss(false)).toBe(false);
  });
});

describe("createSwitchProgressPoller — webui 进度轮询生命周期", () => {
  it("按 intervalMs 注册定时器，正常运行帧写入 UI", async () => {
    const { timers, onMessage, dispose } = makePoller(async () => ({
      running: true,
      progress: "正在备份…",
    }));
    expect(timers.setInterval).toHaveBeenCalledWith(expect.any(Function), 600);

    timers.fire();
    await flushMicrotasks();

    expect(onMessage).toHaveBeenCalledWith("正在备份…");
    dispose();
  });

  const ignoredFrames: ProgressFrame[] = [
    { running: false, progress: "上一轮残留" },
    { running: true, progress: "" },
    { running: true, progress: null },
    { running: true, progress: "   " },
  ];
  for (const frame of ignoredFrames) {
    it(`忽略无效帧 ${JSON.stringify(frame)}`, async () => {
      const { timers, onMessage, dispose } = makePoller(async () => frame);
      timers.fire();
      await flushMicrotasks();
      expect(onMessage).not.toHaveBeenCalled();
      dispose();
    });
  }

  it("dispose 停止 600ms 轮询且幂等", () => {
    const { timers, dispose } = makePoller(async () => ({ running: true, progress: "x" }));

    dispose();
    dispose();

    expect(timers.clearInterval).toHaveBeenCalledTimes(1);
    expect(timers.clearInterval).toHaveBeenCalledWith(42);
  });

  it("dispose 之后迟到的帧不再写 UI（卸载后 setState 防护）", async () => {
    const pending = deferred<ProgressFrame>();
    const { timers, onMessage, dispose } = makePoller(() => pending.promise);

    timers.fire();
    dispose();
    pending.resolve({ running: true, progress: "迟到帧" });
    await flushMicrotasks();

    expect(onMessage).not.toHaveBeenCalled();
  });

  it("服务不可达时静默跳过，不产生未处理拒绝", async () => {
    const { timers, onMessage, dispose } = makePoller(async () => {
      throw new Error("服务不可达");
    });

    expect(() => timers.fire()).not.toThrow();
    await flushMicrotasks();

    expect(onMessage).not.toHaveBeenCalled();
    dispose();
  });
});

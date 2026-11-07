/**
 * 镜像切换的风险确认记忆：**首次使用需要勾选确认风险，之后不再要求**。
 *
 * 只记「用户已知风险」这一件事，不记任何账号 / 会话数据；放在 localStorage 里，
 * 换机器或清掉缓存即失效，退回首次确认。写入失败不影响本次操作，只是下次再确认一次。
 *
 * 本模块是纯函数 + 一个 localStorage 薄封装：node 环境（测试）没有 window 时
 * 一律按「未确认」处理，不会让测试崩掉。
 */

const STORAGE_KEY = "wb-switch-mirror-risk-acknowledged";

/** 是否已经确认过镜像切换的风险。localStorage 不可用时一律按未确认处理。 */
export function hasMirrorRiskAcknowledged(): boolean {
  try {
    return window.localStorage.getItem(STORAGE_KEY) === "1";
  } catch {
    return false;
  }
}

/** 记住「用户已知镜像切换的风险」。写不进去只是下次再确认一次，不报错。 */
export function rememberMirrorRiskAcknowledged(): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, "1");
  } catch {
    /* 忽略：持久化失败不影响本次操作 */
  }
}

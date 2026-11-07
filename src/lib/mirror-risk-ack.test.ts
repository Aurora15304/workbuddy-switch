import { describe, expect, it } from "vitest";

import {
  hasMirrorRiskAcknowledged,
  rememberMirrorRiskAcknowledged,
} from "@/lib/mirror-risk-ack";

describe("镜像切换风险确认记忆", () => {
  it("没有 localStorage 时按未确认处理，绝不抛错", () => {
    expect(hasMirrorRiskAcknowledged()).toBe(false);
  });

  it("写入失败也不影响流程（只是下次再确认一次）", () => {
    // node 环境没有 localStorage：写入应当静默失败，不抛错、不阻断镜像启用。
    expect(() => rememberMirrorRiskAcknowledged()).not.toThrow();
  });
});

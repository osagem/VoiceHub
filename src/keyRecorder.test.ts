import { describe, expect, it } from "vitest";
import { recordKeyEvent } from "./keyRecorder";

function keyEvent(type: "keydown" | "keyup", init: { code: string; key: string; keyCode?: number; ctrlKey?: boolean; altKey?: boolean; shiftKey?: boolean }): KeyboardEvent {
  const event = new KeyboardEvent(type, { code: init.code, key: init.key, ctrlKey: init.ctrlKey, altKey: init.altKey, shiftKey: init.shiftKey });
  if (init.keyCode != null) {
    // jsdom 的 KeyboardEvent.keyCode/which 是只读 0，录制器读的是它们——覆盖。
    Object.defineProperty(event, "keyCode", { value: init.keyCode });
    Object.defineProperty(event, "which", { value: init.keyCode });
  }
  return event;
}

describe("key recorder", () => {
  it("records a lone right Alt on keyup (Doubao adaptation case)", () => {
    const down = recordKeyEvent(keyEvent("keydown", { code: "AltRight", key: "Alt" }), null);
    expect(down).toEqual({ kind: "pending", vk: 0xa5 });
    const up = recordKeyEvent(keyEvent("keyup", { code: "AltRight", key: "Alt" }), 0xa5);
    expect(up).toEqual({ kind: "done", vk: 0xa5, modifiers: 0 });
  });

  it("left and right modifiers map to distinct VKs", () => {
    const cases: Array<[string, number]> = [
      ["ControlLeft", 0xa2], ["ControlRight", 0xa3],
      ["ShiftLeft", 0xa0], ["ShiftRight", 0xa1],
      ["AltLeft", 0xa4], ["AltRight", 0xa5],
    ];
    for (const [code, vk] of cases) {
      expect(recordKeyEvent(keyEvent("keydown", { code, key: "?" }), null)).toEqual({ kind: "pending", vk });
    }
  });

  it("a main key after a modifier confirms the combo and voids the pending single key", () => {
    const ctrlDown = recordKeyEvent(keyEvent("keydown", { code: "ControlLeft", key: "Control" }), null);
    expect(ctrlDown).toEqual({ kind: "pending", vk: 0xa2 });
    const j = recordKeyEvent(
      keyEvent("keydown", { code: "KeyJ", key: "j", keyCode: 0x4a, ctrlKey: true, altKey: true }),
      0xa2,
    );
    expect(j).toEqual({ kind: "done", vk: 0x4a, modifiers: 2 | 1 });
    // 组合键确认后暂存作废：Ctrl 松开不再录出单键。
    const ctrlUp = recordKeyEvent(keyEvent("keyup", { code: "ControlLeft", key: "Control" }), null);
    expect(ctrlUp).toEqual({ kind: "none" });
  });

  it("keyup of a different key never confirms the pending modifier", () => {
    recordKeyEvent(keyEvent("keydown", { code: "AltRight", key: "Alt" }), null);
    const other = recordKeyEvent(keyEvent("keyup", { code: "AltLeft", key: "Alt" }), 0xa5);
    expect(other).toEqual({ kind: "none" });
  });

  it("keydown without a key code is ignored (defensive)", () => {
    expect(recordKeyEvent(keyEvent("keydown", { code: "KeyZ", key: "z" }), null)).toEqual({ kind: "none" });
  });
});

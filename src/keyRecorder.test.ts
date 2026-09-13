import { describe, expect, it } from "vitest";
import { createKeyRecorder } from "./keyRecorder";

function keyEvent(type: "keydown" | "keyup", init: { code: string; key: string; keyCode?: number; ctrlKey?: boolean; altKey?: boolean; shiftKey?: boolean }): KeyboardEvent {
  const event = new KeyboardEvent(type, { code: init.code, key: init.key, ctrlKey: init.ctrlKey, altKey: init.altKey, shiftKey: init.shiftKey });
  if (init.keyCode != null) {
    // jsdom 的 KeyboardEvent.keyCode/which 是只读 0，录制器读的是它们——覆盖。
    Object.defineProperty(event, "keyCode", { value: init.keyCode });
    Object.defineProperty(event, "which", { value: init.keyCode });
  }
  return event;
}

function capture(results: Array<{ vk: number; modifiers: number }>) {
  return createKeyRecorder((vk, modifiers) => results.push({ vk, modifiers }));
}

describe("key recorder", () => {
  it("records a lone right Alt on keyup (Doubao adaptation case)", () => {
    const done: Array<{ vk: number; modifiers: number }> = [];
    const record = capture(done);
    record(keyEvent("keydown", { code: "AltRight", key: "Alt" }));
    expect(done).toEqual([]);
    record(keyEvent("keyup", { code: "AltRight", key: "Alt" }));
    expect(done).toEqual([{ vk: 0xa5, modifiers: 0 }]);
  });

  it("left and right modifiers map to distinct VKs", () => {
    const cases: Array<[string, number]> = [
      ["ControlLeft", 0xa2], ["ControlRight", 0xa3],
      ["ShiftLeft", 0xa0], ["ShiftRight", 0xa1],
      ["AltLeft", 0xa4], ["AltRight", 0xa5],
    ];
    for (const [code, vk] of cases) {
      const done: Array<{ vk: number; modifiers: number }> = [];
      const record = capture(done);
      record(keyEvent("keydown", { code, key: "?" }));
      record(keyEvent("keyup", { code, key: "?" }));
      expect(done).toEqual([{ vk, modifiers: 0 }]);
    }
  });

  it("a full combo confirms on the main key; later modifier keyups record nothing", () => {
    const done: Array<{ vk: number; modifiers: number }> = [];
    const record = capture(done);
    record(keyEvent("keydown", { code: "ControlLeft", key: "Control" }));
    record(keyEvent("keydown", { code: "AltLeft", key: "Alt" }));
    record(keyEvent("keydown", { code: "KeyJ", key: "j", keyCode: 0x4a, ctrlKey: true, altKey: true }));
    expect(done).toEqual([{ vk: 0x4a, modifiers: 2 | 1 }]);
    // 组合键已确认、栈已清：后续修饰键松开不得再录出单键（覆盖回归）。
    record(keyEvent("keyup", { code: "ControlLeft", key: "Control" }));
    record(keyEvent("keyup", { code: "AltLeft", key: "Alt" }));
    expect(done).toHaveLength(1);
  });

  it("mid-sequence modifier release does not record a stray single key", () => {
    const done: Array<{ vk: number; modifiers: number }> = [];
    const record = capture(done);
    record(keyEvent("keydown", { code: "ControlLeft", key: "Control" }));
    record(keyEvent("keydown", { code: "AltLeft", key: "Alt" }));
    record(keyEvent("keyup", { code: "ControlLeft", key: "Control" }));
    expect(done).toEqual([]);
    record(keyEvent("keyup", { code: "AltLeft", key: "Alt" }));
    expect(done).toEqual([{ vk: 0xa4, modifiers: 0 }]);
  });

  it("keydown without a key code is ignored (defensive)", () => {
    const done: Array<{ vk: number; modifiers: number }> = [];
    const record = capture(done);
    record(keyEvent("keydown", { code: "KeyZ", key: "z" }));
    expect(done).toEqual([]);
  });
});

/// 快捷键录制判定（ConnectionPage 触发键录制与 ActionPicker 槽位录制共用）。
///
/// 语义：修饰键（Ctrl/Shift/Alt/Win，区分左右）按下先暂存——期间若按下任意
/// 主键则按组合键确认、暂存作废；若松开时仍是暂存的那颗键，确认录入该修饰键
/// 单键。此前实现直接忽略修饰键 keydown，导致右 Alt 这类单键触发键永远录不上
/// （豆包输入法适配的必要场景）。

export type RecorderResult =
  | { kind: "done"; vk: number; modifiers: number }
  | { kind: "pending"; vk: number }
  | { kind: "none" };

/// event.code 是物理键位，是唯一能区分左右修饰键的信号。
export function modifierVkFromCode(code: string): number | null {
  switch (code) {
    case "ControlLeft":
      return 0xa2;
    case "ControlRight":
      return 0xa3;
    case "ShiftLeft":
      return 0xa0;
    case "ShiftRight":
      return 0xa1;
    case "AltLeft":
      return 0xa4;
    case "AltRight":
      return 0xa5;
    case "MetaLeft":
    case "MetaRight":
      return 0x5b;
    default:
      return null;
  }
}

/// pending：上一颗暂存的修饰键 VK（无则 null），由调用方持有并在 done 后清空。
export function recordKeyEvent(event: KeyboardEvent, pending: number | null): RecorderResult {
  const modVk = modifierVkFromCode(event.code);
  if (event.type === "keydown") {
    if (modVk != null) {
      return { kind: "pending", vk: modVk };
    }
    const vk = event.which || event.keyCode;
    if (!vk) return { kind: "none" };
    let modifiers = 0;
    if (event.ctrlKey) modifiers |= 2;
    if (event.shiftKey) modifiers |= 4;
    if (event.altKey) modifiers |= 1;
    if (event.metaKey) modifiers |= 8;
    return { kind: "done", vk, modifiers };
  }
  // keyup：松开的正是暂存的那颗修饰键（期间无主键介入）→ 单键确认。
  if (modVk != null && pending === modVk) {
    return { kind: "done", vk: modVk, modifiers: 0 };
  }
  return { kind: "none" };
}

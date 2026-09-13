/// 快捷键录制判定（ConnectionPage 触发键录制与 ActionPicker 槽位录制共用）。
///
/// 语义：修饰键（区分左右）按下进入"按住栈"；任意主键按下立即确认为组合键
/// （修饰键取键盘状态标志）；栈内修饰键全部松开时，最后松开的那颗确认为单键。
/// 此前两版缺陷：v1 直接忽略修饰键 keydown（右 Alt 单键永远录不上）；v2 用
/// 单槽暂存——组合键尝试中任何一颗修饰键中途松开都会被误录成单键并覆盖
/// 之前的录制（"只能录到单个键"的由来）。

export type KeyRecorder = (event: KeyboardEvent) => void;

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

export function createKeyRecorder(onDone: (vk: number, modifiers: number) => void): KeyRecorder {
  const held: number[] = []; // 按下顺序的修饰键栈
  return (event) => {
    const modVk = modifierVkFromCode(event.code);
    if (event.type === "keydown") {
      if (modVk != null) {
        if (!held.includes(modVk)) held.push(modVk);
        return;
      }
      const vk = event.which || event.keyCode;
      if (!vk) return;
      let modifiers = 0;
      if (event.ctrlKey) modifiers |= 2;
      if (event.shiftKey) modifiers |= 4;
      if (event.altKey) modifiers |= 1;
      if (event.metaKey) modifiers |= 8;
      held.length = 0;
      onDone(vk, modifiers);
      return;
    }
    if (modVk == null) return;
    const index = held.indexOf(modVk);
    // 不在栈里 = 这颗修饰键的按下已被组合键确认消费，松开不再参与裁决，
    // 否则会跟着 held.length === 0 的条件误录出单键。
    if (index < 0) return;
    held.splice(index, 1);
    // 全部松开才确认单键——中途松开的修饰键不触发（组合键尝试不被单键覆盖）。
    if (held.length === 0) onDone(modVk, 0);
  };
}

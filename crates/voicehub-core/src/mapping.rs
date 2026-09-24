//! 按键映射：每个按键 → { 单击 / 双击 / 长按 } 三槽动作。
//!
//! 语音键不参与映射（固定为开麦）。全部 12 个按键均支持双击/长按二级槽。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::actions::ButtonAction;
use crate::buttons::RemoteButton;
use crate::gesture::Gesture;

/// 单个按键的三个动作槽。
///
/// `push_to_talk` 是边沿直达的第四通道：按下沿向前端发 ptt-down、释放沿发
/// ptt-up（事件直连引擎，不经注入，也不经过单击/双击/长按判定）——用于把
/// 遥控器键变成"按住说话"触发键（麦克风输入源）。绑定后该键的三槽动作被
/// `resolve` 互斥忽略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ButtonBinding {
    pub single: ButtonAction,
    pub double: ButtonAction,
    pub long: ButtonAction,
    pub push_to_talk: bool,
}

impl Default for ButtonBinding {
    fn default() -> Self {
        Self {
            single: ButtonAction::Disabled,
            double: ButtonAction::Disabled,
            long: ButtonAction::Disabled,
            push_to_talk: false,
        }
    }
}

impl ButtonBinding {
    pub fn single(action: ButtonAction) -> Self {
        Self { single: action, ..Default::default() }
    }
}

/// 一整套按键映射（按键 ID → 绑定）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ButtonMapping {
    pub bindings: HashMap<String, ButtonBinding>,
}

impl ButtonMapping {
    pub fn key(button: RemoteButton) -> String {
        serde_json::to_value(button)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{button:?}").to_lowercase())
    }

    pub fn get(&self, button: RemoteButton) -> ButtonBinding {
        self.bindings.get(&Self::key(button)).cloned().unwrap_or_default()
    }

    pub fn set(&mut self, button: RemoteButton, binding: ButtonBinding) {
        let key = Self::key(button);
        if binding == ButtonBinding::default() {
            self.bindings.remove(&key);
        } else {
            self.bindings.insert(key, binding);
        }
    }

    /// 手势 → 动作解析。全部按键支持三槽；长按槽已绑定动作时按住期由
    /// 长按动作接管（抑制单击连发），长按槽为空则维持连发单击。
    pub fn resolve(&self, button: RemoteButton, gesture: Gesture) -> Option<ButtonAction> {
        let binding = self.get(button);
        let action = match gesture {
            // 按住说话直通键：三槽动作全部互斥（含滚轮 tick 走的 SingleClick）。
            _ if binding.push_to_talk => return None,
            Gesture::SingleClick => binding.single,
            Gesture::DoubleClick => binding.double,
            Gesture::LongPress => binding.long,
            Gesture::Repeat => {
                // 长按槽为空 → 连发单击（方向键/音量±按住连发）；
                // 长按槽有绑定 → 长按动作接管按住期，抑制连发（对齐参考实现）。
                if binding.long == ButtonAction::Disabled {
                    binding.single
                } else {
                    ButtonAction::Disabled
                }
            }
        };
        match action {
            ButtonAction::Disabled => None,
            other => Some(other),
        }
    }
}

/// 出厂默认映射：导航键位 + 常用编辑组合（对齐 vibe-flow“通用导航”预设）。
/// 电源/TV 无通用安全动作，缺省 Disabled（用户自配）。
pub fn default_mapping() -> ButtonMapping {
    use crate::actions::vk;
    let shortcut = |vk: u16, m: u8, label: &str| {
        ButtonAction::Shortcut { vk, modifiers: m, label: label.into() }
    };
    let mut mapping = ButtonMapping::default();
    mapping.set(
        RemoteButton::Up,
        ButtonBinding::single(shortcut(vk::UP, 0, "↑")),
    );
    mapping.set(
        RemoteButton::Down,
        ButtonBinding::single(shortcut(vk::DOWN, 0, "↓")),
    );
    mapping.set(
        RemoteButton::Ok,
        ButtonBinding::single(shortcut(vk::RETURN, 0, "Enter")),
    );
    mapping.set(
        RemoteButton::Left,
        ButtonBinding::single(shortcut(vk::LEFT, 0, "←")),
    );
    mapping.set(
        RemoteButton::Right,
        ButtonBinding::single(shortcut(vk::RIGHT, 0, "→")),
    );
    mapping.set(
        RemoteButton::Back,
        ButtonBinding::single(shortcut(vk::BROWSER_BACK, 0, "↩")),
    );
    mapping.set(
        RemoteButton::VolumeUp,
        ButtonBinding::single(ButtonAction::VolumeUp),
    );
    mapping.set(
        RemoteButton::VolumeDown,
        ButtonBinding::single(ButtonAction::VolumeDown),
    );
    mapping
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{ButtonAction, MOD_CONTROL};

    fn paste() -> ButtonAction {
        ButtonAction::Shortcut { vk: 0x56, modifiers: MOD_CONTROL, label: "Ctrl+V".into() }
    }

    #[test]
    fn resolve_single_action() {
        let m = default_mapping();
        let action = m.resolve(RemoteButton::Up, Gesture::SingleClick).unwrap();
        assert_eq!(
            action,
            ButtonAction::Shortcut { vk: 0x26, modifiers: 0, label: "↑".into() }
        );
    }

    /// 全键二级：任意按键的 double/long 槽均可命中（以 Up 为代表——
    /// 旧模型里它不支持二级）。
    #[test]
    fn secondary_slots_resolve_for_all_buttons() {
        let mut m = ButtonMapping::default();
        let mut binding = ButtonBinding::default();
        binding.single = paste();
        binding.double = ButtonAction::ShowDesktop;
        m.set(RemoteButton::Home, binding.clone());
        assert_eq!(m.resolve(RemoteButton::Home, Gesture::DoubleClick), Some(ButtonAction::ShowDesktop));

        m.set(RemoteButton::Up, binding);
        assert_eq!(m.resolve(RemoteButton::Up, Gesture::DoubleClick), Some(ButtonAction::ShowDesktop));
        assert!(m.resolve(RemoteButton::Up, Gesture::SingleClick).is_some());
        // 长按槽未配 → 长按解析为空，但双击不再被拦。
        assert_eq!(m.resolve(RemoteButton::Up, Gesture::LongPress), None);
    }

    /// 连发语义：长按槽为空 → 连发单击；长按槽有绑定 → 长按接管、连发抑制。
    #[test]
    fn long_press_slot_suppresses_repeat() {
        let mut m = ButtonMapping::default();
        let mut binding = ButtonBinding::default();
        binding.single = ButtonAction::VolumeUp;
        binding.long = ButtonAction::ShowDesktop;
        m.set(RemoteButton::Up, binding);
        assert_eq!(m.resolve(RemoteButton::Up, Gesture::Repeat), None);

        let mut plain = ButtonBinding::default();
        plain.single = ButtonAction::VolumeUp;
        m.set(RemoteButton::Down, plain);
        assert_eq!(m.resolve(RemoteButton::Down, Gesture::Repeat), Some(ButtonAction::VolumeUp));
    }

    #[test]
    fn repeat_falls_back_to_single() {
        let mut m = ButtonMapping::default();
        let mut b = ButtonBinding::default();
        b.single = ButtonAction::VolumeUp;
        m.set(RemoteButton::Menu, b);
        assert_eq!(m.resolve(RemoteButton::Menu, Gesture::Repeat), Some(ButtonAction::VolumeUp));
    }

    #[test]
    fn unset_binding_resolves_to_none() {
        let m = ButtonMapping::default();
        assert_eq!(m.resolve(RemoteButton::Menu, Gesture::SingleClick), None);
    }

    #[test]
    fn default_binding_serialization_roundtrip() {
        let m = default_mapping();
        let json = serde_json::to_string(&m).unwrap();
        let back: ButtonMapping = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn setting_default_binding_removes_entry() {
        let mut m = default_mapping();
        m.set(RemoteButton::Menu, ButtonBinding::default());
        assert!(!m.bindings.contains_key(&ButtonMapping::key(RemoteButton::Menu)));
    }

    /// 回归：`key()` 必须与 serde 序列化名一致（统计按键分布、回执、
    /// 前端 buttonNames 表共用这套键；漂移会导致统计页显示不出按键名）。
    #[test]
    fn keys_match_serde_names() {
        for button in RemoteButton::ALL {
            let key = ButtonMapping::key(button);
            assert_eq!(
                key,
                serde_json::to_value(button).unwrap().as_str().unwrap(),
                "key() 与 serde 名漂移：{key}"
            );
        }
        // 多词键名带下划线（12 键模型），确保 serde snake_case 命名稳定。
        assert_eq!(ButtonMapping::key(RemoteButton::VolumeUp), "volume_up");
        assert_eq!(ButtonMapping::key(RemoteButton::VolumeDown), "volume_down");
    }

    /// push_to_talk 字段向后兼容：旧配置 JSON（无该字段）反序列化为 false；
    /// 绑定后三槽动作被 resolve 互斥忽略（含滚轮 SingleClick 路径）。
    #[test]
    fn push_to_talk_field_roundtrips_and_mutes_slots() {
        let legacy = serde_json::json!({ "single": { "kind": "disabled" }, "double": { "kind": "disabled" }, "long": { "kind": "disabled" } });
        let binding: ButtonBinding = serde_json::from_value(legacy).expect("legacy binding must parse");
        assert!(!binding.push_to_talk);

        let mut m = ButtonMapping::default();
        m.bindings.insert(
            ButtonMapping::key(RemoteButton::Up),
            ButtonBinding { push_to_talk: true, single: paste(), ..Default::default() },
        );
        for gesture in [Gesture::SingleClick, Gesture::DoubleClick, Gesture::LongPress, Gesture::Repeat] {
            assert!(m.resolve(RemoteButton::Up, gesture).is_none(),
                "push-to-talk key must mute gesture slot {gesture:?}");
        }
    }
}

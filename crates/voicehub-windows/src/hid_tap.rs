//! 管理员伴生进程直读通道(tap)的报文解析与沿路由。
//!
//! 方案 B 的数据层(见 docs/plan-remote-key-takeover.md §5):伴生 exe 直读
//! 键盘 HID 集合,拿到被 Windows 输入管线丢弃的报文(返回/音量±)以及
//! **先于 LL 钩子**的原始报文(tap 侧武装——WM_INPUT 武装被证明在钩子返回
//! 之后才生成,等待窗方案结构性死锁,2026-09-25 真机打点定案)。
//!
//! 本模块是纯逻辑(可单测):
//! - `TapParser::feed` 把伴生转发的原始报文(hex)解析成 usage 集合并差分出沿;
//! - [`edge_action`] 决定沿去向:键盘通道 9 键 → 武装 + 直派(tap 模式下
//!   Raw Input 同沿让位,映射由 tap 通道直驱,与按钮通道同构);
//!   其余(Back/音量±)→ UsageSet 事件直入手势/映射/统计管线。
//!
//! 报文格式(L1,Vibe-Remote tap 解析 + 真机待验):report_id==1,数据 6 字节
//! = 3× u16 LE usage。首启真机会逐键核对,不符时只改 [`parse_report`]。

use crate::raw_input::HidEvent;
use voicehub_core::buttons::RemoteButton;

/// 伴生转发的单条报文解析出的 usage 集合。空集 = 无按键(释放报文)。
pub fn parse_report(report: &[u8]) -> Option<Vec<u16>> {
    // 哑中继约定:首个字节为 report_id,其余为数据。键盘集合报文:
    // id==1,数据 6 字节 = 3× u16 LE usage。
    let (&id, data) = report.split_first()?;
    if id != 1 {
        return None;
    }
    // 兼容两种长度口径:总长 7(id+6)或把末尾 pad 计入。以 6 字节为准。
    if data.len() < 6 {
        return None;
    }
    let usages: Vec<u16> = data[..6]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Some(usages)
}

/// tap 沿(usage + 按下/释放)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapEdge {
    pub usage: u16,
    pub pressed: bool,
}

/// usage 集合差分器:报文是"当前按住集合"(状态型),沿 = 与上次的差。
#[derive(Debug, Default)]
pub struct TapParser {
    prev: Option<Vec<u16>>,
}

impl TapParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一条解析后的 usage 集合,返回相对上一条的按下/释放沿。
    /// 首条报文只建立基线,不产生沿(伴生连接时可能恰好有键按住)。
    pub fn update(&mut self, usages: &[u16]) -> Vec<TapEdge> {
        let Some(prev) = self.prev.take() else {
            self.prev = Some(usages.to_vec());
            return Vec::new();
        };
        let mut edges = Vec::new();
        for &usage in usages {
            if !prev.contains(&usage) {
                edges.push(TapEdge { usage, pressed: true });
            }
        }
        for usage in &prev {
            if !usages.contains(usage) {
                edges.push(TapEdge { usage: *usage, pressed: false });
            }
        }
        self.prev = Some(usages.to_vec());
        edges
    }
}

/// 单条 tap 沿的去向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapAction {
    /// 键盘通道键:武装 LL 钩子消费端(吞原生键),映射由 tap 通道直派
    /// (宿主侧按此合成 UsageSet;tap 模式下 WM_INPUT 同沿让位,不会双喂)。
    ArmKeyboard { vk: u32, scan: u32, extended: bool, pressed: bool },
    /// 非键盘通道键(Back/音量± 等):合成 UsageSet 事件走映射管线。
    Button(RemoteButton),
    /// 未识别 usage(真机首启逐键核对后进表)。
    Unknown,
}

/// 键盘通道 9 键:HID 键盘页 usage → (VK, 扫描码, 扩展位)。
/// 与 raw_input::vk_remote_button 的 VK 表逐键对应(2026-09-24 真机 L1 定案);
/// 电源以扫描码 0x5E 入表(VK 落 0xFF)。武装位与 LL 钩子报文逐字段对齐。
fn keyboard_usage_vk(usage: u16) -> Option<(u32, u32, bool)> {
    Some(match usage {
        0x52 => (0x26, 0x48, true),  // 上 VK_UP
        0x28 => (0x0D, 0x1C, false), // OK VK_RETURN
        0x51 => (0x28, 0x50, true),  // 下 VK_DOWN
        0x50 => (0x25, 0x4B, true),  // 左 VK_LEFT
        0x4F => (0x27, 0x4D, true),  // 右 VK_RIGHT
        0x35 => (0xC0, 0x29, false), // TV VK_OEM_3
        0x4A => (0x24, 0x47, true),  // 主页 VK_HOME
        0x65 => (0x5D, 0x5D, true),  // 菜单 VK_APPS
        0x5E => (0xFF, 0x5E, true),  // 电源(Windows 无法映射,VK 落 0xFF)
        _ => return None,
    })
}

/// 沿路由:先查键盘通道表(命中=武装钩子消费端 + 宿主直派 UsageSet——
/// tap 模式下 Raw Input 同沿让位,不会双喂;L1 2026-09-25:被钩子吞掉的沿
/// 不生成 WM_INPUT,映射必须由 tap 通道直驱);未命中再看 usage_map(Back/
/// 音量± 等 Windows 丢弃的键,tap 是唯一事件源);都没有 = 未知。
pub fn edge_action(edge: TapEdge) -> TapAction {
    if let Some((vk, scan, extended)) = keyboard_usage_vk(edge.usage) {
        return TapAction::ArmKeyboard { vk, scan, extended, pressed: edge.pressed };
    }
    match RemoteButton::from_hid_usage(edge.usage) {
        Some(button) => TapAction::Button(button),
        None => TapAction::Unknown,
    }
}

/// tap 沿 → 与 WM_INPUT 同构的 HidEvent(Button 路由用)。
pub fn usage_set_event(usage: u16, pressed: bool) -> HidEvent {
    HidEvent::UsageSet(if pressed { vec![usage] } else { Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(usages: [u16; 3]) -> Vec<u8> {
        let mut bytes = vec![1u8];
        for usage in usages {
            bytes.extend_from_slice(&usage.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn parses_report_id_1_three_usages() {
        let usages = parse_report(&report([0xF1, 0x80, 0])).expect("valid report");
        assert_eq!(usages, vec![0xF1, 0x80, 0]);
        // 非 1 号报文 / 过短数据:拒绝。
        assert!(parse_report(&[2, 0, 0, 0, 0, 0, 0]).is_none());
        assert!(parse_report(&[1, 0xF1, 0]).is_none());
        assert!(parse_report(&[]).is_none());
    }

    #[test]
    fn parser_diffs_edges_from_state_reports() {
        let mut parser = TapParser::new();
        // 首条:只建基线。
        assert!(parser.update(&parse_report(&report([0xF1, 0, 0])).unwrap()).is_empty());
        // 按下音量+(集合多一个 usage)。
        let edges = parser.update(&parse_report(&report([0xF1, 0x80, 0])).unwrap());
        assert_eq!(edges, vec![TapEdge { usage: 0x80, pressed: true }]);
        // 全部释放。
        let edges = parser.update(&parse_report(&report([0, 0, 0])).unwrap());
        assert_eq!(
            edges,
            vec![
                TapEdge { usage: 0xF1, pressed: false },
                TapEdge { usage: 0x80, pressed: false },
            ]
        );
    }

    #[test]
    fn keyboard_channel_usages_route_to_hook_arm() {
        // 键盘通道 9 键 → 武装(vk/scan/扩展位与 LL 钩子报文逐字段一致;
        // 直派 UsageSet 由宿主在 ArmKeyboard 分支合成,见 hid_tap_host)。
        assert_eq!(
            edge_action(TapEdge { usage: 0x52, pressed: true }),
            TapAction::ArmKeyboard { vk: 0x26, scan: 0x48, extended: true, pressed: true }
        );
        assert_eq!(
            edge_action(TapEdge { usage: 0x28, pressed: false }),
            TapAction::ArmKeyboard { vk: 0x0D, scan: 0x1C, extended: false, pressed: false }
        );
        assert_eq!(
            edge_action(TapEdge { usage: 0x5E, pressed: true }),
            TapAction::ArmKeyboard { vk: 0xFF, scan: 0x5E, extended: true, pressed: true }
        );
    }

    #[test]
    fn dropped_usages_become_usage_set_events() {
        // Back/音量±:Windows 丢弃,WM_INPUT 永不到达,tap 是唯一事件源。
        assert_eq!(
            edge_action(TapEdge { usage: 0xF1, pressed: true }),
            TapAction::Button(RemoteButton::Back)
        );
        assert_eq!(
            edge_action(TapEdge { usage: 0x80, pressed: true }),
            TapAction::Button(RemoteButton::VolumeUp)
        );
        assert_eq!(
            edge_action(TapEdge { usage: 0x81, pressed: false }),
            TapAction::Button(RemoteButton::VolumeDown)
        );
        // 未知 usage:上报 Unknown(首启逐键核对)。
        assert_eq!(edge_action(TapEdge { usage: 0x1234, pressed: true }), TapAction::Unknown);
    }

    #[test]
    fn usage_set_event_matches_wm_input_semantics() {
        assert_eq!(
            usage_set_event(0xF1, true),
            HidEvent::UsageSet(vec![0xF1])
        );
        assert_eq!(usage_set_event(0xF1, false), HidEvent::UsageSet(Vec::new()));
    }
}
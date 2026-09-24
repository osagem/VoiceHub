//! F5 吞键闸 + 遥控器键盘键武装-消费抑制(WH_KEYBOARD_LL)。
//!
//! 遥控器语音键在 HID 键盘层是 F5。原始 F5 若穿透到前台应用会触发
//! 刷新等行为,必须吞掉;但真实键盘的 F5 不能误伤。
//! F5 策略(与参考实现一致的防粘键设计):
//! - DOWN 沿:会话激活、武装窗口内(GATT 控制通知后 ~250ms)或常驻武装
//!   (遥控器 BLE 已连接且总开关开启)任一成立才吞;非 F5 一律透传;
//!   注入事件(LLKHF_INJECTED)一律透传。
//! - 常驻武装是时序兜底的最终层:LL 钩子拿不到按键来源设备,GATT 通知
//!   缺失/迟到时首个 F5 仍会泄漏(真机实测结论);遥控器在线期间直接吞掉
//!   全部 F5,代价是真键盘 F5 暂时失效(Ctrl+R 不受影响)。
//! - UP 沿:只按配对裁决——本次按住的所有 DOWN 全被吞才吞 UP,
//!   任何 DOWN 泄漏则 UP 必放行(宁送孤立 UP,不留 OS 粘键)。
//!
//! 遥控器键盘键抑制(2026-09-25 重构,参考 Vibe-Remote 武装-消费方案):
//! LL 钩子报文不含设备来源,无法直接区分遥控器与物理键盘;但 Raw Input
//! 报文带设备路径。遥控器按键事件按「钩子先到、WM_INPUT 晚 ~17ms」的
//! 实测时序,用两层配合:
//! 1. **武装(arm)**:Raw Input 线程(`raw_input::parse_raw_input`)在设备
//!    校验通过后,把遥控器键盘沿(键值+扫描码+扩展位+按下/释放)写入武装表,
//!    有效期 180ms,并唤醒等待中的钩子;
//! 2. **消费(consume)**:钩子遇到遥控器 VK 集合内的按键时,等待至多
//!    CONSUME_WAIT_MS(80ms;初值 60ms 真机实测必然超窗,用户裁定上调)查武装表;
//!    命中 → `return 1` 吞掉;超时 → 放行。遥控器离线(`persistent_armed`
//!    为假)或总开关关闭 → 零等待直接放行(物理键盘零影响)。
//! tap 模式下映射动作由 tap 通道直派(合成 UsageSet,Raw Input 同沿让位);
//! 钩子只负责吞,不喂边沿。tap 掉线时零等待放行,WM_INPUT 恢复驱动映射
//! (方案 A 之前的 9 键基线),降级安全。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED,
    LLKHF_LOWER_IL_INJECTED, MSG, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
};

const VK_F5: u32 = 0x74;

/// 武装窗口:GATT 控制通知到达后的一小段时间(遥控器 HID F5 通常
/// 晚 60–90ms 到达;应用被后台节流时工作线程可能再拖 120ms)。
const ARM_GRACE_MS: i64 = 250;

/// 消费等待窗:钩子等武装到达的上限。2026-09-25 用户定:采用 Vibe-Remote
/// 设定值(consume_wait_seconds=0.060)起步;真机实测武装到达 62~72ms(钩子
/// 线程打点)必然超窗致原生键泄漏,用户裁定上调至 80ms(72ms + 余量)。
pub const CONSUME_WAIT_MS: u64 = 80;
/// 武装沿有效期(Vibe-Remote 同款 180ms;须 > 消费等待窗)。
const ARM_LIFETIME_MS: i64 = 180;

static MASTER: AtomicBool = AtomicBool::new(false);
static SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);
static ARMED_UNTIL_MS: AtomicI64 = AtomicI64::new(0);
static HOLD_PAIRING: AtomicU32 = AtomicU32::new(HOLD_NONE);
static HOOK: AtomicI64 = AtomicI64::new(0);
/// 常驻武装总开关(设置项 f5GateEnabled,默认开;关闭 = 逃生开关:
/// F5 退回纯时序兜底,遥控器键盘键零等待直通)。
static GATE_ENABLED: AtomicBool = AtomicBool::new(true);
/// 遥控器 BLE 连接状态(Ready 时常驻武装,断开即解除)。
static REMOTE_CONNECTED: AtomicBool = AtomicBool::new(false);
/// tap 模式开关(方案 B:管理员伴生进程直读通道在跑)。开启后:
/// - 武装独占 tap 通道——Raw Input 的 WM_INPUT 武装不再入表;且 Raw Input
///   对键盘通道 9 键整体让位(tap 通道武装并直派 UsageSet,防一个物理沿
///   双武装/双派发);
/// - 钩子的遥控器 VK 分支才进入武装-消费等待;**被吞按键的映射由 tap 通道
///   合成的 UsageSet 驱动**(L1 2026-09-25 真机定案:钩子吞掉按键后 WM_INPUT
///   按下沿不会送达——"吞键后 WM_INPUT 照常送达"的旧假设被证伪,单击
///   只余孤立释放沿,这正是此前"单击失灵、长按靠自动重复泄漏沿存活"的根因)。
///   tap 掉线时宿主立即关掉本开关:回退零等待放行 + WM_INPUT 映射
///   (方案 A 之前的 9 键基线行为)。
static TAP_MODE: AtomicBool = AtomicBool::new(false);
/// tap 持续会话:tap 武装命中的按下沿把 VK 记入,直到同 VK 的 UP 沿。
/// 作用:按住期键盘自动重复的 DOWN 沿(无 tap 武装,消费必超时)也直接吞,
/// 杜绝原生键泄漏(接管语义);吞 UP 沿即清会话,防粘键。遥控器断开 /
/// tap 关闭时一并清除(防陈旧会话吞掉物理键盘同 VK 按键)。
static TAP_HOLD_VK: AtomicU32 = AtomicU32::new(0);
/// 安装权互斥:覆盖 spawn → HOOK.store 的窗口,防止自愈重装与初始
/// 安装并发时双线程双钩子(双钩子吞键无害,但退出只卸一个会泄漏到进程结束)。
static INSTALL_LOCK: AtomicBool = AtomicBool::new(false);

pub const HOLD_NONE: u32 = 0;
pub const HOLD_SWALLOWED_ALL: u32 = 1;
pub const HOLD_LEAKED: u32 = 2;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 武装表:Raw Input 线程写,钩子线程读删。条目带有效期,读取时惰性清理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArmedEdge {
    vk: u32,
    scan: u32,
    extended: bool,
    pressed: bool,
    expires_ms: i64,
}

fn arm_table_cell() -> &'static OnceLock<Mutex<Vec<ArmedEdge>>> {
    static CELL: OnceLock<Mutex<Vec<ArmedEdge>>> = OnceLock::new();
    &CELL
}

/// 武装表条件变量:武装到达时唤醒钩子线程的等待。
static ARM_SIGNAL: Condvar = Condvar::new();

fn arm_table() -> &'static Mutex<Vec<ArmedEdge>> {
    arm_table_cell().get_or_init(|| Mutex::new(Vec::new()))
}

/// Raw Input 线程调用:遥控器键盘沿入武装表并唤醒等待中的钩子。
/// F5/未知键不入表(消费端不等待它们;F5 走会话/常驻武装专用裁决)。
/// tap 模式下为空操作(武装独占 tap 通道,见 [`TAP_MODE`])。
pub fn arm_keyboard_edge(vk: u32, scan: u32, extended: bool, pressed: bool) {
    if TAP_MODE.load(Ordering::Relaxed) {
        return;
    }
    arm_edge_impl(vk, scan, extended, pressed);
}

/// tap 通道调用:伴生进程先于钩子送达的沿,入武装表并唤醒钩子。
/// 不受 TAP_MODE 空操作影响(它本身就是 tap 通道的入口)。
pub fn arm_tap_edge(vk: u32, scan: u32, extended: bool, pressed: bool) {
    arm_edge_impl(vk, scan, extended, pressed);
}

fn arm_edge_impl(vk: u32, scan: u32, extended: bool, pressed: bool) {
    if crate::raw_input::vk_remote_button(vk as u16, scan as u16).is_none() {
        return;
    }
    let mut table = arm_table().lock().unwrap_or_else(|e| e.into_inner());
    let now = now_ms();
    table.retain(|edge| edge.expires_ms > now);
    table.push(ArmedEdge {
        vk,
        scan,
        extended,
        pressed,
        expires_ms: now + ARM_LIFETIME_MS,
    });
    drop(table);
    // 临时打点(排查用):证明 Raw Input→武装表链路活着。
    log::info!("[key-gate] arm vk=0x{vk:02X} scan=0x{scan:02X} ext={extended} pressed={pressed}");
    ARM_SIGNAL.notify_all();
}

/// 钩子线程调用:等待窗内匹配武装沿。命中(先精确匹配键值+扫描码+扩展位,
/// 再退化为键值+沿)即移除并返回真 = 事件来自遥控器。
fn consume_armed_edge(vk: u32, scan: u32, extended: bool, is_up: bool, wait: Duration) -> bool {
    let pressed = !is_up;
    let started = Instant::now();
    let deadline = started + wait;
    let mut table = arm_table().lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let now = now_ms();
        table.retain(|edge| edge.expires_ms > now);
        let hit = table
            .iter()
            .position(|edge| {
                edge.vk == vk && edge.pressed == pressed && edge.scan == scan && edge.extended == extended
            })
            .or_else(|| {
                table
                    .iter()
                    .position(|edge| edge.vk == vk && edge.pressed == pressed)
            });
        if let Some(index) = hit {
            table.remove(index);
            // 武装延迟打点(方案 §4.4-3):命中前等待时长 ≈ 钩子回调→WM_INPUT
            // 武装到达延迟,作为等待窗(60ms)调参依据;0ms = 武装先于钩子到达
            // (本机时序优于 Vibe-Remote 的钩子先到 ~17ms),同样记录。
            let waited = started.elapsed().as_millis();
            log::info!(
                "[key-gate] arm latency ~{waited}ms (vk=0x{vk:02X}, pressed={pressed})"
            );
            return true;
        }
        let remaining = deadline.checked_duration_since(Instant::now());
        let Some(remaining) = remaining else {
            return false;
        };
        let (guard, _) = ARM_SIGNAL
            .wait_timeout(table, remaining)
            .unwrap_or_else(|e| e.into_inner());
        table = guard;
    }
}

#[cfg(test)]
fn clear_armed_edges() {
    arm_table().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// 纯决策(F5 专用,单测覆盖)。`persistent` = 常驻武装生效中
/// (开关开且遥控器在线)。
pub fn decide(
    vk_code: u32,
    is_key_up: bool,
    session: bool,
    armed_now: bool,
    hold_pairing: u32,
    persistent: bool,
) -> bool {
    if vk_code != VK_F5 {
        return false;
    }
    if is_key_up {
        return hold_pairing == HOLD_SWALLOWED_ALL;
    }
    session || armed_now || persistent
}

/// 纯状态转移:DOWN 裁决后更新配对。
pub fn track_down(hold_pairing: u32, down_swallowed: bool) -> u32 {
    if hold_pairing == HOLD_LEAKED {
        return HOLD_LEAKED;
    }
    if down_swallowed {
        HOLD_SWALLOWED_ALL
    } else {
        HOLD_LEAKED
    }
}

/// GATT 控制通知回调线程直接调用(不要排队到工作线程——节流会迟到)。
pub fn arm_grace() {
    ARMED_UNTIL_MS.store(now_ms() + ARM_GRACE_MS, Ordering::Relaxed);
}

pub fn set_session_active(active: bool) {
    SESSION_ACTIVE.store(active, Ordering::Relaxed);
    if !active {
        // 会话结束:清配对,允许武装窗口继续兜尾沿。
        HOLD_PAIRING.store(HOLD_NONE, Ordering::Relaxed);
    }
}

/// 常驻武装总开关(设置项同步;关闭时 F5 退回纯时序兜底,真键盘 F5 恢复;
/// 遥控器键盘键消费等待随之关闭,零延迟直通)。
pub fn set_gate_enabled(enabled: bool) {
    GATE_ENABLED.store(enabled, Ordering::Relaxed);
}

/// 遥控器 BLE 连接状态(BLE 快照变化时同步;Ready = 常驻武装 + 消费等待开启;
/// 断开时清 tap 持续会话——按住中掉线的 UP 沿不会再来了)。
pub fn set_remote_connected(connected: bool) {
    REMOTE_CONNECTED.store(connected, Ordering::Relaxed);
    if !connected {
        TAP_HOLD_VK.store(0, Ordering::Relaxed);
    }
}

/// tap 模式开关(伴生进程启动/掉线时由宿主同步;关闭时清持续会话)。
pub fn set_tap_mode(enabled: bool) {
    TAP_MODE.store(enabled, Ordering::Relaxed);
    if !enabled {
        TAP_HOLD_VK.store(0, Ordering::Relaxed);
    }
}

pub fn tap_mode() -> bool {
    TAP_MODE.load(Ordering::Relaxed)
}

fn armed() -> bool {
    let until = ARMED_UNTIL_MS.load(Ordering::Relaxed);
    until != 0 && now_ms() < until
}

fn persistent_armed() -> bool {
    GATE_ENABLED.load(Ordering::Relaxed) && REMOTE_CONNECTED.load(Ordering::Relaxed)
}

/// 诊断用:常驻武装当前是否激活(遥控器在线且开关开启)。
pub fn is_persistent_armed() -> bool {
    persistent_armed()
}

/// 安装全局钩子(启动专用线程跑消息泵;LL 钩子要求有线程消息循环)。
/// 消息泵退出或线程 panic 时主动卸载钩子并清 HOOK 标志——宿主
/// (bridge 周期自检)据 is_installed()=false 重装,保护不静默失效。
pub fn install() -> bool {
    if HOOK.load(Ordering::Relaxed) != 0 {
        return true;
    }
    if INSTALL_LOCK
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return true; // 另一线程正在安装。
    }
    MASTER.store(true, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("vh-key-gate".into())
        .spawn(|| {
            let pumped = std::panic::catch_unwind(|| unsafe {
                match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) {
                    Ok(hook) => {
                        HOOK.store(hook.0 as i64, Ordering::Relaxed);
                    }
                    Err(error) => {
                        log::error!("SetWindowsHookExW(WH_KEYBOARD_LL) failed: {error}");
                        INSTALL_LOCK.store(false, Ordering::Relaxed);
                        return;
                    }
                }
                // 安装已生效,释放安装权(后续丢失可重装)。
                INSTALL_LOCK.store(false, Ordering::Relaxed);
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    DispatchMessageW(&msg);
                }
                // 消息泵停转(WM_QUIT / GetMessageW 出错):卸载并清标志。
                let handle = HOOK.swap(0, Ordering::Relaxed);
                if handle != 0 {
                    let hook = HHOOK(handle as *mut core::ffi::c_void);
                    let _ = UnhookWindowsHookEx(hook);
                }
            });
            if pumped.is_err() {
                // panic 路径兜底清标志(swap 卸载可能没跑到),
                // 留着旧值会让 is_installed() 误报、自检永不重装。
                HOOK.store(0, Ordering::Relaxed);
                INSTALL_LOCK.store(false, Ordering::Relaxed);
                log::error!("[key-gate] hook thread panicked; flag cleared for self-heal");
            }
        })
        .is_ok();
    if !spawned {
        // 线程都没起来:释放安装权,下次自检可重试。
        INSTALL_LOCK.store(false, Ordering::Relaxed);
    }
    spawned
}

/// 钩子是否已安装(诊断用)。
pub fn is_installed() -> bool {
    HOOK.load(Ordering::Relaxed) != 0
}

/// 卸载并放行(应用退出 / 遥控器断开时)。
pub fn shutdown() {
    MASTER.store(false, Ordering::Relaxed);
    let handle = HOOK.swap(0, Ordering::Relaxed);
    if handle != 0 {
        unsafe {
            let hook = HHOOK(handle as *mut core::ffi::c_void);
            let _ = UnhookWindowsHookEx(hook);
        }
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, WM_KEYDOWN, WM_SYSKEYDOWN};
    if code >= 0 && MASTER.load(Ordering::Relaxed) {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let is_injected =
            (kb.flags & LLKHF_INJECTED).0 != 0 || (kb.flags & LLKHF_LOWER_IL_INJECTED).0 != 0;
        if !is_injected {
            let vk = kb.vkCode as u32;
            let is_up = !(wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN);

            if vk == VK_F5 {
                // 语音键:会话/武装窗口/常驻武装三选一,配对防粘键(原样保留)。
                let pairing = HOLD_PAIRING.load(Ordering::Relaxed);
                let swallow = decide(
                    vk,
                    is_up,
                    SESSION_ACTIVE.load(Ordering::Relaxed),
                    armed(),
                    pairing,
                    persistent_armed(),
                );
                if swallow {
                    if !is_up {
                        // 吞 DOWN 即顺延武装窗口:长按语音键时键盘自动重复的 F5
                        // 会逐个到达,一次性 250ms 窗口过期后就泄漏(免提模式压掉
                        // 蓝牙流、无会话兜底时尤甚)——每吞一个续一窗,直至 UP。
                        arm_grace();
                        HOLD_PAIRING.store(track_down(pairing, true), Ordering::Relaxed);
                    } else {
                        HOLD_PAIRING.store(HOLD_NONE, Ordering::Relaxed);
                    }
                    return LRESULT(1);
                }
                if !is_up {
                    // 泄漏只在配对状态转换时记一次(长按的重复 F5 不刷屏)。
                    // 竞态成因:F5 走 HID 通道,控制通知走 GATT 通道,F5 先到则
                    // 武装窗口未开——此时前台(如浏览器)会收到真实 F5(刷新)。
                    if pairing != HOLD_LEAKED {
                        log::warn!(
                            "[key-gate] remote F5 leaked (arrived before/without arming); \
                             foreground may receive a refresh"
                        );
                    }
                    HOLD_PAIRING.store(track_down(pairing, false), Ordering::Relaxed);
                }
            } else if crate::raw_input::vk_remote_button(vk as u16, kb.scanCode as u16).is_some() {
                // 遥控器键盘集合 VK:武装-消费裁决。等待只在 tap 模式下进行
                // (伴生进程先于钩子武装;2026-09-25 真机打点证明 WM_INPUT 武装
                // 必然晚于钩子——RIT 在钩子回调返回后才生成,等待结构性死锁)。
                // tap 关闭/掉线或遥控器离线 → 零等待放行,物理键盘零影响。
                let extended = (kb.flags & LLKHF_EXTENDED).0 != 0;
                let persistent = persistent_armed();
                let tap = TAP_MODE.load(Ordering::Relaxed);
                // 临时打点(排查用):钩子侧看到遥控器 VK 集合按键的全貌。
                log::info!(
                    "[key-gate] hook vk=0x{vk:02X} scan=0x{:02X} ext={extended} up={is_up} persistent={persistent} tap={tap}",
                    kb.scanCode
                );
                if TAP_MODE.load(Ordering::Relaxed) {
                    // 持续会话:此前按下沿已被 tap 武装命中吞掉——按住期键盘
                    // 自动重复的 DOWN 沿(无 tap 武装)与 UP 沿不再等待武装,
                    // 直接吞(杜绝原生键泄漏);吞 UP 即清会话,防粘键。
                    if TAP_HOLD_VK.load(Ordering::Relaxed) == vk {
                        if is_up {
                            TAP_HOLD_VK.store(0, Ordering::Relaxed);
                        }
                        return LRESULT(1);
                    }
                    if persistent {
                        let swallowed = consume_armed_edge(
                            vk,
                            kb.scanCode as u32,
                            extended,
                            is_up,
                            Duration::from_millis(CONSUME_WAIT_MS),
                        );
                        if swallowed {
                            // 吞按下沿 = 进入持续会话;吞 UP 沿 = 会话结束。
                            TAP_HOLD_VK.store(if is_up { 0 } else { vk }, Ordering::Relaxed);
                            return LRESULT(1);
                        }
                        // 超时未命中:放行(tap 通道没送来该沿;tap 关闭时由
                        // WM_INPUT 驱动映射;tap 在跑时 tap 通道本沿缺失,
                        // 仅漏原生键,映射由下一报告恢复)。
                        log::info!("[key-gate] consume timeout, passthrough vk=0x{vk:02X}");
                    }
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 各测试用互不相同的 VK,避免共享武装表互相干扰。
    #[test]
    fn armed_edge_is_consumed_immediately() {
        clear_armed_edges();
        arm_keyboard_edge(0x24, 0x47, false, true);
        assert!(consume_armed_edge(0x24, 0x47, false, false, Duration::ZERO));
        // 消费即移除:同一沿第二次不再命中。
        assert!(!consume_armed_edge(0x24, 0x47, false, false, Duration::ZERO));
    }

    #[test]
    fn unmatched_edge_times_out_without_wait_when_zero() {
        clear_armed_edges();
        // 无武装:零等待直接放行(遥控器离线场景)。
        assert!(!consume_armed_edge(0x26, 0x48, false, false, Duration::ZERO));
        // 武装的是按下沿,钩子来的是释放沿:不匹配。
        arm_keyboard_edge(0x27, 0x4D, false, true);
        assert!(!consume_armed_edge(0x27, 0x4D, false, true, Duration::ZERO));
    }

    #[test]
    fn loose_fallback_matches_when_scan_or_extended_differ() {
        clear_armed_edges();
        // 扫描码/扩展位不一致但键值+沿一致:退化匹配仍命中(容错)。
        arm_keyboard_edge(0x0D, 0x1C, false, true);
        assert!(consume_armed_edge(0x0D, 0x00, true, false, Duration::ZERO));
    }

    #[test]
    fn tap_mode_disables_raw_input_arming_but_not_tap_arming() {
        clear_armed_edges();
        set_tap_mode(true);
        // tap 模式:WM_INPUT 武装为空操作。
        arm_keyboard_edge(0x25, 0x4B, true, true);
        assert!(!consume_armed_edge(0x25, 0x4B, true, false, Duration::ZERO));
        // tap 通道武装不受影响。
        arm_tap_edge(0x25, 0x4B, true, true);
        assert!(consume_armed_edge(0x25, 0x4B, true, false, Duration::ZERO));
        set_tap_mode(false);
    }

    #[test]
    fn expired_edges_are_dropped() {
        clear_armed_edges();
        let now = now_ms();
        arm_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ArmedEdge {
                vk: 0x25,
                scan: 0x4B,
                extended: false,
                pressed: true,
                expires_ms: now - 1,
            });
        assert!(!consume_armed_edge(0x25, 0x4B, false, false, Duration::ZERO));
    }

    #[test]
    fn f5_and_unknown_keys_never_arm() {
        clear_armed_edges();
        // F5 不入武装表(专用裁决),未知键不入,电源 VK 0xFF + scan 0x5E 入表。
        arm_keyboard_edge(VK_F5, 0x3E, false, true);
        arm_keyboard_edge(0x41, 0x1E, false, true);
        arm_keyboard_edge(0xFF, 0x5E, false, true);
        let table = arm_table().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].vk, 0xFF);
    }

    #[test]
    fn only_f5_down_swallowed_when_armed_or_session() {
        assert!(!decide(0x41, false, false, false, HOLD_NONE, false));
        assert!(!decide(0x74, false, false, false, HOLD_NONE, false));
        assert!(decide(0x74, false, true, false, HOLD_NONE, false));
        assert!(decide(0x74, false, false, true, HOLD_NONE, false));
    }

    #[test]
    fn persistent_arming_swallows_despite_lost_timing_signals() {
        // 常驻武装:GATT 通知缺失/迟到、无会话时,F5 DOWN 仍被吞——
        // 这是"首个 F5 泄漏刷新页面"竞态的根治层。
        assert!(decide(0x74, false, false, false, HOLD_NONE, true));
        // 配对语义保持:全吞的按住 → 吞 UP。
        assert!(decide(0x74, true, false, false, HOLD_SWALLOWED_ALL, true));
        // 非 F5 不受常驻武装影响。
        assert!(!decide(0x41, false, false, false, HOLD_NONE, true));
    }

    #[test]
    fn up_edge_follows_down_pairing() {
        // 全吞的按住 → 吞 UP。
        assert!(decide(0x74, true, false, false, HOLD_SWALLOWED_ALL, false));
        // 任一 DOWN 泄漏 / 配对未知 → 放行 UP,即使会话仍激活。
        assert!(!decide(0x74, true, true, true, HOLD_LEAKED, false));
        assert!(!decide(0x74, true, true, true, HOLD_NONE, false));
        // 非 F5 的 UP 永不吞。
        assert!(!decide(0x41, true, true, true, HOLD_SWALLOWED_ALL, false));
    }

    #[test]
    fn leaked_hold_stays_leaked() {
        assert_eq!(track_down(HOLD_SWALLOWED_ALL, true), HOLD_SWALLOWED_ALL);
        assert_eq!(track_down(HOLD_SWALLOWED_ALL, false), HOLD_LEAKED);
        assert_eq!(track_down(HOLD_LEAKED, true), HOLD_LEAKED);
        assert_eq!(track_down(HOLD_NONE, false), HOLD_LEAKED);
    }

    #[test]
    fn consume_wait_window_covers_measured_arm_latency() {
        // 2026-09-25 真机打点:武装到达 62~72ms,Vibe-Remote 初始值 60ms 必然
        // 超窗;用户裁定 80ms。武装有效期须大于窗口。
        assert_eq!(CONSUME_WAIT_MS, 80);
        assert!(ARM_LIFETIME_MS > CONSUME_WAIT_MS as i64);
    }
}
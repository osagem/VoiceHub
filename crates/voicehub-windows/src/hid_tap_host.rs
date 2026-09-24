//! 管理员伴生进程宿主:命名管道服务端 + runas 拉起 + tap 沿路由。
//!
//! 方案 B 的运行时层(见 docs/plan-remote-key-takeover.md §5 / hid_tap 数据层):
//! - 主程序建字节模式命名管道,PowerShell `Start-Process -Verb RunAs` 拉起
//!   伴生 exe(UAC 一次);伴生注入 WUDF 宿主并中继 HidOverGatt 报文(方案 B1),
//!   原始报文 hex 行转发;
//! - 本层解析行协议 → [`hid_tap::parse_report`] → 沿差分 → 路由:
//!   键盘通道 9 键 → `key_gate::arm_tap_edge`(先于钩子武装)+ 合成
//!   [`HidInput`] 直派映射管线(tap 模式下 Raw Input 同沿让位,见
//!   raw_input 同名分支);Back/音量± → 合成 [`HidInput`] 同路;
//! - 健康与降级:伴生断开(EOF/错误)或 stop() 时线程退出并关 tap 模式开关;
//!   宿主用 [`TapHandle::is_serving`] 轮询探测(UAC 拒绝 / 伴生死亡 → 回退
//!   零等待放行,即方案 A 之前的 9 键基线)。

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{ReadFile, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::IO::CancelIoEx;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT,
};

use crate::hid_tap::{edge_action, parse_report, usage_set_event, TapAction, TapParser};
use crate::raw_input::HidInput;
use voicehub_core::mapping::ButtonMapping;

/// tap TCP 监听端口(伴生侧同名常量;独立于 Vibe-Remote 的 30684,避免共存冲突)。
pub const TAP_PORT: u16 = 31727;

/// 伴生进程拉起配置。vid/pid/address 为旧直读路线遗留(伴生现按注册表
/// 硬件 token 定位 WUDFHost,忽略之,保留兼容);port/gadget_dll 为注入路线
/// (方案 B1)所需:gadget 回连端口与 fetch 脚本钉死的 Gadget DLL 路径。
#[derive(Debug, Clone)]
pub struct TapConfig {
    pub pipe_name: String,
    pub exe_path: std::path::PathBuf,
    pub vid: u16,
    pub pid: u16,
    pub address: Option<String>,
    /// 伴生 tap TCP 监听端口(Gadget JS 回连)。
    pub port: u16,
    /// Gadget DLL 路径(%PROGRAMDATA%\VoiceHub\hid-tap\...,fetch 脚本落盘)。
    pub gadget_dll: std::path::PathBuf,
}

/// Gadget DLL 约定路径(与伴生 default_gadget_dll / fetch 脚本一致)。
pub fn default_gadget_dll() -> Option<std::path::PathBuf> {
    let program_data = std::env::var("PROGRAMDATA").ok()?;
    Some(std::path::Path::new(&program_data)
        .join("VoiceHub")
        .join("hid-tap")
        .join("17.15.3-x64-6fca4007b228")
        .join("voicehub-hid-gadget.dll"))
}

/// tap 运行句柄。Drop 即停:置停机位、CancelIoEx 唤醒挂起的管道 IO、收线程。
pub struct TapHandle {
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    /// 服务端句柄值(0 = 未创建/已关闭),停机用 CancelIoEx 唤醒阻塞读。
    handle_value: Arc<AtomicI64>,
    thread: Option<JoinHandle<()>>,
}

impl TapHandle {
    /// 伴生已连上管道且服务线程仍在跑。
    pub fn is_serving(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
            && self.thread.as_ref().is_some_and(|thread| !thread.is_finished())
    }
}

impl Drop for TapHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let value = self.handle_value.swap(0, Ordering::SeqCst);
        if value != 0 {
            // 唤醒阻塞在 ConnectNamedPipe / ReadFile 的服务线程。
            unsafe {
                let _ = CancelIoEx(HANDLE(value as *mut core::ffi::c_void), None);
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 从 WinRT 配对 DeviceInformation Id 提取蓝牙地址(12 位小写 hex)。
/// "BluetoothLE#BluetoothLEbc:09:1b:cc:fc:7b-c0:5d:39:c3:23:cd" →
/// 最后一段 '-' 之后的冒号地址去冒号。
pub fn paired_device_address(paired_id: &str) -> Option<String> {
    let (_, remote) = paired_id.rsplit_once('-')?;
    let octets: Vec<_> = remote.split(':').collect();
    if octets.len() != 6
        || octets
            .iter()
            .any(|part| part.len() != 2 || !part.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(octets.concat().to_ascii_lowercase())
}

/// 合成 tap 事件来源的设备路径:逐字段对齐 raw_input::bluetooth_hid_address
/// 的解析格式(经典蓝牙键盘页 GUID + vid&01xxxx_pid&xxxx_rev&xxxx_12hex),
/// 使 `into_events_for` 的选中设备过滤把 tap 事件认作配对遥控器。
pub fn synthetic_device_path(config: &TapConfig) -> String {
    let address = config.address.as_deref().unwrap_or_default();
    format!(
        r"\\?\hid#{{00001812-0000-1000-8000-00805f9b34fb}}_dev_vid&01{:04x}_pid&{:04x}_rev&0100_{}",
        config.vid, config.pid, address
    )
}

/// 伴生 exe 默认位置:主程序同目录。开发期 target\debug、发行版安装目录
/// 都满足(打包时伴生作为资源与主 exe 同放);可用环境变量覆盖。
pub fn default_companion_path() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("VOICEHUB_HID_TAP_EXE") {
        return Ok(std::path::PathBuf::from(path));
    }
    Ok(std::env::current_exe()
        .map_err(|e| format!("current_exe failed: {e}"))?
        .parent()
        .ok_or("current_exe has no parent")?
        .join("voicehub-hid-tap.exe"))
}

/// 启动 tap 宿主:建管道 → 拉起伴生 → 服务线程等连接/读报文。
/// 失败(管道创建/拉起失败)时返回 Err,调用方保持降级模式。
pub fn start(
    config: TapConfig,
    sender: std::sync::mpsc::Sender<HidInput>,
) -> Result<TapHandle, String> {
    let pipe_path = format!(r"\\.\pipe\{}", config.pipe_name);
    let pipe_w = wide(&pipe_path);
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(pipe_w.as_ptr()),
            PIPE_ACCESS_DUPLEX, // 伴生客户端以读+写打开
            PIPE_TYPE_BYTE | PIPE_WAIT,
            1, // 单实例
            4096,
            4096,
            0,
            None,
        )
    };
    if handle.is_invalid() {
        let error = windows::core::Error::from_thread();
        return Err(format!("create pipe failed: {error}"));
    }

    if let Err(error) = spawn_companion_elevated(&config) {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(error);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let connected = Arc::new(AtomicBool::new(false));
    let handle_value = Arc::new(AtomicI64::new(handle.0 as i64));
    let thread = {
        let stop = stop.clone();
        let connected = connected.clone();
        let handle_value = handle_value.clone();
        let raw = handle.0 as i64;
        std::thread::Builder::new()
            .name("vh-hid-tap".into())
            .spawn(move || serve(HANDLE(raw as *mut core::ffi::c_void), handle_value, stop, connected, config, sender))
            .map_err(|e| format!("spawn tap thread failed: {e}"))?
    };
    Ok(TapHandle { stop, connected, handle_value, thread: Some(thread) })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// 服务线程:等伴生连接 → 读 hex 行 → 沿路由。线程退出(伴生断开 / 停机)
/// 即关 tap 模式开关并清 connected 标志,宿主轮询探测降级。
fn serve(
    handle: HANDLE,
    handle_value: Arc<AtomicI64>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    config: TapConfig,
    sender: std::sync::mpsc::Sender<HidInput>,
) {
    // 等伴生连接。客户端在 ConnectNamedPipe 之前先到时返回 ERROR_PIPE_CONNECTED,
    // 视为已连上。
    let result = unsafe { ConnectNamedPipe(handle, None) };
    let established = result.as_ref().is_ok()
        || result.as_ref().is_err_and(|e| e.code() == ERROR_PIPE_CONNECTED.to_hresult());
    if !established {
        if !stop.load(Ordering::SeqCst) {
            log::warn!("[hid-tap] pipe connect failed: {result:?}");
        }
        unsafe {
            let _ = CloseHandle(handle);
        }
        handle_value.store(0, Ordering::SeqCst);
        connected.store(false, Ordering::SeqCst);
        crate::key_gate::set_tap_mode(false);
        return;
    }
    connected.store(true, Ordering::SeqCst);
    log::info!("[hid-tap] companion connected");

    // 读循环:hex 行解析(方案 B 首启逐键核对阶段保留逐报文打点)。
    let mut parser = TapParser::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let mut read = 0u32;
        let result = unsafe { ReadFile(handle, Some(&mut chunk), Some(&mut read), None) };
        match result {
            Ok(()) if read > 0 => {
                pending.extend_from_slice(&chunk[..read as usize]);
                while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line[..line.len() - 1]);
                    handle_line(line.trim(), &mut parser, &config, &sender);
                }
                if pending.len() > 4096 {
                    // 行界丢失(异常流):丢弃缓冲重同步,防内存膨胀。
                    pending.clear();
                }
            }
            Ok(()) => break, // EOF:伴生退出(停机 / 句柄失效)
            Err(error) => {
                if !stop.load(Ordering::SeqCst) {
                    log::warn!("[hid-tap] pipe read ended: {error}");
                }
                break;
            }
        }
    }
    unsafe {
        let _ = CloseHandle(handle);
    }
    handle_value.store(0, Ordering::SeqCst);
    connected.store(false, Ordering::SeqCst);
    // 无论何种退出路径,立即关 tap 模式:钩子回退零等待放行。
    crate::key_gate::set_tap_mode(false);
}

/// 单行伴生报文处理。P/E = 状态/错误;R = 原始报文(解析 + 沿路由)。
fn handle_line(
    line: &str,
    parser: &mut TapParser,
    config: &TapConfig,
    sender: &std::sync::mpsc::Sender<HidInput>,
) {
    let Some((tag, rest)) = line.split_once(' ') else {
        return;
    };
    match tag {
        "P" => log::info!("[hid-tap] companion interface: {rest}"),
        "E" => log::warn!("[hid-tap] companion error: {rest}"),
        "R" => {
            let bytes = match decode_hex(rest) {
                Some(bytes) => bytes,
                None => {
                    log::warn!("[hid-tap] malformed report line: {rest}");
                    return;
                }
            };
            let Some(usages) = parse_report(&bytes) else {
                log::warn!(
                    "[hid-tap] unrecognized report ({} bytes, id=0x{:02X})",
                    bytes.len(),
                    bytes.first().copied().unwrap_or(0)
                );
                return;
            };
            // 报文到达 = 伴生在服:此刻开 tap 模式(武装先于钩子的前提成立)。
            crate::key_gate::set_tap_mode(true);
            for edge in parser.update(&usages) {
                match edge_action(edge) {
                    TapAction::ArmKeyboard { vk, scan, extended, pressed } => {
                        crate::key_gate::arm_tap_edge(vk, scan, extended, pressed);
                        log::info!(
                            "[hid-tap] arm vk=0x{vk:02X} scan=0x{scan:02X} ext={extended} pressed={pressed}"
                        );
                        // 映射由 tap 通道直驱(L1 2026-09-25:被钩子吞掉的按下沿
                        // 不生成 WM_INPUT,单击只剩孤立释放沿——旧"WM_INPUT 照常
                        // 送达"假设被真机证伪)。与按钮通道同构:合成 UsageSet
                        // 进既有手势/映射/统计管线,tap 掉线由 Raw Input 接棒。
                        let _ = sender.send(HidInput {
                            device_path: synthetic_device_path(config),
                            events: vec![usage_set_event(edge.usage, edge.pressed)],
                        });
                    }
                    TapAction::Button(button) => {
                        log::info!(
                            "[hid-tap] button {} usage=0x{:04X} pressed={}",
                            ButtonMapping::key(button),
                            edge.usage,
                            edge.pressed
                        );
                        let _ = sender.send(HidInput {
                            device_path: synthetic_device_path(config),
                            events: vec![usage_set_event(edge.usage, edge.pressed)],
                        });
                    }
                    TapAction::Unknown => {
                        log::warn!("[hid-tap] unknown usage 0x{:04X} pressed={}", edge.usage, edge.pressed);
                    }
                }
            }
        }
        _ => {}
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.is_empty() || text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// PowerShell `Start-Process -Verb RunAs` 拉起伴生(触发一次 UAC)。
/// 非阻塞:不等 powershell 退出(UAC 弹窗期间会阻塞),成败由 is_serving 探测。
fn spawn_companion_elevated(config: &TapConfig) -> Result<(), String> {
    let arg_list = companion_args(config)
        .iter()
        .map(|arg| format!("'{arg}'"))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "Start-Process -FilePath '{}' -ArgumentList {} -Verb RunAs -WindowStyle Hidden",
        config.exe_path.display(),
        arg_list
    );
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("spawn powershell failed: {e}"))?;
    Ok(())
}

/// 伴生命令行参数(与伴生 main 的成对解析约定一致)。
fn companion_args(config: &TapConfig) -> Vec<String> {
    let mut args = vec![
        "--pipe".to_string(),
        config.pipe_name.clone(),
        "--vid".to_string(),
        format!("{:04x}", config.vid),
        "--pid".to_string(),
        format!("{:04x}", config.pid),
    ];
    if let Some(address) = &config.address {
        args.push("--address".to_string());
        args.push(address.clone());
    }
    args.push("--port".to_string());
    args.push(config.port.to_string());
    args.push("--gadget-dll".to_string());
    args.push(config.gadget_dll.to_string_lossy().to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use voicehub_core::buttons::VoiceKeyHid;

    #[test]
    fn paired_device_address_extracts_last_mac_segment() {
        assert_eq!(
            paired_device_address("BluetoothLE#BluetoothLEbc:09:1b:cc:fc:7b-c0:5d:39:c3:23:cd"),
            Some("c05d39c323cd".to_string())
        );
        // 非法形态:段数/长度/字符不过关。
        assert_eq!(paired_device_address("BluetoothLE#BluetoothLEab-cd"), None);
        assert_eq!(
            paired_device_address("BluetoothLE#BluetoothLEbc-c0:5d:39:cd"),
            None
        );
        assert_eq!(paired_device_address(""), None);
    }

    #[test]
    fn synthetic_path_parses_as_selected_remote() {
        use crate::raw_input::is_selected_remote;
        let paired = "BluetoothLE#BluetoothLEbc:09:1b:cc:fc:7b-c0:5d:39:c3:23:cd";
        let address = paired_device_address(paired).unwrap();
        let config = TapConfig {
            pipe_name: "p".into(),
            exe_path: std::path::PathBuf::from("x.exe"),
            vid: VoiceKeyHid::VENDOR_ID,
            pid: VoiceKeyHid::PRODUCT_ID_RC001,
            address: Some(address),
            port: TAP_PORT,
            gadget_dll: std::path::PathBuf::from("gadget.dll"),
        };
        let path = synthetic_device_path(&config);
        // 合成路径必须被选中设备过滤认作配对遥控器(tap 事件进管线的门票)。
        assert!(is_selected_remote(&path, Some(paired)));
        // 换一个地址:不认。
        assert!(!is_selected_remote(
            &path,
            Some("BluetoothLE#BluetoothLEbc:09:1b:cc:fc:7b-aa:bb:cc:dd:ee:ff")
        ));
    }

    #[test]
    fn decode_hex_pairs() {
        assert_eq!(
            decode_hex("01f1800000"),
            Some(vec![0x01, 0xF1, 0x80, 0x00, 0x00])
        );
        assert_eq!(decode_hex(""), None);
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("zz"), None);
    }

    #[test]
    fn companion_args_match_parser_pairs() {
        let config = TapConfig {
            pipe_name: "voicehub-hid-tap-42".into(),
            exe_path: std::path::PathBuf::from("voicehub-hid-tap.exe"),
            vid: VoiceKeyHid::VENDOR_ID,
            pid: VoiceKeyHid::PRODUCT_ID_RC001,
            address: Some("c05d39c323cd".into()),
            port: TAP_PORT,
            gadget_dll: std::path::PathBuf::from("gadget.dll"),
        };
        let args = companion_args(&config);
        // 伴生按 (flag, value) 成对读取,标志必须成对出现。
        let flags: Vec<_> = args.iter().step_by(2).cloned().collect();
        assert_eq!(
            flags,
            vec!["--pipe", "--vid", "--pid", "--address", "--port", "--gadget-dll"]
        );
        assert_eq!(args[1], "voicehub-hid-tap-42");
        assert_eq!(args[3], format!("{:04x}", VoiceKeyHid::VENDOR_ID));
        assert_eq!(args[5], format!("{:04x}", VoiceKeyHid::PRODUCT_ID_RC001));
        assert_eq!(args[7], "c05d39c323cd");
        assert_eq!(args[9], TAP_PORT.to_string());
        assert_eq!(args[11], "gadget.dll");
    }
}
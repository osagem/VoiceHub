//! VoiceHub 管理员伴生进程:Frida Gadget 注入 WUDF 宿主 + 报文中继。
//!
//! 定位(方案 B1,docs/plan-remote-key-takeover.md §5/§7 第 5 遍补记):用户态
//! 直读键盘 HID 集合被 hidclass 三级权限封死(用户/管理员/SYSTEM 实测均 0x80070005),
//! 唯一可行通道 = 注入 WUDFHost,hook 宿主自己的合法 HID 读取点。
//! 实现逐件移植 Vibe-Remote `frida_hid_tap_injector.py` / `frida_hid_tap_runtime.py` /
//! `frida_compat.py`(L1 生产验证),不做自创改动:
//! - 注册表 `Enum\BTHLEDevice\{00001812}...\Device Parameters\WUDFDiagnosticInfo\HostPid`
//!   定位 WUDFHost;SeDebugPrivilege + CreateRemoteThread(LoadLibraryW) 注入 Gadget DLL;
//! - Gadget JS hook `NtDeviceIoControlFile`(IOCTL 0x80018483),经 TCP 回连转发
//!   9 字节 HidOverGatt 读缓冲(头 `01 00 00` + 6 字节 3×u16 LE usage);
//! - 伴生侧 TCP 监听收 JSON 行,解码后按既有行协议转发:
//!
//! 行协议(全部 ASCII 单行,`\n` 结尾):
//! - `P <信息>`      注入完成(wudfhost pid)
//! - `R <hex>`       一条 HID 报文(01 + 6 字节载荷;解析在主程序侧)
//! - `E <消息>`      状态/错误(等待宿主 / 注入重试 / hook 错误 / 心跳失联)

#[cfg(windows)]
mod imp;

#[cfg(windows)]
fn main() {
    imp::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("voicehub-hid-tap: windows only");
}
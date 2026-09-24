//! 注入 + 中继实现(移植自 Vibe-Remote,见 main.rs 模块注释)。

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, LUID, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_READ, REG_DWORD, REG_QWORD, REG_VALUE_TYPE,
};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetCurrentProcess, GetExitCodeThread, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_ACCESS_RIGHTS, PROCESS_CREATE_THREAD,
    PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// Frida Gadget 版本(与 fetch 脚本钉死的资产一致)。
const GADGET_VERSION: &str = "17.15.3";
/// 解压后 Gadget DLL 的 SHA-256(Vibe-Remote GADGET_DLL_SHA256)。
const GADGET_DLL_SHA256: &str = "6fca4007b2284c765a6c15c967a741f536b5865bf83867326a54029a3b752748";
/// RC003 硬件标识(注册表键名片段;Vibe-Remote RC003_HARDWARE_TOKEN)。
const RC003_HARDWARE_TOKEN: &str = "dev_vid&012717_pid&32b8_rev&00a4";
/// HID 服务键前缀(Vibe-Remote HID_SERVICE_PREFIX)。
const HID_SERVICE_PREFIX: &str = "{00001812-0000-1000-8000-00805f9b34fb}";
/// 注册表枚举根(Vibe-Remote BTHLE_ENUM_KEY)。
const BTHLE_ENUM_KEY: &str = "SYSTEM\\CurrentControlSet\\Enum\\BTHLEDevice";
/// WUDF 诊断键后缀(Vibe-Remote WUDF_DIAGNOSTIC_SUFFIX;本机 L1 验证一致)。
const WUDF_DIAGNOSTIC_SUFFIX: &str = "Device Parameters\\WUDFDiagnosticInfo";
/// tap TCP 监听端口(独立于 Vibe-Remote 的 30684,避免共存冲突)。
pub const TAP_PORT: u16 = 31727;
const GADGET_SCRIPT_NAME: &str = "voicehub-rc003-hid-gadget.js";
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_DELAY: Duration = Duration::from_secs(2);
const PIPE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Gadget JS(Vibe-Remote GADGET_SCRIPT 逐字节拷贝;host/port 由 config 注入)。
const GADGET_SCRIPT: &str = include_str!("rc003_hid_gadget.js");

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// 默认 Gadget DLL 路径:%PROGRAMDATA%\VoiceHub\hid-tap\<ver>-x64-<hash12>\voicehub-hid-gadget.dll
/// (与 output/fetch-frida-gadget.ps1 的落盘布局一致)。
pub fn default_gadget_dll() -> Option<PathBuf> {
    let program_data = std::env::var("PROGRAMDATA").ok()?;
    Some(Path::new(&program_data)
        .join("VoiceHub")
        .join("hid-tap")
        .join(format!("{GADGET_VERSION}-x64-{}", &GADGET_DLL_SHA256[..12]))
        .join("voicehub-hid-gadget.dll"))
}

// ---------- 行中继(伴生 → 主程序,协议不变) ----------

struct Relay {
    handle: HANDLE,
}

impl Relay {
    fn connect(name: &str) -> Result<Relay, String> {
        let path = format!(r"\\.\pipe\{name}");
        let path_w = wide(&path);
        let deadline = Instant::now() + PIPE_CONNECT_TIMEOUT;
        loop {
            let result = unsafe {
                CreateFileW(
                    PCWSTR(path_w.as_ptr()),
                    0x8000_0000 | 0x4000_0000, // GENERIC_READ | GENERIC_WRITE
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            };
            match result {
                Ok(handle) => return Ok(Relay { handle }),
                Err(error) => {
                    if Instant::now() > deadline {
                        return Err(format!("pipe {path} unreachable: {error}"));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    fn send_line(&self, line: &str) -> bool {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        let mut written = 0u32;
        let result = unsafe { WriteFile(self.handle, Some(&bytes), Some(&mut written), None) };
        result.is_ok() && written == bytes.len() as u32
    }
}

// ---------- 注入(Vibe-Remote frida_hid_tap_injector.py 逐函数移植) ----------

/// SeDebugPrivilege(WUDFHost 可拒绝 PROCESS_QUERY_LIMITED_INFORMATION,
/// 必须先启用特权再核对目标;Vibe-Remote enable_debug_privilege)。
fn enable_debug_privilege() -> Result<(), String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| format!("OpenProcessToken: {e}"))?;
        let result = (|| {
            let mut luid = LUID::default();
            LookupPrivilegeValueW(None, windows::core::w!("SeDebugPrivilege"), &mut luid)
                .map_err(|e| format!("LookupPrivilegeValueW: {e}"))?;
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None)
                .map_err(|e| format!("AdjustTokenPrivileges: {e}"))?;
            let error = GetLastError();
            if error == windows::Win32::Foundation::ERROR_NOT_ALL_ASSIGNED {
                return Err("SeDebugPrivilege is not assigned".into());
            }
            Ok(())
        })();
        let _ = CloseHandle(token);
        result
    }
}

/// 目标进程名核对,拒绝非 WUDFHost(Vibe-Remote _target_process_name)。
fn target_process_name(pid: u32) -> Result<String, String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| format!("OpenProcess({pid}): {e}"))?;
        let mut buffer = [0u16; 512];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        result.map_err(|e| format!("QueryFullProcessImageNameW: {e}"))?;
        Ok(String::from_utf16_lossy(&buffer[..length as usize]))
    }
}

/// 经典 DLL 注入:VirtualAllocEx → WriteProcessMemory → CreateRemoteThread(LoadLibraryW)
/// (Vibe-Remote inject_library;等待 20s,退出码 0 = 加载失败)。
fn inject_library(pid: u32, dll_path: &Path) -> Result<(), String> {
    const RIGHTS: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(
        PROCESS_CREATE_THREAD.0
            | PROCESS_QUERY_INFORMATION.0
            | PROCESS_VM_OPERATION.0
            | PROCESS_VM_WRITE.0
            | PROCESS_VM_READ.0,
    );
    unsafe {
        let process =
            OpenProcess(RIGHTS, false, pid).map_err(|e| format!("OpenProcess({pid}): {e}"))?;
        let outcome = (|| {
            let encoded: Vec<u8> = dll_path
                .to_string_lossy()
                .encode_utf16()
                .chain(Some(0))
                .flat_map(|unit| unit.to_le_bytes())
                .collect();
            let remote_path = VirtualAllocEx(
                process,
                None,
                encoded.len(),
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            if remote_path.is_null() {
                return Err(format!(
                    "VirtualAllocEx: {}",
                    windows::core::Error::from_thread()
                ));
            }
            let result = (|| {
                WriteProcessMemory(
                    process,
                    remote_path.cast(),
                    encoded.as_ptr().cast(),
                    encoded.len(),
                    None,
                )
                .map_err(|e| format!("WriteProcessMemory: {e}"))?;
                let kernel = GetModuleHandleW(PCWSTR(wide("kernel32.dll").as_ptr()))
                    .map_err(|e| format!("GetModuleHandleW: {e}"))?;
                let load_library = GetProcAddress(kernel, windows::core::s!("LoadLibraryW"))
                    .ok_or_else(|| "GetProcAddress(LoadLibraryW): null".to_string())?;
                let start: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 =
                    std::mem::transmute(load_library);
                let thread = CreateRemoteThread(process, None, 0, Some(start), Some(remote_path.cast()), 0, None)
                    .map_err(|e| format!("CreateRemoteThread: {e}"))?;
                let outcome = (|| {
                    if WaitForSingleObject(thread, 20_000) != WAIT_OBJECT_0 {
                        return Err("remote LoadLibraryW timed out".into());
                    }
                    let mut exit_code = 0u32;
                    GetExitCodeThread(thread, &mut exit_code)
                        .map_err(|e| format!("GetExitCodeThread: {e}"))?;
                    if exit_code == 0 {
                        return Err("remote LoadLibraryW returned NULL".into());
                    }
                    Ok(())
                })();
                let _ = CloseHandle(thread);
                outcome
            })();
            let _ = VirtualFreeEx(process, remote_path, 0, MEM_RELEASE);
            result
        })();
        let _ = CloseHandle(process);
        outcome
    }
}

/// 注册表定位 WUDFHost PID(Vibe-Remote find_rc003_hidogatt_host_pid;键路径按本机
/// L1:`<设备>\<实例>\Device Parameters\WUDFDiagnosticInfo\HostPid`)。
fn find_host_pid() -> Option<u32> {
    unsafe {
        let mut root = HKEY::default();
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(wide(BTHLE_ENUM_KEY).as_ptr()),
            None,
            KEY_READ,
            &mut root,
        ) != windows::Win32::Foundation::ERROR_SUCCESS
        {
            return None;
        }
        let result = find_host_pid_under(root);
        let _ = RegCloseKey(root);
        result
    }
}

fn find_host_pid_under(root: HKEY) -> Option<u32> {
    unsafe {
        let mut index = 0u32;
        loop {
            let mut name = [0u16; 256];
            let mut length = name.len() as u32;
            if RegEnumKeyExW(
                root,
                index,
                Some(windows::core::PWSTR(name.as_mut_ptr())),
                &mut length,
                None,
                None,
                None,
                None,
            ) != windows::Win32::Foundation::ERROR_SUCCESS
            {
                return None;
            }
            index += 1;
            let key_name = String::from_utf16_lossy(&name[..length as usize]);
            let folded = key_name.to_ascii_lowercase();
            if !(folded.starts_with(&HID_SERVICE_PREFIX.to_ascii_lowercase())
                && folded.contains(RC003_HARDWARE_TOKEN))
            {
                continue;
            }
            let mut service_key = HKEY::default();
            if RegOpenKeyExW(
                root,
                PCWSTR(wide(&key_name).as_ptr()),
                None,
                KEY_READ,
                &mut service_key,
            ) != windows::Win32::Foundation::ERROR_SUCCESS
            {
                continue;
            }
            let pid = find_host_pid_in_service(service_key);
            let _ = RegCloseKey(service_key);
            if pid.is_some() {
                return pid;
            }
        }
    }
}

fn find_host_pid_in_service(service_key: HKEY) -> Option<u32> {
    unsafe {
        let mut index = 0u32;
        loop {
            let mut name = [0u16; 256];
            let mut length = name.len() as u32;
            if RegEnumKeyExW(
                service_key,
                index,
                Some(windows::core::PWSTR(name.as_mut_ptr())),
                &mut length,
                None,
                None,
                None,
                None,
            ) != windows::Win32::Foundation::ERROR_SUCCESS
            {
                return None;
            }
            index += 1;
            let instance = String::from_utf16_lossy(&name[..length as usize]);
            let diag_path = format!("{instance}\\{WUDF_DIAGNOSTIC_SUFFIX}");
            let mut diag_key = HKEY::default();
            if RegOpenKeyExW(
                service_key,
                PCWSTR(wide(&diag_path).as_ptr()),
                None,
                KEY_READ,
                &mut diag_key,
            ) != windows::Win32::Foundation::ERROR_SUCCESS
            {
                continue;
            }
            let pid = query_host_pid(diag_key);
            let _ = RegCloseKey(diag_key);
            if let Some(pid) = pid.filter(|pid| *pid > 0) {
                return Some(pid);
            }
        }
    }
}

fn query_host_pid(diag_key: HKEY) -> Option<u32> {
    unsafe {
        let mut kind = REG_VALUE_TYPE::default();
        // L1(2026-09-25 dump-bthle.ps1):本机 HostPid 是 REG_QWORD(8 字节),
        // 不是 REG_DWORD —— 按 QWORD 读,DWORD 兼容收窄(Python QueryValueEx
        // 对两种类型都返回 int,此处显式双类型)。
        let mut value = 0u64;
        let mut size = std::mem::size_of::<u64>() as u32;
        let status = RegQueryValueExW(
            diag_key,
            PCWSTR(wide("HostPid").as_ptr()),
            None,
            Some(&mut kind),
            Some(std::ptr::addr_of_mut!(value).cast::<u8>()),
            Some(&mut size),
        );
        if status != windows::Win32::Foundation::ERROR_SUCCESS {
            return None;
        }
        let pid = match kind {
            REG_QWORD => value as u32,
            REG_DWORD => (value & 0xFFFF_FFFF) as u32,
            _ => return None,
        };
        (pid > 0).then_some(pid)
    }
}

/// 注入前置检查 + 注入(Vibe-Remote inject_current_process):
/// 目标必须仍是当前注册表记录的 WUDFHost;特权先开;非 wudfhost.exe 拒绝;
/// Gadget DLL 哈希复核后才注入。
fn inject_current(pid: u32, port: u16, dll_path: &Path) -> Result<(), String> {
    if find_host_pid() != Some(pid) {
        return Err(format!("RC003 host changed before injection: requested={pid}"));
    }
    enable_debug_privilege()?;
    let name = target_process_name(pid)?;
    let file_name = name.rsplit(['\\', '/']).next().unwrap_or("");
    if !file_name.eq_ignore_ascii_case("wudfhost.exe") {
        return Err(format!("refusing non-WUDFHost target: {name}"));
    }
    prepare_runtime(dll_path, port)?;
    inject_library(pid, dll_path)
}

/// Gadget 运行时准备(Vibe-Remote prepare_secure_runtime):DLL 哈希复核、
/// 写 config/JS、icacls 锁 ACL(SYSTEM/Admins 全控,Users 只读)。
fn prepare_runtime(dll_path: &Path, port: u16) -> Result<(), String> {
    let dll_hash = sha256_hex(dll_path)?;
    if dll_hash != GADGET_DLL_SHA256 {
        return Err(format!("verified Gadget changed before injection: {dll_hash}"));
    }
    let dir = dll_path.parent().ok_or("gadget dll has no parent dir")?;
    let stem = dll_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("gadget dll name invalid")?;
    let script_path = dir.join(GADGET_SCRIPT_NAME);
    write_if_changed(&script_path, GADGET_SCRIPT.as_bytes())?;
    let config_path = dir.join(format!("{stem}.config"));
    write_if_changed(&config_path, gadget_config_text(port).as_bytes())?;
    lock_runtime_acl(dir, true)?;
    lock_runtime_acl(&script_path, false)?;
    lock_runtime_acl(&config_path, false)?;
    lock_runtime_acl(dll_path, false)?;
    Ok(())
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    match std::fs::read(path) {
        Ok(existing) if existing == bytes => return Ok(()),
        _ => {}
    }
    std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

fn gadget_config_text(port: u16) -> String {
    // 与 Vibe-Remote gadget_config_text 同构(DLL 同名 .config,脚本经 config 指路)。
    format!(
        "{{\n  \"interaction\": {{\n    \"type\": \"script\",\n    \"path\": \"{GADGET_SCRIPT_NAME}\",\n    \"parameters\": {{\"host\": \"127.0.0.1\", \"port\": {port}}},\n    \"on_change\": \"ignore\"\n  }},\n  \"runtime\": \"qjs\",\n  \"teardown\": \"minimal\"\n}}\n"
    )
}

fn lock_runtime_acl(path: &Path, directory: bool) -> Result<(), String> {
    let suffix = if directory { "(OI)(CI)" } else { "" };
    let mut command = Command::new("icacls");
    command
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            &format!("*S-1-5-18:{suffix}F"),
            &format!("*S-1-5-32-544:{suffix}F"),
            &format!("*S-1-5-32-545:{suffix}RX"),
            "/C",
            "/Q",
        ])
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let output = command
        .output()
        .map_err(|e| format!("icacls spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to secure Gadget runtime ACL: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        ));
    }
    Ok(())
}

// ---------- tap 会话(Vibe-Remote RC003HidReportTap._run_loop 移植) ----------

/// 单条会话的退出意图。
#[derive(Debug, PartialEq, Eq)]
enum SessionExit {
    /// 普通行处理完毕,继续。
    Continue,
    /// 心跳/就绪行,刷新心跳计时。
    Heartbeat,
    /// 管道死亡,主程序要我们退。
    AppExit,
}

/// 处理一条 gadget JSON 行;报文经 `R` 行转发(01 + 6 字节载荷)。
fn handle_gadget_line(relay: &Relay, line: &str) -> SessionExit {
    match json_str_field(line, "kind").unwrap_or_default() {
        "heartbeat" | "ready" => SessionExit::Heartbeat,
        "gatt_read" => {
            let raw = json_str_field(line, "raw").unwrap_or_default();
            let payload = hex_decode(raw)
                .as_deref()
                .and_then(decode_ioctl_output)
                .map(|payload| payload.to_vec());
            match payload {
                Some(payload) => {
                    if !relay.send_line(&report_line(&payload)) {
                        return SessionExit::AppExit;
                    }
                }
                None => {
                    relay.send_line(&format!("E tap bad report raw={raw}"));
                }
            }
            SessionExit::Continue
        }
        "error" => {
            let message = json_str_field(line, "message").unwrap_or_default();
            relay.send_line(&format!("E hook_error={message}"));
            SessionExit::Continue
        }
        _ => SessionExit::Continue,
    }
}

/// 6 字节 usage 载荷 → `R <hex(01‖载荷)>` 行(主程序 parse_report 约定:首字节 report_id=1)。
fn report_line(payload: &[u8]) -> String {
    let mut report = Vec::with_capacity(payload.len() + 1);
    report.push(1u8);
    report.extend_from_slice(payload);
    let hex: String = report.iter().map(|b| format!("{b:02x}")).collect();
    format!("R {hex}")
}

/// HidOverGatt 读缓冲 → 6 字节 usage 载荷(Vibe-Remote decode_rc003_ioctl_output)。
fn decode_ioctl_output(data: &[u8]) -> Option<&[u8]> {
    if data.len() != 9 || data[..3] != [0x01, 0x00, 0x00] {
        return None;
    }
    Some(&data[3..9])
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok())
        .collect()
}

/// 从扁平 gadget JSON 行提取字符串字段(自产报文,足够)。
fn json_str_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let rest = &line[line.find(&needle)? + needle.len()..];
    let rest = &rest[rest.find(':')? + 1..];
    let rest = &rest[rest.find('"')? + 1..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

pub fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pipe = String::new();
    let mut port = TAP_PORT;
    let mut gadget_dll = default_gadget_dll();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pipe" => pipe = args.get(i + 1).cloned().unwrap_or_default(),
            "--port" => port = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(port),
            "--gadget-dll" => gadget_dll = args.get(i + 1).map(PathBuf::from),
            _ => {} // --vid/--pid/--address:旧参数,注入路线不再需要,忽略
        }
        i += 2;
    }
    let Some(dll_path) = gadget_dll.filter(|p| p.is_file()) else {
        eprintln!("gadget dll missing; run output/fetch-frida-gadget.ps1 first");
        return;
    };
    let relay = match Relay::connect(&pipe) {
        Ok(relay) => relay,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };

    // 先备运行时(资产缺失/哈希不符时重试无意义,直接退让主程序降级)。
    if let Err(error) = prepare_runtime(&dll_path, port) {
        relay.send_line(&format!("E prepare_runtime: {error}"));
        return;
    }

    // 注入 + 接受 + 会话循环(Vibe-Remote _run_loop 结构)。
    let mut injected_pid: Option<u32> = None;
    loop {
        let Some(pid) = find_host_pid() else {
            relay.send_line("E waiting_for_rc003_host");
            std::thread::sleep(RETRY_DELAY);
            continue;
        };
        if injected_pid != Some(pid) {
            match inject_current(pid, port, &dll_path) {
                Ok(()) => {
                    injected_pid = Some(pid);
                    relay.send_line(&format!("P wudfhost-inject:{pid}"));
                }
                Err(error) => {
                    relay.send_line(&format!("E injection_retry {error}"));
                    std::thread::sleep(RETRY_DELAY);
                    continue;
                }
            }
        }
        let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => listener,
            Err(error) => {
                relay.send_line(&format!("E tcp bind failed: {error}"));
                std::thread::sleep(RETRY_DELAY);
                continue;
            }
        };
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) => {
                relay.send_line(&format!("E tcp accept failed: {error}"));
                drop(listener);
                std::thread::sleep(RETRY_DELAY);
                continue;
            }
        };
        drop(listener);
        relay.send_line("E ATTACHED awaiting_io");
        let exit = serve_session(&relay, stream, pid);
        if exit == SessionExit::AppExit {
            return;
        }
        // 宿主换了才重注;同一宿主的心跳失联/gadget 回连不重注(Vibe-Remote 同)。
        if find_host_pid() != Some(pid) {
            injected_pid = None;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// 单条 gadget 连接的会话循环:JSON 行解析 + 心跳超时 + 宿主更替检查。
fn serve_session(relay: &Relay, stream: TcpStream, pid: u32) -> SessionExit {
    if stream.set_read_timeout(Some(Duration::from_secs(1))).is_err() {
        return SessionExit::Continue;
    }
    let mut reader = BufReader::new(stream.try_clone().expect("tcp try_clone"));
    let mut last_heartbeat = Instant::now();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return SessionExit::Continue,
            Ok(_) => {}
            Err(_) => {}
        }
        if let Some(stripped) = line.strip_suffix('\n') {
            match handle_gadget_line(relay, stripped) {
                SessionExit::AppExit => return SessionExit::AppExit,
                SessionExit::Heartbeat => last_heartbeat = Instant::now(),
                SessionExit::Continue => {}
            }
            line.clear();
        }
        if last_heartbeat.elapsed() >= HEARTBEAT_TIMEOUT {
            relay.send_line("E UNHEALTHY agent_heartbeat_stale");
            return SessionExit::Continue;
        }
        if find_host_pid() != Some(pid) {
            relay.send_line("E HOST_CHANGED");
            return SessionExit::Continue;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_ioctl_output_validates_header_and_length() {
        // 9 字节:头 01 00 00 + 6 字节载荷(0xF1 back)。
        let ok = hex_decode("010000f10000000000").unwrap();
        assert_eq!(decode_ioctl_output(&ok), Some(&[0xf1u8, 0, 0, 0, 0, 0][..]));
        // 头不对 / 长度不对 → None。
        assert_eq!(
            hex_decode("020000f10000000000")
                .as_deref()
                .and_then(decode_ioctl_output),
            None
        );
        assert_eq!(
            hex_decode("010000f10000")
                .as_deref()
                .and_then(decode_ioctl_output),
            None
        );
        assert_eq!(hex_decode("zz"), None);
    }

    #[test]
    fn report_line_prefixes_report_id() {
        assert_eq!(report_line(&[0xf1, 0, 0, 0, 0, 0]), "R 01f10000000000");
    }

    #[test]
    fn json_str_field_extracts_from_flat_lines() {
        let line = r#"{"kind": "gatt_read", "raw": "0100"}"#;
        assert_eq!(json_str_field(line, "kind"), Some("gatt_read"));
        assert_eq!(json_str_field(line, "raw"), Some("0100"));
        assert_eq!(json_str_field(line, "missing"), None);
    }

    #[test]
    fn gadget_config_text_carries_script_and_port() {
        let config = gadget_config_text(31727);
        assert!(config.contains(&format!("\"port\": {TAP_PORT}")));
        assert!(config.contains(GADGET_SCRIPT_NAME));
        assert!(config.contains("\"runtime\": \"qjs\""));
    }

    #[test]
    fn default_gadget_dll_matches_fetch_script_layout() {
        let dll = default_gadget_dll().expect("PROGRAMDATA present");
        let path = dll.to_string_lossy().to_lowercase();
        assert!(path.contains("voicehub\\hid-tap\\17.15.3-x64-6fca4007b228"));
        assert!(path.ends_with("voicehub-hid-gadget.dll"));
    }

    #[test]
    fn hex_decode_pairs_and_rejects_odd_length() {
        assert_eq!(hex_decode("01f1"), Some(vec![0x01, 0xf1]));
        assert_eq!(hex_decode("0"), None);
        assert_eq!(hex_decode("zz"), None);
    }
}
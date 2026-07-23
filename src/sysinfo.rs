use std::net::UdpSocket;
use std::process::Command;
use std::sync::LazyLock;
use std::time::Instant;

#[cfg(windows)]
unsafe extern "system" {
    /// 系統開機以來經過的毫秒數（`kernel32.dll`），Windows 內建就有，不需要
    /// 額外的 crate。
    fn GetTickCount64() -> u64;

    /// 取得電腦名稱（`kernel32.dll`）。`lpBuffer` 是呼叫端配好的緩衝區，
    /// `lpnSize` 進去時是緩衝區大小、回來時是實際寫入的字元數（不含結尾的
    /// null）；失敗（例如緩衝區太小）回傳 0。
    fn GetComputerNameW(lpBuffer: *mut u16, lpnSize: *mut u32) -> i32;

    /// `pid_alive` 用：開一個能查詢（但不需要完整控制權限）目標行程的
    /// handle，開不起來（行程不存在/沒權限）回傳 null。
    fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut std::os::raw::c_void;

    /// 查 `OpenProcess` 開到的行程目前的結束代碼；還在跑的話會是 `STILL_ACTIVE`
    /// （259）。
    fn GetExitCodeProcess(hProcess: *mut std::os::raw::c_void, lpExitCode: *mut u32) -> i32;

    fn CloseHandle(hObject: *mut std::os::raw::c_void) -> i32;

    /// `local_hms` 用：把一個 UTC `FILETIME` 轉成本地時區的 `FILETIME`
    /// （`kernel32.dll`）。
    fn FileTimeToLocalFileTime(lpFileTime: *const FileTimeRaw, lpLocalFileTime: *mut FileTimeRaw) -> i32;

    /// `local_hms` 用：把 `FILETIME` 拆成年月日時分秒（`kernel32.dll`）。
    fn FileTimeToSystemTime(lpFileTime: *const FileTimeRaw, lpSystemTime: *mut SystemTimeRaw) -> i32;
}

#[cfg(windows)]
#[repr(C)]
struct FileTimeRaw {
    dw_low_date_time: u32,
    dw_high_date_time: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct SystemTimeRaw {
    w_year: u16,
    w_month: u16,
    w_day_of_week: u16,
    w_day: u16,
    w_hour: u16,
    w_minute: u16,
    w_second: u16,
    w_milliseconds: u16,
}

#[cfg(not(windows))]
unsafe extern "C" {
    /// 取得主機名稱（libc），標準函式庫沒有直接包這個所以用 FFI 呼叫，不
    /// 需要額外的 crate。
    fn gethostname(name: *mut std::os::raw::c_char, len: usize) -> i32;

    /// `pid_alive` 用：`kill(pid, 0)` 不會真的送訊號，只檢查行程存不存在/
    /// 有沒有權限送訊號給它，成功（回傳 0）就代表還活著。
    fn kill(pid: i32, sig: i32) -> i32;

    /// `is_foreground_tty` 用：查 `fd` 這個終端機目前的 foreground process
    /// group 是誰。不是終端機（例如純管線）或查詢失敗都回傳 -1。
    fn tcgetpgrp(fd: i32) -> i32;

    /// `is_foreground_tty` 用：查這個行程自己的 process group id。
    fn getpgrp() -> i32;

    /// `local_hms` 用：把 unix 秒數轉成本地時區的年月日時分秒（libc）。只填
    /// 我們需要的欄位（`tm_hour`/`tm_min`/`tm_sec`），但 `TmRaw` 完整宣告到
    /// glibc/BSD 的 `tm_gmtoff`/`tm_zone` 擴充欄位——`localtime_r` 是照系統
    /// 實際的 `struct tm` 大小寫入，如果我們宣告的型別比實際小，會寫出邊界
    /// 覆蓋到不屬於這個 struct 的記憶體。
    fn localtime_r(timep: *const i64, result: *mut TmRaw) -> *mut TmRaw;
}

#[cfg(not(windows))]
#[repr(C)]
#[derive(Default)]
struct TmRaw {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const std::os::raw::c_char,
}

/// 這個程式行程真正啟動的時間點，第一次被讀到的當下算（`LazyLock` 保證只
/// 初始化一次）。因為每個 plugin 都是程式一啟動就建好（見 `main.rs`），不同
/// plugin 第一次讀到這個值的時間點差異在毫秒等級，對「跑了多久」這種顯示
/// 用途可以忽略不計。
static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// 把秒數格式化成 `1d 02:03:04` 或 `02:03:04`（不到一天就不顯示天數）。
pub fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!("{days}d {hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

/// 這個程式（行程）本身跑了多久，從 `PROCESS_START` 算起。
pub fn app_uptime_secs() -> u64 {
    PROCESS_START.elapsed().as_secs()
}

/// 把 unix 秒數格式化成本地時區的 `hh:mm:ss`，給 `activities` plugin 顯示活動
/// 紀錄的時間戳用——跟 `main::log_respawn` 故意用原始秒數不同（那是寫進 log
/// 檔案事後查的，不在乎好不好讀），這裡是印在畫面上給人看的，需要看得懂的
/// 時分秒。查本地時區用系統 API（Windows 的
/// `FileTimeToLocalFileTime`/`FileTimeToSystemTime`，其他平台的
/// `localtime_r`），不為了這個引入額外的日期時間 crate。查詢失敗（理論上
/// 不會發生）就退回顯示 UTC，至少還是看得懂的 `hh:mm:ss`，不是原始秒數。
#[cfg(windows)]
pub fn local_hms(ts_secs: u64) -> String {
    // FILETIME 是從 1601-01-01 起算的 100 奈秒間隔數，跟 unix epoch
    // （1970-01-01）之間差 116444736000000000 個間隔，這是換算 FILETIME/unix
    // 時間戳的標準常數。
    const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;
    let ticks = ts_secs.saturating_mul(10_000_000).saturating_add(UNIX_EPOCH_AS_FILETIME);
    let utc = FileTimeRaw { dw_low_date_time: (ticks & 0xFFFF_FFFF) as u32, dw_high_date_time: (ticks >> 32) as u32 };
    let mut local = FileTimeRaw { dw_low_date_time: 0, dw_high_date_time: 0 };
    let mut sys = SystemTimeRaw::default();
    let ok = unsafe {
        FileTimeToLocalFileTime(&utc, &mut local) != 0 && FileTimeToSystemTime(&local, &mut sys) != 0
    };
    if !ok {
        return utc_hms_fallback(ts_secs);
    }
    format!("{:02}:{:02}:{:02}", sys.w_hour, sys.w_minute, sys.w_second)
}

#[cfg(not(windows))]
pub fn local_hms(ts_secs: u64) -> String {
    let ts = ts_secs as i64;
    let mut tm = TmRaw::default();
    let ok = unsafe { !localtime_r(&ts, &mut tm).is_null() };
    if !ok {
        return utc_hms_fallback(ts_secs);
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn utc_hms_fallback(ts_secs: u64) -> String {
    let secs_of_day = ts_secs % 86400;
    format!("{:02}:{:02}:{:02}", secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60)
}

/// 裝置（作業系統）開機多久了，跟程式本身跑了多久（`app_uptime_secs`）是兩
/// 回事——程式可能是開機後很久才啟動的。Windows 下直接呼叫 `GetTickCount64`
/// （系統開機以來的毫秒數），是很快的本地呼叫，不需要開子行程或快取。
#[cfg(windows)]
pub fn device_uptime_secs() -> u64 {
    unsafe { GetTickCount64() / 1000 }
}

#[cfg(not(windows))]
pub fn device_uptime_secs() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0)
}

/// 用 UDP「連線」（不會真的送出封包，只是讓作業系統決定路由要走哪個介面）
/// 反查對外連線時會用的本機 ip，藉此當作「最具代表性」的 ip。純本地運算、
/// 不牽涉真正的網路 I/O，查不到（例如完全沒有網路介面）就回傳 "N/A"。
pub fn local_ip() -> String {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("8.8.8.8:80")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "N/A".to_string())
}

/// 這台機器的名字，拿來當 device registry 裡的識別碼。直接查系統 API
/// （Windows 用 `GetComputerNameW`，其他平台用 `gethostname`），不吃環境
/// 變數——`HOSTNAME` 這類變數不一定會被 export 給子行程（例如服務、排程、
/// 非互動式 shell 啟動時），查不到才給一個看得懂的預設值，同一個區網/實驗
/// 室裡機器名稱通常不會重複，這裡不做更嚴謹的 UUID 之類的機制。
#[cfg(windows)]
pub fn hostname() -> String {
    const MAX_COMPUTERNAME_LENGTH: usize = 31;
    let mut buf = [0u16; MAX_COMPUTERNAME_LENGTH + 1];
    let mut len = buf.len() as u32;
    let ok = unsafe { GetComputerNameW(buf.as_mut_ptr(), &mut len) };
    if ok != 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        "未知主機".to_string()
    }
}

#[cfg(not(windows))]
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ok = unsafe { gethostname(buf.as_mut_ptr() as *mut _, buf.len()) };
    if ok == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        "未知主機".to_string()
    }
}

/// 檢查 `pid` 這個行程是不是還在跑。給 `upgrade` 指令的重啟流程用（見
/// `shell::run_upgrade`/`main.rs` 的 `--respawn-after`）：舊行程要真的完全
/// 結束、放開 port 9759 之後，才能啟動新編出來的執行檔，不能只靠猜一個固定
/// 時間就當作「應該結束了」。
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}

#[cfg(not(windows))]
pub fn pid_alive(pid: u32) -> bool {
    unsafe { kill(pid as i32, 0) == 0 }
}

/// 這個行程現在是不是 stdin 那個終端機的 foreground process group——不是單純
/// 「stdin 是不是 tty」：像 `upgrade` 重新呼叫自己那種情境，新行程的 stdin
/// 繼承自原本那個終端機，`isatty()` 檢查會過，但這個新行程已經不在那個終端機
/// 目前的 foreground process group 裡了（原本的 shell 在中間那段 process 結束
/// 的瞬間就把控制權收回去）。這種狀態下對終端機做需要控制權的操作（例如
/// crossterm/rustyline 的 raw mode）會撞上 Unix 對 background process group
/// 動終端機的行為，回傳 `Input/output error`，不是單純「沒有 tty」那種可以
/// 忽略的情況。`main.rs` 用這個判斷要不要走 headless 模式（見
/// `main::run_headless`），不是只看 stdin 是不是 tty。
///
/// `tcgetpgrp` 查不到（不是 tty，或這個行程沒有 controlling terminal）都回傳
/// -1，跟 `getpgrp()` 不會相等，一併視為「不是 foreground」，不需要另外判斷
/// 是不是 tty。Windows 沒有這套 Unix session/job control 的概念，永遠當作
/// foreground（GUI/CLI 在 Windows 上不會遇到這個問題）。
#[cfg(windows)]
pub fn is_foreground_tty() -> bool {
    true
}

#[cfg(not(windows))]
pub fn is_foreground_tty() -> bool {
    unsafe {
        let pgrp = tcgetpgrp(0);
        pgrp >= 0 && pgrp == getpgrp()
    }
}

/// 這台機器的作業系統（`linux`/`windows`/`macos`...），拿來給 device/global
/// registry 顯示用。`std::env::consts::OS` 是編譯期常數（編出來給哪個平台跑，
/// 就是哪個值），不需要另外查系統 API 或開子行程。
pub fn os() -> &'static str {
    std::env::consts::OS
}

/// 執行 `tailscale ip -4` 取得目前的 tailscale IPv4 位址；沒安裝、沒登入或
/// 沒有位址（指令失敗或沒輸出）都算沒有，回傳 `None`。這是會真的開子行程的
/// 呼叫，呼叫端（見 `plugins::system::TailscaleCache`）要自己做快取，不要在
/// 持有鎖或高頻率呼叫的地方直接呼叫。
pub fn fetch_tailscale_ip() -> Option<String> {
    Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 目前系統 ARP 快取裡看得到的 mac 位址（小寫、`:` 分隔），拿來判斷「最近有
/// 沒有在這個網段活動過」（見 `plugins::wol::WolPlugin::status`）。只讀本地
/// 核心/系統既有的 ARP 表，不會主動送任何封包，所以查不到不代表真的沒開機，
/// 只是最近沒跟這台機器通訊過、ARP 項目過期了。
#[cfg(not(windows))]
pub fn arp_table() -> Vec<String> {
    std::fs::read_to_string("/proc/net/arp")
        .unwrap_or_default()
        .lines()
        .skip(1) // 第一行是欄位標題（IP address / HW type / Flags / HW address / ...）。
        .filter_map(|line| line.split_whitespace().nth(3))
        .filter(|mac| *mac != "00:00:00:00:00:00")
        .map(str::to_lowercase)
        .collect()
}

#[cfg(windows)]
pub fn arp_table() -> Vec<String> {
    let Ok(output) = Command::new("arp").arg("-a").output() else { return Vec::new() };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            // Windows 的 `arp -a` 輸出裡 mac 位址用 `-` 分隔（例如
            // `aa-bb-cc-dd-ee-ff`），找這一行裡符合這個外形的欄位。
            line.split_whitespace()
                .find(|token| token.len() == 17 && token.matches('-').count() == 5)
        })
        .map(|mac| mac.replace('-', ":").to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn pid_alive_reflects_process_lifetime() {
        let mut child = if cfg!(windows) {
            Command::new("cmd").args(["/c", "timeout /t 5"]).spawn().expect("spawn timeout")
        } else {
            Command::new("sleep").arg("5").spawn().expect("spawn sleep")
        };
        let pid = child.id();
        assert!(pid_alive(pid), "剛 spawn 的子行程應該還活著");
        child.kill().expect("kill child");
        child.wait().expect("wait for child");
        // 送出結束訊號跟核心真的回收行程之間可能有極短暫的延遲，重試幾次
        // 避免測試偶爾閃燁失敗。
        let mut still_alive = pid_alive(pid);
        for _ in 0..20 {
            if !still_alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            still_alive = pid_alive(pid);
        }
        assert!(!still_alive, "kill + wait 之後應該回報已經不在了");
    }

    #[test]
    fn pid_alive_false_for_bogus_pid() {
        // 挑一個幾乎不可能存在的超大 pid，確認查不到的情況回傳 false 而不是
        // panic（`OpenProcess`/`kill` 對不存在的 pid 都應該乾淨地失敗）。
        assert!(!pid_alive(u32::MAX - 1));
    }

    #[test]
    fn local_hms_matches_system_clock() {
        // 跟系統既有的 `date` 指令比對，而不是自己重算一次時區轉換邏輯
        // （那樣測試只是在驗證「程式碼跟自己一致」，沒有測到真正的時區/FFI
        // struct layout 對不對）。取兩次 `date` 之間的中間值當「現在」，容忍
        // 呼叫之間的時間差；`hh:mm:ss` 用字串比較，跨分鐘/跨小時邊界時字串
        // 前後可能差 1 秒不相等，重試幾次避免這種邊界情況造成偶發失敗。
        fn now_via_date() -> (u64, String) {
            let epoch = Command::new("date").arg("+%s").output().expect("date +%s");
            let epoch: u64 = String::from_utf8_lossy(&epoch.stdout).trim().parse().expect("解析 date +%s 輸出");
            let hms = Command::new("date").arg("+%H:%M:%S").output().expect("date +%H:%M:%S");
            let hms = String::from_utf8_lossy(&hms.stdout).trim().to_string();
            (epoch, hms)
        }

        for attempt in 0..5 {
            let (epoch, expected_hms) = now_via_date();
            let actual_hms = local_hms(epoch);
            if actual_hms == expected_hms {
                return;
            }
            if attempt == 4 {
                panic!("local_hms({epoch}) = {actual_hms}，跟系統 date 指令回報的 {expected_hms} 對不起來");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

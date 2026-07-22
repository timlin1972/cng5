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
}

#[cfg(not(windows))]
unsafe extern "C" {
    /// 取得主機名稱（libc），標準函式庫沒有直接包這個所以用 FFI 呼叫，不
    /// 需要額外的 crate。
    fn gethostname(name: *mut std::os::raw::c_char, len: usize) -> i32;
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

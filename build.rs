/// `system` plugin 的 `version` 指令要顯示這個 build 是什麼時候編譯的（見
/// `src/plugins/system.rs`），但 Rust 標準函式庫本身沒有「編譯當下的時間」這種
/// 東西——只能靠 build script 在編譯當下先算好，透過 `cargo:rustc-env` 塞進一個
/// 環境變數，執行時再用 `env!` 讀出來當常數用。原本借用系統的 `date` 指令格式化，
/// 但 Windows 上沒有這個指令，改成純 Rust 用 `SystemTime` 自己算 UTC 曆法
/// （Howard Hinnant 的 civil_from_days 算法），跨平台且不需要額外套件。
fn main() {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系統時間早於 UNIX epoch")
        .as_secs();

    let days = (secs / 86400) as i64;
    let time_of_day = secs % 86400;
    let (hour, minute, second) = (time_of_day / 3600, (time_of_day % 3600) / 60, time_of_day % 60);

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    println!(
        "cargo:rustc-env=CNG5_BUILD_TIMESTAMP={year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
    );
}

/// `system` plugin 的 `version` 指令要顯示這個 build 是什麼時候編譯的（見
/// `src/plugins/system.rs`），但 Rust 標準函式庫本身沒有「編譯當下的時間」這種
/// 東西——只能靠 build script 在編譯當下先算好，透過 `cargo:rustc-env` 塞進一個
/// 環境變數，執行時再用 `env!` 讀出來當常數用。沒有額外用 `chrono` 之類的套件算
/// 日期，直接借用系統本來就有的 `date` 指令格式化，省得自己刻曆法轉換。
fn main() {
    let output = std::process::Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .expect("執行 date 指令失敗");
    let timestamp = String::from_utf8(output.stdout).expect("date 指令輸出不是合法 UTF-8");
    println!("cargo:rustc-env=CNG5_BUILD_TIMESTAMP={}", timestamp.trim());
}

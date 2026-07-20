use std::collections::HashMap;
use std::fs;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::output::OutputBuffer;
use crate::plugin::{Plugin, SharedContext};
use crate::sysinfo;

/// Wake-on-LAN 慣例上送去的 UDP port，本身沒有特殊意義（WOL 封包內容才是
/// 重點，port 9 是傳統上的「discard」服務，不需要對方真的在監聽這個 port）。
const WOL_PORT: u16 = 9;

/// 已命名裝置清單存放位置，跟 `GitRepoPlugin`/`GITREPO_DIR` 一樣的作法：存在
/// 程式執行目錄底下，重啟後不用重新 `add` 一次。
const WOL_DIR: &str = "wol";
const DEVICES_FILE: &str = "devices.txt";

/// `manual` 指令的說明。
const MANUAL_TEXT: &str = "\
wol：喚醒區網內支援 Wake-on-LAN 的裝置（送 magic packet）。

範例：
  add linds 90:09:d0:64:4e:a4   把 mac 位址存成一個好記的名字
  wakeon linds                  用剛剛存的名字喚醒
  wakeon AA:BB:CC:DD:EE:FF      也可以直接送 mac 位址，不用先 add
  remove linds                  移除一個存過的名字
  status                        查每個存過的裝置最近有沒有在網路上活動

wakeon 對整個區網廣播 magic packet（255.255.255.255），不需要知道目標的 ip、
也不管它現在開著還是關著，支援 WOL 的網卡在關機狀態下就會持續監聽這個封包。

status 查的是本機目前的 ARP 表（Linux 讀 /proc/net/arp，Windows 跑 arp -a），
不會主動送任何封包，所以「查不到」不代表真的關機，也可能只是太久沒通訊、ARP
項目過期了。
";

/// 檢查大致像不像一個 mac 位址（6 組兩碼十六進位，用 `:` 或 `-` 分隔），只是
/// 擋掉明顯打錯的情況，不是嚴格的規格驗證。
fn looks_like_mac(mac: &str) -> bool {
    let sep = if mac.contains(':') { ':' } else { '-' };
    let groups: Vec<&str> = mac.split(sep).collect();
    groups.len() == 6 && groups.iter().all(|g| g.len() == 2 && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// 把 `add` 存的字串（`:` 或 `-` 分隔的六組十六進位）轉成 6 個 byte，`wakeon`
/// 組 magic packet 用。跟 `looks_like_mac` 分開是因為這裡要真的解出數值，不
/// 只是檢查外形。
fn parse_mac(mac: &str) -> Result<[u8; 6]> {
    let sep = if mac.contains(':') { ':' } else { '-' };
    let parts: Vec<&str> = mac.split(sep).collect();
    if parts.len() != 6 {
        bail!("不是合法的 mac 位址: {mac}（要是 6 組兩碼十六進位，用 : 或 - 分隔）");
    }
    let mut bytes = [0u8; 6];
    for (byte, part) in bytes.iter_mut().zip(&parts) {
        *byte = u8::from_str_radix(part, 16)
            .with_context(|| format!("不是合法的 mac 位址: {mac}（要是 6 組兩碼十六進位，用 : 或 - 分隔）"))?;
    }
    Ok(bytes)
}

/// magic packet 本體：6 個 `0xFF` 開頭，接著目標 mac 位址重複 16 次，這是
/// Wake-on-LAN 的標準格式，支援的網卡收到這個 pattern 就會開機。
fn magic_packet(mac: [u8; 6]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(6 + 16 * 6);
    packet.extend_from_slice(&[0xFF; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    packet
}

pub struct WolPlugin {
    #[allow(dead_code)]
    ctx: SharedContext,
    /// `add <name> <mac>` 存的對照表，`wakeon <name>` 查這裡把名字換成 mac；
    /// 查不到就把參數當成使用者直接給的 mac，不需要先 `add` 才能用。
    devices: HashMap<String, String>,
}

impl WolPlugin {
    pub fn new(ctx: SharedContext) -> Self {
        Self { ctx, devices: Self::load_devices() }
    }

    fn devices_path() -> PathBuf {
        Path::new(WOL_DIR).join(DEVICES_FILE)
    }

    fn load_devices() -> HashMap<String, String> {
        fs::read_to_string(Self::devices_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(name, mac)| (name.to_string(), mac.to_string()))
            .collect()
    }

    fn save_devices(&self) -> Result<()> {
        fs::create_dir_all(WOL_DIR).context("建立 wol 目錄失敗")?;
        let content: String = self.devices.iter().map(|(name, mac)| format!("{name} {mac}\n")).collect();
        fs::write(Self::devices_path(), content).context("儲存裝置清單失敗")?;
        Ok(())
    }

    fn add(&mut self, name: &str, mac: &str, out: &OutputBuffer) -> Result<()> {
        if !looks_like_mac(mac) {
            bail!("不像是 mac 位址: {mac}（要是 6 組兩碼十六進位，用 : 或 - 分隔）");
        }
        let existed = self.devices.insert(name.to_string(), mac.to_string()).is_some();
        self.save_devices()?;
        if existed {
            out.push(&format!("已更新: {name} -> {mac}\n"));
        } else {
            out.push(&format!("已新增: {name} -> {mac}\n"));
        }
        Ok(())
    }

    fn remove(&mut self, name: &str, out: &OutputBuffer) -> Result<()> {
        if self.devices.remove(name).is_none() {
            bail!("沒有這個名字: {name}");
        }
        self.save_devices()?;
        out.push(&format!("已移除: {name}\n"));
        Ok(())
    }

    /// `target` 先查有沒有對應的已命名裝置，查不到就當成使用者直接給的 mac，
    /// 組好 magic packet 之後對整個區網廣播（`255.255.255.255`）。廣播的原因
    /// 是目標機器這時候還沒開機，沒有自己的 ip 可以送單播——網卡本身在關機
    /// 狀態下就會持續監聽整個網段的廣播封包，看到帶有自己 mac 的 magic packet
    /// pattern 就開機，不需要知道對方 ip。
    fn wakeon(&mut self, target: &str, out: &OutputBuffer) -> Result<()> {
        let resolved = self.devices.get(target).cloned().unwrap_or_else(|| target.to_string());
        let mac = parse_mac(&resolved)?;
        let packet = magic_packet(mac);
        let socket = UdpSocket::bind("0.0.0.0:0").context("建立 UDP socket 失敗")?;
        socket.set_broadcast(true).context("設定 UDP broadcast 失敗")?;
        socket
            .send_to(&packet, ("255.255.255.255", WOL_PORT))
            .with_context(|| format!("送出 magic packet 失敗: {resolved}"))?;
        out.push(&format!("已送出 magic packet: {resolved}\n"));
        Ok(())
    }

    /// `list`/`status` 指令跟 `panel_text()` 共用的內容：每個 `add` 過的裝置，
    /// 現在讀一次系統的 ARP 表（見 `sysinfo::arp_table`）比對有沒有看到。只讀
    /// 本地既有的 ARP 表，不會主動送封包，很快，不需要像 tailscale/weather
    /// 那樣背景快取。
    fn status_text(&self) -> String {
        if self.devices.is_empty() {
            return "(還沒有用 add 存過任何裝置)".to_string();
        }
        let arp = sysinfo::arp_table();
        let mut lines: Vec<String> = self
            .devices
            .iter()
            .map(|(name, mac)| {
                let state = if arp.contains(&mac.to_lowercase()) {
                    "最近有在網路上活動"
                } else {
                    "查不到（可能沒開機，也可能只是太久沒通訊，ARP 項目過期了）"
                };
                format!("{name} ({mac}): {state}")
            })
            .collect();
        lines.sort();
        lines.join("\n")
    }

    fn status(&mut self, out: &OutputBuffer) -> Result<()> {
        out.push(&format!("{}\n", self.status_text()));
        Ok(())
    }
}

impl Plugin for WolPlugin {
    fn commands(&self) -> &'static [&'static str] {
        &["add <name> <mac>", "remove <name>", "wakeon <name/mac>", "status"]
    }

    fn dispatch(&mut self, cmd: &str, args: &[String], out: &OutputBuffer) -> Result<()> {
        match cmd {
            "add" => self.add(
                args.first().context("add 需要接名字跟 mac 位址")?,
                args.get(1).context("add 需要接名字跟 mac 位址")?,
                out,
            ),
            "remove" => self.remove(args.first().context("remove 需要接名字")?, out),
            "wakeon" => self.wakeon(args.first().context("wakeon 需要一個名字/mac 參數")?, out),
            "status" => self.status(out),
            other => bail!("wol 不認得指令: {other}"),
        }
    }

    fn panel_text(&self) -> Option<String> {
        Some(self.status_text())
    }

    fn manual_text(&self) -> &'static str {
        MANUAL_TEXT
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

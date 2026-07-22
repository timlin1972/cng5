use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// 跨 domain remote 用的共用對稱金鑰檔案路徑（見 `.gitignore` 的 `/remote-key`）。
/// 純文字、64 個 hex 字元（= 32 bytes），所有裝置要放同一份、手動複製過去
/// （不做自動產生/分發指令——這是使用者明確要的最簡單做法，跟 `utils/align.sh`
/// 手動同步機器的風格一致）。查不到/格式不對就直接回錯，不像 tailscale 那種
/// 「查不到就當作沒有」的情境——沒有這把 key，跨 domain remote 完全用不了。
const KEY_FILE: &str = "remote-key";

/// 加密訊息裡嵌入的時間戳，跟目前時間差超過這個秒數就當作不可信（重放攻擊，
/// 或裝置時間沒對好）拒絕。±30 秒是抓一般跨網路 NTP 同步裝置的時鐘誤差＋MQTT
/// 傳輸延遲的餘裕。
const REPLAY_WINDOW_SECS: u64 = 30;

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 已經處理過一次的 nonce（防重放）：直接原封不動重播同一則加密訊息時，
/// nonce/密文會跟原始那一次一模一樣，靠這個表擋掉。刻意用 nonce 而不是直接
/// 比對時間戳本身——兩個「不同」但剛好落在同一秒的合法訊息，時間戳會相等，
/// 但 nonce 一定不同（`Nonce::generate()` 每次都是新的隨機值），只比較時間戳
/// 會誤殺這種正常情況，nonce 才是真正能區分「這是同一則訊息」的依據。只保留
/// 還在容許窗口內可能被重放的 nonce，太舊的訊息反正已經會被時間戳檢查擋掉，
/// 不用留著佔記憶體，每次檢查順便清掉過期的。
static SEEN_NONCES: LazyLock<Mutex<HashMap<[u8; 12], Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 記錄這個 nonce 已經處理過一次；如果之前就記錄過（代表這則訊息被重放了）
/// 就回錯誤。只在解密成功之後才呼叫（見 `open_envelope`），不記錄解密失敗
/// （金鑰不對/被竄改）的雜訊。
fn check_and_record_nonce(nonce: &[u8]) -> Result<()> {
    let key: [u8; 12] = nonce.try_into().ok().context("nonce 長度不對")?;
    let mut seen = SEEN_NONCES.lock().unwrap();
    seen.retain(|_, seen_at| seen_at.elapsed() <= Duration::from_secs(REPLAY_WINDOW_SECS * 3));
    if seen.contains_key(&key) {
        bail!("這則訊息之前已經處理過一次，當作重放攻擊拒絕");
    }
    seen.insert(key, Instant::now());
    Ok(())
}

/// 手寫的簡單 hex decode，不為了讀一個 key 檔案這種小事引入額外的 `hex` crate。
fn decode_hex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("hex 字串長度必須是偶數");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).with_context(|| format!("非法的 hex 字元: {}", &s[i..i + 2])))
        .collect()
}

/// 把 `KEY_FILE` 的內容（64 個 hex 字元）解成 32 bytes 的金鑰。
fn load_key() -> Result<Key> {
    let text = std::fs::read_to_string(KEY_FILE)
        .with_context(|| format!("讀不到金鑰檔案 {KEY_FILE}（跨 domain remote 需要先手動放一份，所有裝置要一樣）"))?;
    let bytes = decode_hex(text.trim()).with_context(|| format!("{KEY_FILE} 內容不是合法的 hex"))?;
    Key::try_from(bytes.as_slice())
        .ok()
        .with_context(|| format!("{KEY_FILE} 長度不對，要 64 個 hex 字元（= 32 bytes）"))
}

/// 加密訊息的外層信封：`ts` 是防重放用的時間戳，`body` 是實際要傳的內容
/// （`Exec`/`ExecReply`/`Panel`/`PanelReply`，見 `plugin.rs`）。這一層拆出來，
/// 是因為時間戳檢查只需要寫一次，套用到所有訊息種類上，呼叫端（`seal`/
/// `open`）不用各自處理。
#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    ts: u64,
    body: T,
}

/// 用 `key` 把 `envelope` 序列化＋加密：wire bytes = `nonce(12 bytes) ||
/// ciphertext`。每則訊息都用新產生的隨機 nonce（`getrandom` feature，
/// `Nonce::generate()`），不用自己維護計數器讓多個行程/執行緒同步。
fn seal_envelope<T: Serialize>(key: &Key, envelope: &Envelope<T>) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(key);
    let plaintext = serde_json::to_vec(envelope).context("序列化失敗")?;
    let nonce = Nonce::generate();
    let ciphertext = cipher.encrypt(&nonce, plaintext.as_ref()).map_err(|_| anyhow::anyhow!("加密失敗"))?;
    let mut wire = nonce.to_vec();
    wire.extend_from_slice(&ciphertext);
    Ok(wire)
}

/// 解密 `wire` 拿回 `Envelope`，順便做 nonce 防重放檢查（`check_and_record_nonce`），
/// 但不檢查裡面的時間戳是否過期——`open_with`/`open` 才會做那一步，這裡拆
/// 出來單純是給 `stale_timestamp_rejected` 這類測試組裝過期時間戳的訊息用。
fn open_envelope<T: DeserializeOwned>(key: &Key, wire: &[u8]) -> Result<Envelope<T>> {
    if wire.len() < 12 {
        bail!("訊息長度不足，不是合法的加密封包");
    }
    let cipher = ChaCha20Poly1305::new(key);
    let (nonce_bytes, ciphertext) = wire.split_at(12);
    let nonce = <&Nonce>::try_from(nonce_bytes).ok().context("nonce 長度不對")?;
    let plaintext =
        cipher.decrypt(nonce, ciphertext).map_err(|_| anyhow::anyhow!("解密失敗（金鑰不對或密文被竄改）"))?;
    check_and_record_nonce(nonce_bytes)?;
    serde_json::from_slice(&plaintext).context("解密後的內容不是合法的訊息格式")
}

fn seal_with<T: Serialize>(key: &Key, body: &T) -> Result<Vec<u8>> {
    seal_envelope(key, &Envelope { ts: now_secs(), body })
}

fn open_with<T: DeserializeOwned>(key: &Key, wire: &[u8]) -> Result<T> {
    let envelope: Envelope<T> = open_envelope(key, wire)?;
    let diff = now_secs().abs_diff(envelope.ts);
    if diff > REPLAY_WINDOW_SECS {
        bail!("時間戳超出容許範圍（{diff} 秒），可能是重放攻擊或裝置時間沒對好");
    }
    Ok(envelope.body)
}

/// 把 `body` 包上時間戳、加密，讀 `KEY_FILE` 當金鑰。給 `global.rs`/`shell.rs`/
/// `web.rs` 真正要發布/送出跨 domain 訊息時用。
pub fn seal<T: Serialize>(body: &T) -> Result<Vec<u8>> {
    seal_with(&load_key()?, body)
}

/// 解密＋檢查時間戳（見 `REPLAY_WINDOW_SECS`），讀 `KEY_FILE` 當金鑰。竄改過的
/// 密文、或時間戳超出窗口都會回傳錯誤，呼叫端應該把這則訊息當不可信直接丟棄。
pub fn open<T: DeserializeOwned>(wire: &[u8]) -> Result<T> {
    open_with(&load_key()?, wire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Ping {
        n: u32,
    }

    fn test_key() -> Key {
        Key::try_from([7u8; 32].as_slice()).unwrap()
    }

    #[test]
    fn round_trip() {
        let key = test_key();
        let wire = seal_with(&key, &Ping { n: 42 }).unwrap();
        let got: Ping = open_with(&key, &wire).unwrap();
        assert_eq!(got, Ping { n: 42 });
    }

    #[test]
    fn wrong_key_fails() {
        let wire = seal_with(&test_key(), &Ping { n: 1 }).unwrap();
        let other_key = Key::try_from([9u8; 32].as_slice()).unwrap();
        assert!(open_with::<Ping>(&other_key, &wire).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = test_key();
        let mut wire = seal_with(&key, &Ping { n: 1 }).unwrap();
        let last = wire.len() - 1;
        wire[last] ^= 0xff;
        assert!(open_with::<Ping>(&key, &wire).is_err());
    }

    #[test]
    fn replayed_message_rejected() {
        let key = test_key();
        let wire = seal_with(&key, &Ping { n: 1 }).unwrap();
        assert!(open_with::<Ping>(&key, &wire).is_ok(), "第一次打開應該成功");
        let second: Result<Ping> = open_with(&key, &wire);
        assert!(second.is_err(), "原封不動重放同一包加密訊息應該被拒絕");
    }

    #[test]
    fn same_second_different_messages_both_accepted() {
        // 兩個「不同」的合法訊息剛好落在同一秒（時間戳會相等），不該被誤判成
        // 重放——nonce 每次都是新的隨機值，才是真正判斷「這是同一則訊息」的
        // 依據，不是時間戳本身。
        let key = test_key();
        let ts = now_secs();
        let first = seal_envelope(&key, &Envelope { ts, body: Ping { n: 1 } }).unwrap();
        let second = seal_envelope(&key, &Envelope { ts, body: Ping { n: 2 } }).unwrap();
        assert_eq!(open_with::<Ping>(&key, &first).unwrap(), Ping { n: 1 });
        assert_eq!(open_with::<Ping>(&key, &second).unwrap(), Ping { n: 2 });
    }

    #[test]
    fn stale_timestamp_rejected() {
        let key = test_key();
        let envelope = Envelope { ts: now_secs() - REPLAY_WINDOW_SECS - 5, body: Ping { n: 1 } };
        let wire = seal_envelope(&key, &envelope).unwrap();
        assert!(open_with::<Ping>(&key, &wire).is_err());
    }

    #[test]
    fn future_timestamp_within_window_ok() {
        let key = test_key();
        // 容許一點點超前（時鐘沒完全同步時，對方可能比我早幾秒），只要落在
        // 窗口內就該接受，不是只檢查「太舊」這個方向。
        let envelope = Envelope { ts: now_secs() + REPLAY_WINDOW_SECS - 5, body: Ping { n: 1 } };
        let wire = seal_envelope(&key, &envelope).unwrap();
        let got: Ping = open_with(&key, &wire).unwrap();
        assert_eq!(got, Ping { n: 1 });
    }

    #[test]
    fn decode_hex_roundtrips() {
        assert_eq!(decode_hex("00ff7f").unwrap(), vec![0x00, 0xff, 0x7f]);
        assert!(decode_hex("0").is_err()); // 長度不是偶數
        assert!(decode_hex("zz").is_err()); // 不是合法 hex
    }
}

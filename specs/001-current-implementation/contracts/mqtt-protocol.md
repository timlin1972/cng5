# Contract: 跨網域橋接協定（公開 MQTT broker `broker.emqx.io`）

只有 domain 的**伺服器**角色會連線 MQTT；客戶端一律只透過自己伺服器的
`/api/global/list` 與 `/api/remote/cross-relay` 間接參與（見 web-api.md）。

## Topic 命名

- `<bridge-id>/<domain>/device` — 各 domain 伺服器發布/訂閱自己與其他 domain 的
  裝置清單（`global` 外掛的 `bridge <id>` 設定 bridge-id、`domain <name>` 設定
  自己的 domain 名稱）。
- `<bridge-id>/<target-domain>/remote/request` — 發起跨網域請求時發布的 topic，
  由目標 domain 的伺服器訂閱。
- `<bridge-id>/<source-domain>/remote/reply` — 目標處理完成後回覆的 topic，由
  發起請求那一端的伺服器訂閱、依 `request_id` 配對回原本等待中的呼叫。

同一個 `bridge-id` 底下的所有 domain 互相可見；不同 `bridge-id` 之間完全隔離
（純字串比對，沒有存取控制機制——`bridge-id` 本身即是「誰能看見誰」的邊界，
搭配 `remote-key` 加密確保即使 bridge-id 被他人猜中，也無法解密內容或偽造有效
訊息）。

## 訊息封裝

所有發布到上述 topic 的訊息，wire bytes 皆為：

```
nonce(12 bytes) || ChaCha20Poly1305(key, nonce, JSON(Envelope{ ts, body }))
```

- `key`：`remote-key` 檔案內容解出的 32 bytes 對稱金鑰，所有參與機器必須一致。
- `ts`：Unix 秒數時間戳，接收端會拒絕與目前時間差超過 `±30` 秒的訊息。
- `body`：依 topic 而定，`.../device` 帶裝置清單相關內容、`.../remote/request`
  帶 `RemoteRequest`（見 data-model.md）、`.../remote/reply` 帶 `RemoteReply`。

接收端額外維護一張「已處理過的 nonce」記錄表（保留時間窗 3 倍長），同一則訊息
的原封重放會被拒絕，即使仍在時間窗內。

## 請求/回應配對

發起端（`shell.rs`/`web.rs`）產生一個行程內唯一的 `request_id`，登記一個一次性
回覆通道到 `ContextInner.cross_domain_pending`，再發布 `RemoteRequest`；目標
domain 的伺服器處理完成後，把結果包成帶相同 `request_id` 的 `RemoteReply` 發布
回 `.../remote/reply`；發起端的 MQTT session 收到後查表送進對應通道。查無對應
`request_id`（例如發起端已逾時放棄）時直接丟棄回覆。

## 檔案傳輸的分段規則

- `FileList`：`offset` 為清單索引（非位元組），每頁最多 `FILE_LIST_PAGE_BUDGET =
  4096` bytes（依檔名長度估算），回覆帶 `total` 供呼叫端判斷是否還有下一頁。
- `FilePull`/`FilePush`：`offset` 為位元組位移，單次資料量上限
  `FILE_CHUNK_SIZE = 4096` bytes；是否為最後一塊由發送端依本機檔案總長度自行
  判斷，不需要額外欄位告知接收端。
- 兩項限制皆源自實測 broker 對單則訊息原始 payload 約 11000 bytes 的靜默丟棄
  門檻（見 research.md §4），刻意留有安全餘裕。

## 授權邊界

`RemoteRequest` 內容通過 AEAD 解密僅代表「發送端持有正確的 `remote-key`」，
**不代表**內容本身合法——例如 `FileList`/`FilePull`/`FilePush` 的 `folder`
欄位在接收端仍必須驗證在允許清單（`music`/`notepad`）內才會處理，其餘一律拒絕。

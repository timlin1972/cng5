#! /bin/bash

# 純粹是 crash 韌性網：cng5 不管什麼原因結束都重新跑一次已經在硬碟上、已知
# 能動的那份 code。程式碼更新完全交給 cng5 自己的 upgrade 指令（見
# src/shell.rs 的 run_upgrade）——它會先 git fetch/reset + cargo build，
# 確認編譯成功才切換，失敗就維持原狀繼續跑舊版本。這裡如果也自己做
# fetch/reset，會讓「crash 重啟」意外變成「順便升級」：万一 origin/main
# 那時候剛好是壞的，這裡的 cargo run 沒有 upgrade 那種「先確認編譯成功才
# 切換」的保護，會直接建置失敗、服務起不來，而且原本能跑的 code 已經被
# reset --hard 蓋掉了。

cng5="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

while true
do
        cd "$cng5"
        cd ..

        CARGO_BIN=$(which cargo)

        if [ -z "$CARGO_BIN" ]; then
                echo "cargo not found in PATH"
                exit 1
        fi

        # 這個 loop 本身就是「行程結束就重啟」的機制，跟 upgrade 指令自己的
        # 重啟（--respawn-after）是兩套獨立的東西——都留著的話，upgrade 編譯
        # 成功、觸發結束之後，這個 loop 跟 upgrade 自己 spawn 的重啟流程會
        # 同時搶著啟動下一個 process、搶 bind port 9759。設這個環境變數告訴
        # upgrade「重啟交給我（這個 loop）就好，你不用自己動手」。
        CNG5_EXTERNAL_RESTART=1 "$CARGO_BIN" run
done

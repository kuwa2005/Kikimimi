# 録音の取りこぼし耐性の試験（Playwright）

会議の録音中に起こりうる妨害を、本物の Chromium（偽マイク・偽の音声認識）で起こし、
録音と書き起こしがサーバに残るかを確かめます。

| シナリオ | 起こすこと | 確かめること |
|---|---|---|
| `crash` | 送信できない状態で録音を続け、ブラウザを SIGKILL で落とす → 同じプロファイルで開き直す | 落ちる直前までの録音（途中保存の間隔 2 秒以内）と速報字幕がすべて届く／再開した続きが隙間・重なりなく並ぶ／端末の保管庫が空になる |
| `offline` | 録音中に 20 秒オフライン | 録音が止まらない／保存済みと表示／復帰後すぐ送り切る／隙間が無い／速報字幕がすべて残る |
| `background` | 別のページを前面・タブを非表示・AudioContext の停止・ページ凍結・マイクの切断 | Wake Lock の取得と取り直し／非表示中も録音が進む／音声処理の自動復帰／マイクの取り直し／隙間が無い |
| `errors` | 音声の送信に 500 を返し続ける → 送れない間に別端末で会議を終了 | 再送で全部届く／終了後に残った分も取り込みとして保存される |
| `apifail` | ダミーの API キーで API 文字起こしを失敗させる | 失敗区間が速報字幕の控えで埋まり、議事録に欠落と補完が明記される |

## 実行

サーバを開発用のメール出力つきで起動し、試験用アカウントを作ってから実行します。

```bash
(cd web && npm ci && npm run build) && cargo build --release
```

```bash
KIKIMIMI_DATA_DIR=/tmp/kikimimi-e2e/data KIKIMIMI_MAIL_DIR=/tmp/kikimimi-e2e/mail KIKIMIMI_WEB_DIR=$PWD/web/dist ./target/release/kikimimi
```

別の端末で、画面の「新しく登録」から試験用アカウントを作ります（確認コードは `KIKIMIMI_MAIL_DIR` に書き出されます）。そのあと:

```bash
E2E_EMAIL=<試験用アカウント> E2E_PASSWORD=<そのパスワード> KIKIMIMI_DATA_DIR=/tmp/kikimimi-e2e/data node e2e/resilience.mjs
```

- 試験はサーバの DB を読み取り専用で開いて、録音区間の並びを確かめます（`python3` を使います）。
- `apifail` は、試験用アカウントの Gemini キーをダミー値に書き換えます。本番のアカウントでは実行しないでください。
- 作業ファイル（ブラウザのプロファイル・偽マイクの音声）は `e2e/.work/` に置きます。

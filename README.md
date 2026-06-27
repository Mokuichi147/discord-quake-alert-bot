# 地震bot (Rust)

[P2P地震情報 JSON API v2](https://www.p2pquake.net/develop/json_api_v2/) の WebSocket を購読し、日本国内（特に関東）で強い揺れの地震が発生したときに、震源地・マグニチュード・最大震度を **Discord Webhook** へ通知します。座標が判明している場合は震源地をプロットした地図画像も添付します。

## 通知条件

デフォルトは「関東で震度4以上、それ以外の地域は震度5強以上」。環境変数で柔軟に変更できます。

- 関東のいずれかの地点で `KANTO_MIN_SCALE`（既定 40 = 震度4）以上 → 通知
- もしくは全国の最大震度が `OTHER_MIN_SCALE`（既定 50 = 震度5強）以上 → 通知

関東＝茨城・栃木・群馬・埼玉・千葉・東京・神奈川。

震度スケール値: `10`=1, `20`=2, `30`=3, `40`=4, `45`=5弱, `50`=5強, `55`=6弱, `60`=6強, `70`=7。

## セットアップ

1. Rust（stable）をインストール: https://rustup.rs/
2. Discord でチャンネルの Webhook URL を発行（サーバー設定 → 連携サービス → ウェブフック）。
3. 設定ファイルを用意:

   ```sh
   cp .env.example .env
   # .env を編集して DISCORD_WEBHOOK_URL を設定
   ```

## ビルドと実行

```sh
cargo run --release
```

テスト（通知判定ロジック）:

```sh
cargo test
```

## 設定（環境変数）

| 変数 | 既定値 | 説明 |
| --- | --- | --- |
| `DISCORD_WEBHOOK_URL` | （必須） | Discord Webhook URL |
| `KANTO_MIN_SCALE` | `40` | 関東で通知する最小震度スケール |
| `OTHER_MIN_SCALE` | `50` | 関東以外で通知する最小震度スケール |
| `ATTACH_MAP` | `true` | 地図画像を添付するか |
| `TILE_URL_TEMPLATE` | OpenStreetMap | 地図タイルの URL テンプレート |
| `P2PQUAKE_WS_URL` | `wss://api.p2pquake.net/v2/ws` | WebSocket エンドポイント |
| `RUST_LOG` | `info` | ログレベル |

`.env` が無い場合は OS の環境変数を参照します。

## 構成

```
src/
  main.rs       … エントリポイント。WebSocket購読・再接続・通知の制御
  config.rs     … 環境変数の読み込み
  model.rs      … P2P地震情報 API のレスポンス型(serde)
  intensity.rs  … 震度変換・通知条件の判定（単体テストあり）
  mapgen.rs     … 震源地をプロットした地図PNGの生成(staticmap)
  discord.rs    … Discord embed の組み立てと Webhook 送信(multipart)
```

## 動作の概要

1. `wss://api.p2pquake.net/v2/ws` に接続し、流れてくる JSON を1件ずつ処理。
2. `code == 551`（地震情報）のみを対象に解析。
3. 通知条件を満たす場合のみ、地図画像（座標があれば）を生成。
4. Discord Webhook へ embed + 画像を送信。
5. 切断時は指数バックオフ（最大60秒）で自動再接続。

## 注意事項

- **地図タイルの利用規約**: 既定の OpenStreetMap 公式タイルは個人利用・低頻度を想定したものです。頻繁に利用する場合は [OSM のタイル利用ポリシー](https://operations.osmfoundation.org/policies/tiles/) を確認のうえ、商用タイルプロバイダや自前のタイルサーバへ差し替えてください（`TILE_URL_TEMPLATE`）。
- P2P地震情報は気象庁発表をもとにした第三者サービスです。緊急地震速報（予報・警報）そのものではなく、揺れの「予想/観測」情報を扱います。重大用途には公式情報源も併用してください。
- 震源座標が不明な情報（`latitude`/`longitude` が無効値）では地図を添付せずテキストのみで通知します。

## 常時稼働

`systemd` の例（`/etc/systemd/system/quake-alert-bot.service`）:

```ini
[Unit]
Description=Quake Alert Bot
After=network-online.target

[Service]
WorkingDirectory=/opt/quake-alert-bot
EnvironmentFile=/opt/quake-alert-bot/.env
ExecStart=/opt/quake-alert-bot/target/release/quake-alert-bot
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

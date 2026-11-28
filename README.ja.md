<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-wordmark-dark.png">
    <img src="assets/logo-wordmark.png" alt="Dogmind" width="240">
  </picture>
</p>

<p align="center"><a href="README.md">English</a></p>

Dogmind は、Windows PC で動く AI エージェントのアプリです。選んだフォルダーのファイルを読み書きし、コマンドを実行します。変更やコマンドの前には確認を求めます。

- **この PC のモデル**: エンジンの [Straydog](https://github.com/rewroy/Straydog) が、この PC の GPU でモデルを動かします。
- **ほかのモデルも**: 別のマシンのモデル、OpenAI 互換のサービス、Codex、Claude も使えます。
- **軽い**: Win32 のアプリで、画面の描画に GPU のメモリを使いません。VRAM はモデルのために残ります。

## インストール

Windows 11 で、管理者権限なしで動きます。[Releases](https://github.com/rewroy/dogmind-app/releases) の `DogmindSetup-VERSION.exe` を実行してください。

この PC でモデルを動かすには、新しいドライバーの入った NVIDIA の GPU と、ドライブに約 76 GB の空きが必要です。GPU がなくても、ほかの接続先は使えます。

署名がまだないため、警告が出たら「詳細情報 > 実行」を押してください(スマート アプリ コントロールが有効だと実行できません)。

## 使い始める

最初の起動で、モデルへの接続方法と作業するフォルダーを聞かれます(`/setup` でやり直せます)。

- Enter で送信、Shift+Enter で改行、Esc で中止
- `/` でコマンド、`@` でワークスペースのファイル
- Ctrl+N で新しいセッション、Ctrl+, で設定

## 接続先

「設定 > モデルと接続先」で選びます。

| 種類 | 内容 | キー |
| --- | --- | --- |
| この PC の Straydog | 選んだときにアプリが取得する Straydog の最新リリース(約 0.6 GB)と、モデル | 不要 |
| OpenAI 互換 | 別のマシンの Straydog や、同じ形の API を持つサービス | 貼り付け、または `env:NAME` |
| Codex | 公式の Codex CLI を通した ChatGPT のプラン(先に `codex login`) | CLI のもの |
| Claude | Messages API | 貼り付け、または `env:ANTHROPIC_API_KEY` |

キーは Windows の資格情報マネージャーに保存し、設定ファイルには書きません。

## 安全のために

- コマンドはサンドボックス(AppContainer)の中で動きます。書き込めるのは作業フォルダーだけで、ネットワークは確認したときだけです。
- 編集とコマンドは、実行の前に確認を求めます。
- 会話は「設定 > プライバシー」で暗号化できます(既定はオフ)。

## プライバシー

会話やファイルを開発者に送ることはなく、テレメトリもありません。外と通信するのは次のときだけです([詳しく](docs/guide.ja.md#プライバシー))。

- 選んだ接続先(この PC の Straydog なら外に出ません)
- Web の検索と閲覧(既定でオン)
- 更新の確認(既定でオン、送るのは版だけ)
- モデル、ページの確認のライブラリ、同梱のガイドの取得
- 追加した MCP サーバーと、許可したコマンド

## 更新と削除

新しい版が出ると、アプリが知らせて、ボタン 1 つで更新します。削除は「設定 > 全般 > この PC からアプリを削除」です。

## 困ったとき

`/report` でレポートを作り、[Issue](https://github.com/rewroy/dogmind-app/issues) に添付してください。セキュリティの問題は [SECURITY.md](SECURITY.md) へ。

## もっと詳しく

細かい説明は[ガイド](docs/guide.ja.md)にあります。

## ライセンス

MIT(`LICENSE`)。

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

アプリにはまだ署名がありません。「Windows によって PC が保護されました」と表示されたら、「詳細情報 > 実行」を押してください。スマート アプリ コントロールが有効な場合、Windows はこのアプリを実行しません。

## 使い始める

最初の起動で、モデルへの接続方法と作業するフォルダーを聞かれます(`/setup` でやり直せます)。

- Enter で送信、Shift+Enter で改行、Esc で中止
- `/` でコマンド、`@` でワークスペースのファイル
- Ctrl+N で新しいセッション、Ctrl+, で設定

## 接続先

「設定 > モデルと接続先」で選びます。

| 種類 | 内容 | キー |
| --- | --- | --- |
| この PC の Straydog | アプリの横の `straydog.exe` | 不要 |
| OpenAI 互換 | 別のマシンの Straydog や、同じ形の API を持つサービス | 貼り付け、または `env:NAME` |
| Codex | 公式の Codex CLI を通した ChatGPT のプラン(先に `codex login`) | CLI のもの |
| Claude | Messages API | 貼り付け、または `env:ANTHROPIC_API_KEY` |

キーは Windows の資格情報マネージャーに保存し、設定ファイルには書きません。

## 安全のために

- コマンドはサンドボックス(AppContainer)の中で動きます。書き込めるのは作業フォルダーだけで、ネットワークは確認したときだけ使えます。
- 編集とコマンドは、モードに応じて実行の前に確認を求めます。Web の内容が会話に入った後は、そこから頼まれたかもしれないことを毎回確認します。
- 会話は「設定 > プライバシー」で暗号化できます(既定はオフ)。

## 更新と削除

新しい版が出ると、アプリが知らせて、ボタン 1 つで更新します。削除は「設定 > 全般 > この PC からアプリを削除」です。

## 困ったとき

`/report` で問題のレポートを作り、何をしていたかを添えて [Issue](https://github.com/rewroy/dogmind-app/issues) に添付してください。自動で送信されることはありません。セキュリティの問題は、[SECURITY.md](SECURITY.md) の方法で非公開で報告してください。

## もっと詳しく

操作の細部、会話の暗号化、エンジンの状態を残す設定、更新の仕組み、ビルド、エージェントのツールとサンドボックスは、[ガイド](docs/guide.ja.md)にあります。

## ライセンス

MIT ライセンスです(`LICENSE`)。同梱物のライセンスは、配布物の `LICENSE.txt` にまとめています。

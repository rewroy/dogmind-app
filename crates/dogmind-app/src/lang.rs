//! The words of the interface. English is written in the code and is the key; what another language shows is in a table here,
//! and a word it does not have shows in English. `{0}` and so on stand for values, in whatever order the language wants them.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use dogmind_agent::{images, llm};

static JAPANESE: AtomicBool = AtomicBool::new(false);

/// `system`, `en` or `ja`.
pub fn set(choice: &str) {
    JAPANESE.store(resolves_to_japanese(choice, system_is_japanese()), Ordering::Relaxed);
}

pub fn is_japanese() -> bool {
    JAPANESE.load(Ordering::Relaxed)
}

fn resolves_to_japanese(choice: &str, system: bool) -> bool {
    match choice {
        "ja" => true,
        "en" => false,
        _ => system,
    }
}

pub fn system_is_japanese() -> bool {
    unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() & 0x3FF == 0x11 }
}

fn japanese() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| JA.iter().copied().collect())
}

#[cfg(test)]
pub fn has_japanese(english: &str) -> bool {
    japanese().contains_key(english)
}

/// These words in the language of the interface.
pub fn t(english: &str) -> &str {
    if is_japanese() {
        japanese().get(english).copied().unwrap_or(english)
    } else {
        english
    }
}

/// The same, with values for `{0}`, `{1}`...
pub fn tf(english: &str, values: &[&dyn Display]) -> String {
    fill(t(english), values)
}

fn fill(words: &str, values: &[&dyn Display]) -> String {
    let mut text = words.to_string();
    for (at, value) in values.iter().enumerate() {
        text = text.replace(&format!("{{{at}}}"), &value.to_string());
    }
    text
}

/// What the agent says of itself comes as English, some of it with a detail after the words that are always the same.
pub fn notice(text: &str) -> String {
    if !is_japanese() {
        return without_system_code(text);
    }
    without_system_code(&in_japanese(text))
}

/// The same for why a turn failed: `embedded`, it was the engine this app starts, and writing brings it back.
pub fn failure(text: &str, embedded: bool) -> String {
    notice(if embedded { llm::for_the_engine(text) } else { text })
}

fn in_japanese(text: &str) -> String {
    if let Some(words) = japanese().get(text) {
        return (*words).to_string();
    }
    for (english, words) in NOTICES {
        if let Some(rest) = text.strip_prefix(english) {
            return format!("{words}{rest}");
        }
    }
    for (english, words) in COUNTED {
        let (before, after) = english.split_once("{0}").unwrap_or((english, ""));
        if let Some(count) = text.strip_prefix(before).and_then(|rest| rest.strip_suffix(after)) {
            return words.replace("{0}", count);
        }
    }
    text.to_string()
}

/// Windows' words for a failure are in the person's language; the number after them, ` (os error 10061)`, helps no one.
fn without_system_code(text: &str) -> String {
    let mut out = text.to_string();
    while let Some(at) = out.find(" (os error ") {
        let Some(end) = out[at..].find(')') else { break };
        out.replace_range(at..at + end + 1, "");
    }
    out
}

const NOTICES: &[(&str, &str)] = &[
    ("reads a place secrets are kept: ", "秘密が置かれた場所を読みます: "),
    (llm::SERVER_ERROR, "サーバーでエラーが起きました。会話はそのまま残っています。もう一度送ってください。サーバーからの内容: "),
    (llm::REFUSED, "サーバーがこの依頼を受け付けませんでした。会話はそのまま残っています。サーバーからの内容: "),
    (llm::NO_ANSWER, "サーバーから応答がありません。起動しているか、アドレスが正しいかを確認してください: "),
    ("the session could not be saved: ", "セッションを保存できませんでした: "),
    ("this turn cannot be rewound: ", "このターンは巻き戻せません: "),
    ("a rewind will not bring this edit back: ", "この編集は巻き戻しでは戻せません: "),
    ("the rule could not be saved, so it lasts this session: ", "ルールを保存できなかったため、このセッションの間だけ有効です: "),
    ("Claude declined this request (", "Claude がこの依頼を断りました ("),
    ("Claude: ", "Claude からのエラー: "),
    ("the Codex app-server asked for something this transport does not allow: ", "Codex が、この接続では許可されない操作を求めました: "),
    ("the Codex app-server tried to use a tool of its own: ", "Codex が独自のツールを使おうとしました: "),
    ("codex: ", "Codex を実行できませんでした: "),
    ("codex ", "Codex のエラー: "),
];

/// What holds a number at its front or in its middle.
const COUNTED: &[(&str, &str)] = &[
    ("Session compacted: old tool outputs shortened by {0} KB", "セッションを圧縮しました: 古いツール出力を {0} KB 短くしました"),
    ("Session compacted: every tool output, the newest too, shortened by {0} KB, as nothing else would fit", "セッションを圧縮しました: ほかに減らせるものがないため、最新のものも含めてツール出力を {0} KB 短くしました"),
    ("{0} tool calls failed in this turn: stopped so that you can look", "このターンでツール呼び出しが {0} 回失敗したため、確認できるように止めました。"),
    ("the model made the same call {0} times with nothing edited in between: stopped so that you can look", "モデルが何も編集せずに同じ呼び出しを {0} 回繰り返したため、確認できるように止めました。"),
    ("stopped after {0} iterations", "{0} 回繰り返したところで止めました。もう一度送ると続けられます。"),
];

const JA: &[(&str, &str)] = &[
    ("Let's go", "行こう"),
    // Commands.
    ("Go through the first setup again", "最初のセットアップをやり直す"),
    ("How to use this app", "このアプリの使い方"),
    ("Version, model, folder and connection", "バージョン、モデル、フォルダー、接続状態"),
    ("A new session in this folder", "このフォルダーで新しいセッションを始める"),
    ("The same as /new", "/new と同じ"),
    ("Open another session of this folder", "このフォルダーの別のセッションを開く"),
    ("Find in this conversation: /find words", "この会話の中を検索: /find 語句"),
    ("Search all sessions: /search words", "全セッションを検索: /search 語句"),
    ("Name this session", "このセッションに名前を付ける"),
    ("Sum up the conversation to make room", "会話を要約して空きを作る"),
    ("Ask the last request again", "最後の依頼をもう一度送る"),
    ("Go back to an earlier turn: its files, the conversation, or both", "前のターンに戻る: ファイル、会話、またはその両方"),
    ("Copy the last answer", "最後の回答をコピー"),
    ("What changed in the folder, file by file", "フォルダーの変更をファイルごとに見る"),
    ("Tokens and speed of this session", "このセッションのトークン数と速度"),
    ("Have the model write this project's DOGMIND.md", "このプロジェクトの DOGMIND.md をモデルに書かせる"),
    ("Change how the model runs: /model Claude", "接続先を変更: /model Claude"),
    ("How hard to think: none low medium high xhigh default", "思考の強さ: none low medium high xhigh default"),
    ("When to ask: ask edits auto plan", "確認のタイミング: ask edits auto plan"),
    ("Look and plan first, and change nothing until you say", "まず調べて計画し、指示があるまで何も変更しない"),
    ("The skills of this folder, from Claude's and Codex's folders too", "このフォルダーのスキル (Claude と Codex のフォルダーも含む)"),
    ("What is kept between sessions", "セッションをまたいで保持される内容"),
    ("MCP servers: /mcp shows them, /mcp import takes Claude's and Codex's", "MCP サーバー: /mcp で一覧、/mcp import で Claude と Codex の設定を取り込み"),
    ("What may always run in this folder", "このフォルダーで常に実行してよいもの"),
    ("Show or hide the thinking", "思考の表示を切り替える"),
    ("Open another program's folder on PATH to sandboxed commands, with one Windows prompt", "PATH にある別のプログラムのフォルダーをサンドボックス内のコマンドに開く (Windows の確認が 1 回出ます)"),
    ("Look into", "調査"),
    ("Check a page", "ページを確認"),
    ("How the agent should talk and work, for every new session", "エージェントの話し方と進め方 (新しいセッションすべてに適用)"),
    ("Open Straydog's dashboard in the browser", "Straydog のダッシュボードをブラウザーで開く"),
    ("Show or hide the panel on the right", "右パネルの表示を切り替える"),
    ("Show a web page or picture of the folder beside the conversation", "フォルダーの Web ページや画像を会話の横に表示する"),
    ("As Windows is set, dark, or light: the next of them", "Windows の設定、ダーク、ライトを順に切り替える"),
    ("Allow safe commands without asking, or not", "安全なコマンドを確認なしで実行するかを切り替える"),
    ("Start a session in another folder", "別のフォルダーでセッションを始める"),
    ("Settings: models and connections", "設定: モデルと接続先"),
    ("Settings: models on this PC", "設定: この PC のモデル"),
    ("Settings: general", "設定: 全般"),
    ("Open the engine's log", "エンジンのログを開く"),
    ("Close the app", "アプリを閉じる"),
    // Choices of the composer.
    ("Default", "既定"),
    ("None", "なし"),
    ("Low", "低"),
    ("Medium", "中"),
    ("High", "高"),
    ("Extra high", "最高"),
    ("Ask first", "毎回確認"),
    ("Accept edits", "編集を許可"),
    ("Auto", "自動"),
    ("Plan first", "計画のみ"),
    ("Effort: {0}", "思考: {0}"),
    ("Connections...", "接続先..."),
    ("Allow safe commands without asking", "安全なコマンドは確認なしで実行"),
    ("Show the thinking", "思考を表示"),
    // The transcript.
    ("This has text that reads like instructions to an AI. It went to the model marked as data, with word not to act on it.\n", "AI への指示のように読める文章が含まれています。データとして印を付け、従わないよう伝えた上でモデルに渡しました。\n"),
    ("… {0} lines above", "… 上に {0} 行"),
    ("… {0} more lines", "… 残り {0} 行"),
    ("loading", "読み込み中"),
    ("starting the engine", "エンジンを起動中"),
    ("waiting for the last engine to end", "前のエンジンの終了を待っています"),
    ("authenticate", "モデルを確認中"),
    ("drafter", "ドラフターを確認中"),
    ("prepare", "モデルを配置中"),
    ("load", "モデルを読み込み中"),
    ("warm", "GPU を準備中"),
    ("vram", "GPU メモリの空きを待っています"),
    ("The settings could not be saved: {0}\n", "設定を保存できませんでした: {0}\n"),
    ("Thinking…", "考え中…"),
    ("Compacting the session…", "セッションを圧縮しています…"),
    ("Waiting for the server to restart…", "サーバーの再起動を待っています…"),
    ("Waiting for the server: {0}", "サーバーを待っています: {0}"),
    ("Writing…", "書いています…"),
    ("Waiting for the engine…", "エンジンの起動を待っています…"),
    ("{0} s", "{0} 秒"),
    ("{0} so far", "{0} 件目"),
    ("Don't ask for safe commands", "安全なコマンドは確認なしで実行"),
    ("Give the variable's name below.", "下に変数の名前を入力してください。"),
    ("System", "システム"),
    ("The longest conversation is a number of tokens, or leave it empty.", "会話の長さの上限はトークン数で入力するか、空欄にしてください。"),
    ("On this PC (Straydog)", "この PC で動かす (Straydog)"),
    ("Another computer or an online service", "別のコンピューターやオンラインサービス"),
    ("ChatGPT plan (Codex)", "ChatGPT のプラン (Codex)"),
    ("Claude (API key)", "Claude (API キー)"),
    ("An address looks like http://192.168.0.100:8080/v1", "アドレスは http://192.168.0.100:8080/v1 のような形です"),
    ("An address looks like https://api.anthropic.com/v1, or leave it empty.", "アドレスは https://api.anthropic.com/v1 のような形です。空欄でもかまいません。"),
    ("Claude needs an API key: paste it here.", "Claude には API キーが必要です。ここに貼り付けてください。"),
    ("The key could not be removed from Windows Credential Manager, so it is still kept there: {0}", "キーを Windows の資格情報マネージャーから削除できませんでした。まだそこに保存されています: {0}"),
    ("The API key of {0} could not be kept in Windows Credential Manager ({1}), and it is no longer in the settings file. Paste it again in Settings, under its connection.", "{0} の API キーを Windows の資格情報マネージャーに保存できず、設定ファイルにも残っていません。設定のその接続で、もう一度貼り付けてください。（{1}）"),
    ("the web search", "Web 検索"),
    ("That looks like a key, and a name is written in the settings file and in messages. Give the connection a name of your own, and paste the key in the API key field.", "キーのように見えます。名前は設定ファイルやメッセージに書かれます。接続には別の名前を付けて、キーは API キーの欄に貼り付けてください。"),
    ("An address may not hold a key (user:password@ or ?key=): it is written in the settings file and in messages. Paste the key in the API key field, and leave it out of the address.", "アドレスにキーを含めることはできません（user:password@ や ?key= など）。アドレスは設定ファイルやメッセージに書かれます。キーは API キーの欄に貼り付け、アドレスからは外してください。"),
    ("That is not the name of an environment variable. A key itself goes in the API key field above.", "環境変数の名前ではないようです。キーそのものは上の API キーの欄に貼り付けてください。"),
    ("{0}: the saved API key is gone. Paste it again in Settings.", "{0}: 保存した API キーが見つかりません。設定でもう一度貼り付けてください。"),
    ("The graphics memory limit is a number of GB, or leave it empty.", "グラフィックメモリの上限は GB の数字で入力するか、空欄にしてください。"),
    ("Models and connections", "モデルと接続先"),
    ("Permissions and safety", "権限と安全"),
    ("Tools", "ツール"),
    ("First setup", "はじめの設定"),
    ("Run again", "もう一度行う"),
    ("Read pictures", "画像を読む"),
    ("Read pictures ({0})", "画像を読む ({0})"),
    ("Its file: {0}", "そのファイル: {0}"),
    ("{0} to download in all", "ダウンロードは合計 {0}"),
    ("Get it too", "一緒に取得する"),
    ("That can be done once the work under way has ended.", "今の作業が終わるとできるようになります。"),
    ("Setting up the vision file...", "画像用ファイルを準備しています..."),
    ("This model can't read pictures.", "このモデルは画像を読めません。"),
    ("This model's vision file did not load. Its log says why: /log", "このモデルの画像用ファイルを読み込めませんでした。理由はログにあります: /log"),
    ("Fetching the vision file.", "画像用ファイルを取得しています。"),
    ("Reading pictures takes a file of {0} that this model does not have yet.", "画像を読むには、このモデルにまだない {0} のファイルが必要です。"),
    ("Reading pictures is turned off.", "画像の読み取りはオフになっています。"),
    ("Turn on", "オンにする"),
    ("Download", "ダウンロード"),
    ("About", "このアプリについて"),
    ("Which versions these are", "バージョンを確かめる"),
    ("Straydog: reading...", "Straydog: 読み取り中..."),
    ("Straydog: not found", "Straydog: 見つかりません"),
    ("Straydog: no version given", "Straydog: バージョン不明"),
    ("Waiting", "確認待ち"),
    ("Error", "エラー"),
    ("after this work", "この作業のあとで"),
    ("when connected", "接続できたら"),
    ("when the engine is ready", "エンジンの準備ができたら"),
    ("at its next step", "次のステップの前に"),
    ("Send it into this work now", "今の作業に割り込んで送る"),
    ("Not recommended: it needs {0} of graphics memory free, and this PC has {1} for it", "非推奨: グラフィックスメモリの空きが {0} 必要で、この PC で使えるのは {1}"),
    ("This Straydog does not set up a model that it finds too big for this PC. A newer Straydog does.", "この Straydog は、この PC には大きすぎると判断したモデルを設定しません。新しい Straydog なら設定できます。"),
    ("Branch from here: a new session with the conversation up to this answer", "ここから分岐: この回答までの会話で新しいセッションを始めます"),
    ("This answer is not in the saved session yet, so there is nothing to branch from. Try again once it is.", "この回答はまだセッションに保存されていないため、分岐できません。保存されてからもう一度試してください。"),
    ("{0} (branch)", "{0} (分岐)"),
    ("The branch could not be made: {0}", "分岐を作れませんでした: {0}"),
    ("Discard", "破棄"),
    ("Optional. Kept in Credential Manager", "任意。資格情報マネージャーに保存"),
    ("Anything with an OpenAI-compatible API", "OpenAI 互換の API があるもの"),
    ("Previous change (P)", "前の変更 (P)"),
    ("Next change (N)", "次の変更 (N)"),
    ("Pinned", "ピン留め"),
    ("Pin", "ピン留め"),
    ("Unpin", "ピン留めを外す"),
    ("Pin this session to the top of the sidebar, or unpin it", "このセッションをサイドバーの上にピン留め、または解除"),
    ("Trust what it returns", "返す内容を信頼する"),
    ("What a trusted server returns no longer makes later commands ask. Its own tools still ask.", "信頼したサーバーの返す内容を理由に、以後のコマンドが確認することはなくなります。サーバー自身のツールは引き続き確認します。"),
    ("What this server returns no longer counts as web content, so later commands stop asking on its account. Its tools still ask, and in a session that already has web content they keep asking. What it returns is still marked as data.", "このサーバーの返す内容は Web の内容として扱われなくなり、それを理由に以後のコマンドが確認することはなくなります。ツール自体は引き続き確認し、すでに Web の内容がある会話では確認を続けます。返す内容は引き続きデータとして扱います。"),
    ("Needs an NVIDIA GPU. Downloads a model of about 75 GB", "NVIDIA の GPU が必要です。約 75 GB のモデルをダウンロードします"),
    ("Needs the Codex program from OpenAI, which is not on this PC", "OpenAI の Codex プログラムが必要ですが、この PC にはありません"),
    ("A key from console.anthropic.com, paid by use", "console.anthropic.com で発行するキー (従量課金)"),
    ("A key comes from console.anthropic.com, under API keys, and is paid by use: a Claude plan does not include it.", "キーは console.anthropic.com の API keys で発行します。従量課金で、Claude のプランには含まれません。"),
    ("Cannot run here: this PC has no NVIDIA GPU", "この PC では動かせません: NVIDIA の GPU がありません"),
    ("Cannot run here yet: the NVIDIA driver needs updating", "まだ動かせません: NVIDIA ドライバーの更新が必要です"),
    ("Cannot run here: this GPU is too old", "動かせません: この GPU は古すぎます"),
    ("Straydog runs on an NVIDIA GeForce RTX 20 series or newer, and this PC's GPU is older. You can still use a model on another computer, an online service, Codex or Claude.", "Straydog は NVIDIA GeForce RTX 20 シリーズ以降で動きます。この PC の GPU はそれより古いものです。別のコンピューターのモデル、オンラインサービス、Codex、Claude は使えます。"),
    ("Cannot run here: too little graphics memory", "この PC では動かせません: グラフィックスメモリが足りません"),
    ("Straydog cannot start on this PC", "この PC では Straydog を起動できません"),
    ("Started from inside the zip: extract it first", "zip の中から起動されています: 先に展開してください"),
    ("straydog.exe is not beside this app", "このアプリの隣に straydog.exe がありません"),
    ("Straydog needs an NVIDIA GPU with its driver, and this PC has none (an AMD or Intel GPU does not work). You can still use a model on another computer, an online service, Codex or Claude.", "Straydog には NVIDIA の GPU とそのドライバーが必要ですが、この PC にはありません (AMD や Intel の GPU では動きません)。別のコンピューターやオンラインサービス、Codex、Claude のモデルなら使えます。"),
    ("CUDA cannot use this PC's NVIDIA GPU: {0}. Install the newest driver from nvidia.com and try again, or use a model on another computer, an online service, Codex or Claude.", "CUDA がこの PC の NVIDIA GPU を使えません: {0}。nvidia.com から最新のドライバーを入れてもう一度試すか、別のコンピューターやオンラインサービス、Codex、Claude のモデルを使ってください。"),
    ("These models need {0} of graphics memory, and this PC's GPU has {1}. You can still use a model on another computer, an online service, Codex or Claude.", "これらのモデルには {0} のグラフィックスメモリが必要ですが、この PC の GPU は {1} です。別のコンピューターやオンラインサービス、Codex、Claude のモデルなら使えます。"),
    ("it finds no GPU", "GPU が見つかりません"),
    ("{0} of memory", "メモリ {0}"),
    ("{0} of memory, {1} free now", "メモリ {0} (いまの空き {1})"),
    ("{0} to download, {1} on the drive", "ダウンロード {0} / 保存 {1}"),
    ("{0} to download", "ダウンロード {0}"),
    ("{0} GB free on that drive", "そのドライブの空き {0} GB"),
    ("Runs on the graphics card alone: fastest", "GPU だけで動作: 最速"),
    ("Part of it waits in memory: a little slower", "一部はメモリ上: 少し遅い"),
    ("Part of it is read from the drive: slower", "一部はドライブから: 遅い"),
    ("A library Straydog needs is missing from its folder (cublas64_13.dll, cublasLt64_13.dll). Extract the whole zip again, and keep its files together.", "Straydog に必要なライブラリがフォルダーにありません (cublas64_13.dll、cublasLt64_13.dll)。zip 全体を展開し直し、ファイルをまとめて置いてください。"),
    ("A library beside Straydog is damaged or from another version. Extract the whole zip again, over the old folder.", "Straydog の隣のライブラリが壊れているか、別のバージョンです。zip 全体を古いフォルダーに上書きで展開し直してください。"),
    ("Windows refused to run straydog.exe (access denied). An antivirus or a policy on this PC may be blocking it.", "Windows が straydog.exe の実行を拒否しました (アクセス拒否)。ウイルス対策ソフトか、この PC のポリシーが止めている可能性があります。"),
    ("Windows refused to run {0}. Smart App Control, an antivirus or a policy on this PC may be blocking it: this app is not signed yet.", "Windows が {0} の実行を拒否しました。スマート アプリ コントロール、ウイルス対策ソフト、またはこの PC のポリシーが止めている可能性があります。このアプリはまだ署名されていません。"),
    ("This program was started from inside its zip file. Extract the whole zip to a folder, and start it there.", "zip ファイルの中から起動されています。zip 全体をフォルダーに展開し、そこから起動してください。"),
    ("straydog.exe was not found beside this app. Keep all the files of the zip together, or choose another way.", "このアプリの隣に straydog.exe が見つかりません。zip のファイルをまとめて置くか、別の方法を選んでください。"),
    ("Downloading {0}", "ダウンロード中 {0}"),
    ("Checking what has arrived of {0}", "受信済みの分を確認中 {0}"),
    ("Checking {0}", "確認中 {0}"),
    ("Preparing {0}", "準備中 {0}"),
    ("Building the faster-writing data {0}", "高速化データを作成中 {0}"),
    ("The download stopped and could not go on: {0}. What has arrived stays in the folder, and downloading again goes on from there.", "ダウンロードが止まり、続けられませんでした: {0}。受信済みの分はフォルダーに残り、もう一度ダウンロードするとその続きから再開します。"),
    ("The connection dropped. Trying again in {0} seconds, from where it stopped ({1} of {2})...", "接続が切れました。{0} 秒後に、止まったところから再開します ({1}/{2})..."),
    ("This model takes {0} GB on its drive, and that drive has {1} GB free. Choose a folder on another drive, or make room.", "このモデルはドライブ上で {0} GB 使いますが、そのドライブの空きは {1} GB です。別のドライブのフォルダーを選ぶか、空きを作ってください。"),
    ("Dogmind needs Windows 11.", "Dogmind には Windows 11 が必要です。"),
    ("This app ended unexpectedly last time. What happened is in {0}. /report writes a report you can send.", "前回、このアプリは予期せず終了しました。内容は {0} にあります。/report で送れるレポートを作成できます。"),
    ("Write a problem report", "問題のレポートを作成"),
    ("Write what can be sent about a problem", "問題について送れる内容を書き出します"),
    ("Written to {0}. Nothing is sent by itself: read it, and send it with what you were doing to whoever you had this app from, in any way you like.", "{0} に書き出しました。自動では送信されません。内容を確認し、何をしていたかを添えて、このアプリを受け取った相手に好きな方法で送ってください。"),
    ("The installer asks this app to close, to replace its files. It is still working, so it stays open: when it is done, quit it from the tray icon and press Try again in the installer.", "インストーラーがファイルを置き換えるため、このアプリを閉じるよう求めています。作業中なので開いたままにしています。終わったらトレイのアイコンから終了し、インストーラーで「もう一度試す」を押してください。"),
    ("It is done. Closing this takes away the program too: {0}", "完了しました。これを閉じるとプログラムも削除されます: {0}"),
    ("{0} of {1} GB in all ({2}%)", "全体で {1} GB 中 {0} GB ({2}%)"),
    ("This model's licence: {0}. Read its terms", "このモデルのライセンス: {0}。条項を読む"),
    ("{0} is on a network drive, and the engine has no network to read a model by. Choose a folder on a drive of this PC.", "{0} はネットワーク ドライブにあります。エンジンはネットワークを使えないため、そこのモデルを読めません。この PC のドライブのフォルダーを選んでください。"),
    ("{0} is on a FAT32 drive, which cannot hold a file of 4 GB or more, and a model's files are far larger. Choose a folder on an NTFS or exFAT drive.", "{0} は FAT32 のドライブにあります。FAT32 は 4 GB 以上のファイルを保存できず、モデルのファイルはそれよりずっと大きいものです。NTFS か exFAT のドライブのフォルダーを選んでください。"),
    ("You cannot write in {0}. Choose a folder of your own.", "{0} には書き込めません。自分のフォルダーを選んでください。"),
    ("{0} belongs to another account, so the engine could not be let read a model there. Choose a folder of your own.", "{0} は別のアカウントのフォルダーのため、エンジンにそこのモデルを読ませることができません。自分のフォルダーを選んでください。"),
    ("{0} is on a network drive. Commands cannot run there, as the sandbox has no network; files can still be read and edited.", "{0} はネットワーク ドライブにあります。サンドボックスはネットワークを使えないため、そこではコマンドを実行できません。ファイルの読み取りと編集はできます。"),
    ("{0} is on a drive with no file permissions ({1}). Commands cannot be contained there, so they do not run; files can still be read and edited.", "{0} はファイルのアクセス許可がないドライブ ({1}) にあります。コマンドを閉じ込められないため、そこでは実行しません。ファイルの読み取りと編集はできます。"),
    ("{0} belongs to another account. Commands need its permissions changed, so they may not run; a folder of your own is safer.", "{0} は別のアカウントのフォルダーです。コマンドの実行にはアクセス許可の変更が必要なため、実行できないことがあります。自分のフォルダーのほうが確実です。"),
    ("{0} is kept by OneDrive or another cloud, with files that may be online only. A command in the sandbox cannot bring those down and sees them as missing, and every build gets synced: a folder outside it works better.", "{0} は OneDrive などのクラウドで管理されていて、オンラインだけにあるファイルがあるかもしれません。サンドボックス内のコマンドはそれらを取得できず、ないものとして扱います。またビルドのたびに同期も走ります。クラウドの外のフォルダーのほうがうまく動きます。"),
    ("Written to {0}. Nothing is sent by itself: read it, and attach it at {1} with what you were doing.", "{0} に書き出しました。自動では送信されません。内容を確認し、何をしていたかを添えて {1} に添付してください。"),
    ("Remove this app from this PC", "この PC からアプリを削除"),
    ("Remove this app from this PC?", "この PC からアプリを削除しますか？"),
    ("Remove...", "削除..."),
    ("Remove all", "すべて削除"),
    ("Remove, keep the model", "削除してモデルは残す"),
    ("Removing...", "削除しています..."),
    ("This takes away what this app has left on this PC: its settings, sessions and logs, the keys it saved in Credential Manager, and what it opened to its sandbox. Your workspaces stay as they are.", "このアプリが PC に残したものを削除します: 設定、セッション、ログ、資格情報マネージャーに保存したキー、サンドボックスに開いた権限。ワークスペースのファイルはそのまま残ります。"),
    ("The downloaded model is not in the app's folder, and stays.", "ダウンロードしたモデルはアプリのフォルダーの外にあるので残ります。"),
    ("The downloaded model is in {0}. It goes too, unless you keep it.", "ダウンロードしたモデルは {0} にあります。残すことを選ばなければ一緒に削除します。"),
    ("It is done. To finish, close this and delete the folder this app runs from: {0}", "完了しました。最後に、これを閉じて、このアプリを実行しているフォルダーを削除してください: {0}"),
    ("The model stays in {0}.", "モデルは {0} に残っています。"),
    ("These could not be removed:", "次のものは削除できませんでした:"),
    ("This app did not make these, so they stay in {0}:", "次のものはこのアプリが作ったものではないため、{0} に残しています:"),
    ("As you write", "入力に合わせる"),
    ("This folder's path is too long for Windows to run a program in it, so commands will not start here. Files can still be read and edited. A folder with a shorter path has neither limit.", "このフォルダーはパスが長すぎて Windows がプログラムを起動できないため、ここではコマンドを実行できません。ファイルの読み書きはできます。パスの短いフォルダーならこの制限はありません。"),
    ("The first it offers", "最初に提供されるもの"),
    ("If it needs one", "必要な場合のみ"),
    ("Kept in Windows Credential Manager", "資格情報マネージャーに保存"),
    ("This cannot be undone. Files in the folder stay.", "元に戻せません。フォルダーのファイルは残ります。"),
    ("Only the agent's own edits come back. What this overwrites is saved first.", "戻せるのはエージェント自身の編集だけです。上書きされる内容は先に保存されます。"),
    ("If you have not signed in yet, run  codex login  in a terminal first.", "まだサインインしていなければ、先にターミナルで  codex login  を実行してください。"),
    ("The Codex program is not on this PC. Install it from OpenAI (github.com/openai/codex), run  codex login  in a terminal, then press Continue.", "Codex のプログラムがこの PC にありません。OpenAI のもの (github.com/openai/codex) をインストールし、ターミナルで  codex login  を実行してから、「続ける」を押してください。"),
    ("Found ANTHROPIC_API_KEY in the environment: leave this empty to use it.", "環境変数に ANTHROPIC_API_KEY があります。使うときは空のままにしてください。"),
    ("Only for a gateway", "ゲートウェイを使うときのみ"),
    ("Automatic", "自動"),
    ("Longest conversation (tokens)", "会話の長さの上限 (トークン)"),
    ("Faster-writing data (5 GB)", "高速化データ (5 GB)"),
    ("Brave Search key", "Brave Search のキー"),
    ("No skills yet.", "スキルはまだありません。"),
    ("Theme", "テーマ"),
    ("Text size", "文字の大きさ"),
    ("Small", "小"),
    ("Standard", "標準"),
    ("Large", "大"),
    ("Larger", "特大"),
    ("The conversation, what you type and the page beside it.", "会話、入力欄、横のページの文字が変わります。"),
    ("A skill turned off is neither offered to the model nor started with /name. A session already under way keeps its list, but cannot read one that is off.", "オフにしたスキルはモデルに示されず、/スキル名 でも起動しません。始まっているセッションは一覧をそのまま持ちますが、オフのスキルは読めません。"),
    ("Enabled", "有効"),
    ("Disabled", "無効"),
    ("Memory", "記憶"),
    ("A new version is here.", "新しいバージョンがあります。"),
    ("Restart to update", "再起動して更新"),
    ("It restarts into the new version when this work is done.", "この作業が終わったら、再起動して新しいバージョンにします。"),
    ("Not now", "やめる"),
    ("A new version of Dogmind is here", "Dogmind の新しいバージョンがあります"),
    ("Open the app to restart into it.", "アプリを開いて、再起動するか選んでください。"),
    ("This project", "このプロジェクトについて"),
    ("You, in every project", "あなたについて（すべてのプロジェクト）"),
    ("What the model keeps from one session to the next, and a new session reads. Remove what is wrong or should not be kept.", "モデルがセッションをまたいで覚えていることで、新しいセッションが読み込みます。間違っているものや残したくないものは削除してください。"),
    ("That skill is turned off. Settings > Skills turns it on.", "そのスキルはオフになっています。設定 > スキル でオンにできます。"),
    ("Show the model's thinking", "モデルの思考を表示"),
    ("Group tool calls into one line", "ツールの実行を 1 行にまとめる"),
    ("Connections", "接続先"),
    ("Details", "詳細"),
    ("How it runs", "動かし方"),
    ("Address", "アドレス"),
    ("Model", "モデル"),
    ("API key", "API キー"),
    ("Replace", "置き換える"),
    ("chevron-right|Other ways to give the key", "chevron-right|キーのその他の指定方法"),
    ("chevron-down|Other ways to give the key", "chevron-down|キーのその他の指定方法"),
    ("Read from an environment variable", "環境変数から読む"),
    ("Variable name", "変数の名前"),
    ("Read from the variable {0}", "環境変数 {0} から読みます"),
    ("Read from this file: {0}", "このファイルから読みます: {0}"),
    ("The key was removed.", "キーを削除しました。"),
    ("Another connection has that name.", "同じ名前の接続先があります。"),
    ("Connecting.", "接続しています。"),
    ("Connecting again.", "接続し直しています。"),
    ("Choose a model", "モデルを選ぶ"),
    ("Where models are kept", "モデルの保存先"),
    ("Graphics memory limit (GB)", "グラフィックメモリの上限 (GB)"),
    ("Longest conversation", "会話の長さの上限"),
    ("Engine", "エンジン"),
    ("Open the dashboard", "ダッシュボードを開く"),
    ("When to ask", "確認のタイミング"),
    ("Web search and reading", "Web の検索と閲覧"),
    ("MCP servers", "MCP サーバー"),
    ("No server yet.", "サーバーはまだありません。"),
    ("Import from Claude and Codex", "Claude と Codex から取り込む"),
    ("Licenses", "ライセンス"),
    ("LICENSE.txt is not beside this program.", "このプログラムの隣に LICENSE.txt がありません。"),
    ("Check for updates", "更新を確認"),
    ("Checking…", "確認中…"),
    ("Update automatically", "自動で更新する"),
    ("It downloads in the background and asks before restarting.", "バックグラウンドでダウンロードし、再起動の前に確認します。"),
    ("Dogmind {0} is up to date.", "Dogmind {0} は最新です。"),
    ("Dogmind {0} is available.", "Dogmind {0} の新しいバージョンがあります。"),
    ("Open the release page", "リリースページを開く"),
    ("No release has been published yet.", "公開されているリリースはまだありません。"),
    ("This test build asks no one.", "このテストビルドはどこにも問い合わせません。"),
    ("Could not reach GitHub.", "GitHub に接続できませんでした。"),
    ("GitHub did not answer in time.", "GitHub からの応答が時間内にありませんでした。"),
    ("GitHub answered with an error ({0}). Try again later.", "GitHub がエラーを返しました ({0})。しばらくしてからもう一度お試しください。"),
    ("GitHub's answer could not be read.", "GitHub からの応答を読み取れませんでした。"),
    ("Dogmind asks GitHub for the newest release at most once a day. It sends only its name and version. When there is a newer one, it downloads the setup in the background and checks it, and restarts into it only when you press Restart to update.", "Dogmind は 1 日に 1 回まで、GitHub に最新のリリースを問い合わせます。送るのは名前とバージョンだけです。新しいリリースがあると、セットアップをバックグラウンドでダウンロードして検証し、「再起動して更新」を押したときだけ再起動して更新します。"),
    ("Downloading Dogmind {0}… {1}%", "Dogmind {0} をダウンロードしています… {1}%"),
    ("Dogmind {0} is ready to install.", "Dogmind {0} をインストールする準備ができました。"),
    ("The update download was cancelled, and its partial file deleted. The next automatic look tries again.", "更新のダウンロードを中止し、途中のファイルを削除しました。次の自動確認でもう一度試します。"),
    ("The update was not downloaded: this connection is metered. Check for updates downloads it anyway.", "更新はダウンロードしませんでした。この接続は従量制です。「更新を確認」を押すとダウンロードします。"),
    ("The update could not be downloaded: GitHub could not be reached. Its partial file is deleted, and the next automatic look tries again.", "更新をダウンロードできませんでした。GitHub に接続できません。途中のファイルは削除しました。次の自動確認でもう一度試します。"),
    ("The update could not be downloaded: GitHub answered with an error ({0}). The next automatic look tries again.", "更新をダウンロードできませんでした。GitHub がエラーを返しました ({0})。次の自動確認でもう一度試します。"),
    ("The update was not downloaded: its address is not one Dogmind accepts. Nothing was changed.", "更新はダウンロードしませんでした。Dogmind が受け付けないアドレスです。何も変更していません。"),
    ("The download did not match the size and hash GitHub published, so it was deleted. Nothing was run or changed. The next automatic look tries again.", "ダウンロードしたファイルが GitHub の公開しているサイズとハッシュに一致しなかったため、削除しました。何も実行も変更もしていません。次の自動確認でもう一度試します。"),
    ("The update could not be saved: {0}. Its partial file is deleted, and the next automatic look tries again.", "更新を保存できませんでした: {0}。途中のファイルは削除しました。次の自動確認でもう一度試します。"),
    ("The downloaded update is gone. Check for updates to download it again.", "ダウンロードした更新が見つかりません。「更新を確認」でもう一度ダウンロードしてください。"),
    ("The update could not be started: {0}", "更新を開始できませんでした: {0}"),
    ("Close", "閉じる"),
    ("How should the model run?", "モデルをどう動かしますか?"),
    ("Another computer, or an online service", "別のコンピューター、またはオンラインサービス"),
    ("Found ANTHROPIC_API_KEY in the environment", "環境変数 ANTHROPIC_API_KEY が見つかりました"),
    ("Skip for now", "あとで設定する"),
    ("Your Claude API key", "Claude の API キー"),
    ("Begin", "はじめる"),
    ("Continue while it downloads", "ダウンロードしながら次へ"),
    ("Writing {0}", "{0} を書いています"),
    ("Writing a file", "ファイルを書いています"),
    ("Click to see what it is writing", "クリックで書いている内容を表示"),
    ("Click to hide what it is writing", "クリックで書いている内容を隠す"),
    (", ", "・"),
    ("exit code {0}", "終了コード {0}"),
    ("Tool calls", "ツールの呼び出し"),
    ("Show or hide these calls", "これらの呼び出しを表示 / 隠す"),
    ("Click to show these calls", "クリックで呼び出しの一覧を表示"),
    ("Click to hide these calls", "クリックで呼び出しの一覧を隠す"),
    ("Went back: 1 file restored.\n", "戻しました: 1 ファイルを復元しました。\n"),
    ("Went back: {0} files restored.\n", "戻しました: {0} ファイルを復元しました。\n"),
    ("Could not connect: {0}", "接続できませんでした: {0}"),
    ("The server refused the API key. Check it in Settings.", "サーバーが API キーを受け付けませんでした。設定で確かめてください。"),
    ("No answer from {0}. Is it running?", "{0} から応答がありません。起動していますか?"),
    // /status.
    ("not found", "見つかりません"),
    ("Provider", "接続先"),
    ("Connection", "接続"),
    ("Folder", "フォルダー"),
    ("Branch", "ブランチ"),
    ("Session", "セッション"),
    ("Approvals", "承認"),
    ("Web", "Web"),
    ("Effort", "思考"),
    ("Conversation", "会話"),
    ("Document", "ドキュメント"),
    ("Conversation length", "会話の長さ"),
    ("none", "なし"),
    ("not saved yet", "未保存"),
    ("off", "オフ"),
    ("on, with DuckDuckGo and Bing", "オン (DuckDuckGo と Bing)"),
    ("on, with Brave", "オン (Brave)"),
    ("on, with DuckDuckGo and Bing: the search key could not be read", "オン (DuckDuckGo と Bing): 検索キーを読み取れませんでした"),
    ("{0} of {1} tokens", "{0} / {1} トークン"),
    // Slash commands' answers.
    ("This session has no messages yet.", "このセッションにはまだメッセージがありません。"),
    ("Stop the work first.", "先に作業を止めてください。"),
    ("Asking again.", "もう一度送ります。"),
    ("Copied the last answer.", "最後の回答をコピーしました。"),
    (", last answer at {0} tok/s", "、最後の回答は {0} tok/s"),
    ("{0} requests: {1} prompt tokens, {2} of them from the cache, {3} written{4}. The conversation is {5} of {6}.", "{0} リクエスト: プロンプト {1} トークン (うちキャッシュ {2})、生成 {3}{4}。会話の長さは {5} / {6} です。"),
    ("Nothing to show yet. Name a file: /preview page.html", "表示できるものがまだありません。ファイルを指定してください: /preview page.html"),
    ("{0} servers added to {1}. They start the next time this app does.", "{0} 個のサーバーを {1} に追加しました。次にアプリを起動したときに開始します。"),
    ("No server yet. {0} lists them, and /mcp import copies Claude's and Codex's in.", "サーバーはまだありません。{0} に記述するか、/mcp import で Claude と Codex の設定を取り込めます。"),
    ("Nothing is kept yet. The model keeps what later sessions would want, and you can ask it to.", "保持している内容はまだありません。後のセッションで役立つことはモデルが記録します。頼んで記録させることもできます。"),
    ("Every folder on PATH is open to sandboxed commands.", "PATH のフォルダーはすべてサンドボックス内のコマンドから使えます。"),
    ("Windows will ask to let sandboxed commands read and run: {0}. Nothing else changes.", "サンドボックス内のコマンドに {0} の読み取りと実行を許可するか、Windows が確認します。他は変わりません。"),
    ("Straydog is not what this session is connected to, so it has no dashboard to open.", "このセッションの接続先は Straydog ではないので、開けるダッシュボードがありません。"),
    ("The thinking is shown.", "思考を表示します。"),
    ("The thinking is hidden.", "思考を隠します。"),
    ("Safe commands run without asking.", "安全なコマンドは確認なしで実行します。"),
    ("Every command asks.", "すべてのコマンドで確認します。"),
    ("No such command: {0}. /help lists them.", "{0} というコマンドはありません。/help で一覧を見られます。"),
    ("There is nothing to find in yet.", "検索できる内容がまだありません。"),
    ("This session has nothing to go back to yet.", "このセッションにはまだ戻れる地点がありません。"),
    ("That session is at work: stop it first.", "そのセッションは作業中です。先に止めてください。"),
    // The window.
    ("New session", "新しいセッション"),
    ("plus|New session", "plus|新しいセッション"),
    ("settings|Settings", "settings|設定"),
    ("Rejected", "却下しました"),
    ("{0}  ·  {1} risk", "{0}  ·  リスク: {1}"),
    ("safe", "安全"),
    ("low", "低"),
    ("medium", "中"),
    ("high", "高"),
    ("Reason (optional)", "理由 (任意)"),
    ("Find in this conversation", "この会話を検索"),
    ("Search sessions", "セッションを検索"),
    ("Yes (Y)", "承認 (Y)"),
    ("Always (A)", "常に許可 (A)"),
    ("No (N)", "却下 (N)"),
    ("Message Dogmind", "Dogmind にメッセージ"),
    ("working", "作業中"),
    ("ready", "準備完了"),
    ("ready · Straydog", "準備完了 · Straydog"),
    ("not connected", "未接続"),
    ("no engine", "エンジンなし"),
    ("no model yet", "モデル未導入"),
    ("setting up the model", "モデルを準備中"),
    ("starts when you write", "入力すると起動します"),
    ("this address is not from your messages, a search or a page, and an address can carry data out", "このアドレスはあなたのメッセージにも検索結果にもページにも出ていません。アドレスにはデータを載せて外へ送れます。"),
    ("web content is in this conversation, and an address can carry data out in its path", "この会話にはウェブの内容が含まれています。アドレスのパスにもデータを載せて外へ送れます。"),
    ("this query looks like it holds a secret or a private path, and it would go to a search index", "この検索語には秘密の値か個人のパスが含まれているようです。検索エンジンに送られます。"),
    ("the engine stopped", "エンジン停止"),
    ("connecting", "接続中"),
    ("the engine did not start: {0}", "エンジンを起動できませんでした: {0}"),
    ("this plan offers no models", "このプランで使えるモデルがありません"),
    ("Claude needs an API key", "Claude には API キーが必要です"),
    // The folder.
    ("Choose a folder...", "フォルダーを選択..."),
    ("Choose a folder", "フォルダーを選択"),
    ("New session here", "ここで新しいセッション"),
    // Text menus.
    ("Cut", "切り取り"),
    ("Copy", "コピー"),
    ("Paste", "貼り付け"),
    ("Select all", "すべて選択"),
    // The tray.
    ("Open Dogmind", "Dogmind を開く"),
    ("Quit Dogmind", "Dogmind を終了"),
    ("Dogmind is waiting for you", "Dogmind が確認を待っています"),
    ("It asks to use {0}.", "{0} の使用を求めています。"),
    ("Dogmind has finished", "Dogmind の作業が終わりました"),
    ("Dogmind stopped with an error", "Dogmind はエラーで止まりました"),
    ("The window is too narrow for the preview: make it wider.", "ウィンドウが狭くてプレビューを表示できません。広げてください。"),
    ("The engine is not ready yet: ask again once it is.", "エンジンの準備がまだです。準備ができてからもう一度お試しください。"),
    ("The work is done.", "作業が完了しました。"),
    ("Dogmind is still here", "Dogmind は動作を続けています"),
    ("It keeps running in the tray, with the engine loaded. Quit it from the tray icon's menu.", "エンジンを読み込んだままタスクトレイに常駐します。終了はトレイアイコンのメニューからどうぞ。"),
    // The approval card.
    ("\n\nin {0}", "\n\n場所: {0}"),
    ("As written: {0}", "モデルが書いた行: {0}"),
    ("{0}\n... and {1} more characters", "{0}\n... 残り {1} 文字"),
    ("Always saves: {0}", "常に許可で保存されるルール: {0}"),
    ("Always allows this file, for this session: {0}", "常に許可: このセッションの間、このファイル: {0}"),
    ("Always allows this on the network, for this session: {0}", "常に許可: このセッションの間、次のネットワーク利用: {0}"),
    ("Folders commands can read", "コマンドが読めるフォルダー"),
    ("Commands can read everything in these folders, and that includes any keys or passwords kept there. They cannot write there, and the network stays off unless you approve it. Files that look like keys are left out only if they are there when you add the folder, not ones made later.", "コマンドはこれらのフォルダーの中身をすべて読めます。そこに置かれた鍵やパスワードも読めます。書き込みはできず、ネットワークも許可しない限り使えません。鍵に見えるファイルを除外するのは、追加したときに既にあったものだけで、後から作られたものは除外されません。"),
    ("Add a folder...", "フォルダーを追加..."),
    ("Stop", "停止"),
    ("Stopping, and undoing what was done...", "停止して、元に戻しています..."),
    ("Opening {0}: {1} so far", "{0} を開いています: ここまで {1} 件"),
    ("Taking back {0}: {1} so far", "{0} の許可を取り消しています: ここまで {1} 件"),
    ("Not finished: this is put right when the app next starts.", "未完了です。次にアプリを起動したときに整えます。"),
    ("{0} that looked like keys when it was added are left out.", "追加したときに鍵に見えた {0} 件は除外されています。"),
    ("{0} could not be changed, so commands cannot read those.", "{0} 件は変更できなかったため、コマンドからは読めません。"),
    ("Commands can now read {0}. {1} that look like keys were left out, as they are now: one made later can be read.", "コマンドが {0} を読めるようになりました。鍵に見える {1} 件は今の時点で除外しました。後から作られたものは読めます。"),
    ("Commands can now read {0}.", "コマンドが {0} を読めるようになりました。"),
    ("Stopped, and all of it undone.", "停止し、すべて元に戻しました。"),
    ("Commands can no longer read {0}. A file moved out of it while it was on this list keeps that permission, so a command could read it where it can reach it: resetting that file's permissions ends that.", "コマンドは {0} を読めなくなりました。一覧にある間にこのフォルダーから移動したファイルにはこの許可が残るため、コマンドが届く場所にあれば読めます。そのファイルのアクセス権をリセットすると読めなくなります。"),
    ("Commands can no longer read {0}, which had moved to {1}. A file moved out of it while it was on this list keeps that permission, so a command could read it where it can reach it: resetting that file's permissions ends that.", "{1} に移動していた {0} を、コマンドは読めなくなりました。一覧にある間にこのフォルダーから移動したファイルにはこの許可が残るため、コマンドが届く場所にあれば読めます。そのファイルのアクセス権をリセットすると読めなくなります。"),
    ("{0} is no longer on its drive, so nothing there was left to take back. If it was copied somewhere with its permissions, that copy can still be read.", "{0} はドライブ上にもうないため、取り消すものはありませんでした。アクセス権ごとどこかにコピーされていた場合、そのコピーはまだ読めます。"),
    ("It is no longer on its drive. Remove it to take it off this list.", "ドライブ上にもうありません。削除すると一覧から消えます。"),
    ("Its drive is not here.", "ドライブが接続されていません。"),
    ("{0} does not look like this app's folder, so nothing in it was removed.", "{0} はこのアプリのフォルダーではないようなので、中のものは削除していません。"),
    ("{0} may still be open to commands: what was given could not be taken back.", "{0} はまだコマンドから使える状態かもしれません。許可を取り消せませんでした。"),
    ("The list of folders opened to commands could not be read, so what it named may still be open to them. It is kept here, for anyone who wants to look.", "コマンドに開いたフォルダーの一覧を読めなかったため、そこにあったフォルダーはまだコマンドから読める状態かもしれません。一覧はここに残してあります。"),
    ("The drive it was on is not here, so what it was given cannot be taken back yet. It stays on the list, and that is tried again each time the app starts.", "ドライブが接続されていないため、まだ許可を取り消せません。一覧に残し、アプリを起動するたびにもう一度試します。"),
    ("{0} could not all be taken back, and the app will try again when it next starts: {1}", "{0} の許可をすべては取り消せませんでした。次にアプリを起動したときにもう一度試します: {1}"),
    ("That is not a folder.", "それはフォルダーではありません。"),
    ("That is too wide to open to commands: name a project or data folder inside it.", "範囲が広すぎるため、コマンドに開けません。その中のプロジェクトやデータのフォルダーを指定してください。"),
    ("That is a place keys are kept, so it cannot be opened to commands.", "そこは鍵が保管されている場所なので、コマンドに開けません。"),
    ("That is inside a folder already named, or holds one: remove that one first.", "すでに指定したフォルダーの中にあるか、それを含んでいます。先にそちらを外してください。"),
    ("Windows would not let its access be changed.", "Windows がアクセス権の変更を許可しませんでした。"),
    ("Always allows this folder, for this session: {0}", "常に許可: このセッションの間、このフォルダー: {0}"),
    ("Always allows this exact call, for this session.", "常に許可: このセッションの間、これと同じ呼び出し。"),
    // The panel on the right.
    ("{0} in conflict", "競合 {0}"),
    ("Preview", "プレビュー"),
    ("Tasks", "タスク"),
    ("and {0} more", "ほか {0} 件"),
    ("Changes", "変更"),
    ("and {0} more: /diff", "ほか {0} 件: /diff"),
    ("Web content is here: commands ask", "Web の内容を含むため、コマンドは毎回確認します"),
    // The preview.
    ("{0}: starting...", "{0}: 起動中..."),
    ("There was no vision file to turn on or off any more: nothing was changed.", "切り替える画像用ファイルがもうありません。何も変更していません。"),
    ("Shown as {0}.", "{0} として表示しています。"),
    ("an encoding it does not know", "不明な文字コード"),
    ("{0} is gone. Nothing is shown until it is written again.", "{0} がなくなりました。もう一度書かれるまで何も表示しません。"),
    ("{0} requests to outside were blocked here. Open in browser to let them through.", "外部への通信を {0} 件ブロックしました。通すにはブラウザーで開いてください。"),
    ("There is no {0} in this folder.", "このフォルダーに {0} はありません。"),
    ("The preview shows web pages, pictures, PDFs, Markdown and source files.", "プレビューできるのは Web ページ、画像、PDF、Markdown、ソースファイルです。"),
    ("Only the first {0} KB are shown.", "先頭の {0} KB だけを表示しています。"),
    ("This file is not text, so it has no preview.", "このファイルはテキストではないため、プレビューできません。"),
    ("The preview shows what is in this folder.", "プレビューできるのはこのフォルダー内のファイルです。"),
    ("The preview did not open: {0}", "プレビューを開けませんでした: {0}"),
    ("The preview needs the WebView2 runtime of Windows, which did not start: {0}", "プレビューには Windows の WebView2 ランタイムが必要ですが、起動できませんでした: {0}"),
    // Panels.
    ("Delete this session?", "このセッションを削除しますか？"),
    ("Delete", "削除"),
    ("Keep it", "残す"),
    ("Thinking", "思考"),
    ("Safe commands", "安全なコマンド"),
    ("Language of answers", "回答の言語"),
    ("Display language", "表示言語"),
    ("Appearance", "外観"),
    ("Your instructions", "カスタム指示"),
    ("Data folder", "データフォルダー"),
    ("Kind", "種類"),
    ("No changes.", "変更はありません。"),
    ("On this PC, with Straydog", "この PC で (Straydog)"),
    ("Codex, with a ChatGPT plan", "Codex (ChatGPT プラン)"),
    ("Claude, with an API key", "Claude (API キー)"),
    ("Continue", "続ける"),
    ("Where is the server?", "サーバーはどこにありますか？"),
    ("Back", "戻る"),
    ("Help", "ヘルプ"),
    ("Done", "完了"),
    ("About Dogmind", "Dogmind について"),
    ("Settings", "設定"),
    ("Connect again", "再接続"),
    ("Engine's log", "エンジンのログ"),
    ("Dashboard", "ダッシュボード"),
    ("Name", "名前"),
    ("Save", "保存"),
    ("Cancel", "キャンセル"),
    ("Open a session", "セッションを開く"),
    ("Open", "開く"),
    ("{0}  ·  {1} messages", "{0}  ·  {1} 件のメッセージ"),
    ("Go back to the beginning of", "このターンの最初に戻る"),
    ("Files and chat", "ファイルと会話"),
    ("Files only", "ファイルのみ"),
    ("Chat only", "会話のみ"),
    ("no file changed since", "以降の変更なし"),
    ("1 file changed since", "以降 1 ファイルを変更"),
    ("{0} files changed since", "以降 {0} ファイルを変更"),
    ("  ·  files only", "  ·  ファイルのみ"),
    ("Skills", "スキル"),
    ("Looking...", "確認中..."),
    ("What may always run here", "ここで常に実行してよいもの"),
    ("Remove", "削除"),
    ("This exact command", "このコマンドそのもの"),
    ("Without asking", "確認なし"),
    ("Any command that starts so", "このように始まるコマンド"),
    ("No rules yet.", "ルールはまだありません。"),
    ("Browse...", "参照..."),
    ("Which model?", "どのモデルにしますか？"),
    ("Asking Straydog...", "Straydog に確認中..."),
    ("Use this", "これを使う"),
    ("New", "新規"),
    ("Where shall we work?", "どのフォルダーで作業しますか？"),
    ("Start here", "ここで始める"),
    ("Cancelling...", "キャンセル中..."),
    ("Cancel download", "ダウンロードを中止"),
    ("Download / verify", "ダウンロード / 検証"),
    ("Off", "オフ"),
    ("Japanese, always", "常に日本語"),
    ("English, always", "常に英語"),
    ("As you write: Japanese on this system", "入力に合わせる (この環境では日本語)"),
    ("Dark", "ダーク"),
    ("Light", "ライト"),
    ("As Windows is set", "Windows の設定に合わせる"),
    ("Choose one of these.", "この中から選んでください。"),
    ("Build it too (about 5 GB more to download)", "一緒に作る (ダウンロードが約 5 GB 増えます)"),
    ("Leave it out", "作らない"),
    ("(no title)", "(無題)"),
    ("No session has that", "該当するセッションはありません"),
    ("Rename...", "名前を変更..."),
    ("Delete...", "削除..."),
    ("Working", "作業中"),
    ("Choose a connection to move.", "移動する接続先を選んでください。"),
    ("Saved.", "保存しました。"),
    ("No changes", "変更なし"),
    ("Download stopped. What has arrived stays in the folder, and downloading again goes on from there.", "ダウンロードを中止しました。受信済みの分はフォルダーに残り、次回はその続きから再開します。"),
    ("Choose a model from the list.", "リストからモデルを選んでください。"),
    ("Choose a folder for the model.", "モデルのフォルダーを選んでください。"),
    ("Setting up {0}...", "{0} を準備中..."),
    ("Straydog is not what is connected, so it has no dashboard to open.", "接続先が Straydog ではないので、開けるダッシュボードがありません。"),
    ("Choose a turn.", "ターンを選んでください。"),
    ("That part of the conversation was compacted or rewound past: only its files can go back.", "その部分の会話は圧縮済みか巻き戻し済みのため、ファイルしか戻せません。"),
    ("Choose a rule.", "ルールを選んでください。"),
    ("Choose a folder from the list, or another one.", "リストのフォルダーか、別のフォルダーを選んでください。"),
    ("A session needs a folder to work in: choose one.", "セッションには作業フォルダーが必要です。選んでください。"),
    ("Providers", "接続先"),
    ("Models on this PC", "この PC のモデル"),
    ("General", "全般"),
    // Tools.
    ("Read", "読み取り"),
    ("List", "一覧"),
    ("Find", "ファイル検索"),
    ("Search", "内容検索"),
    ("Diff", "差分"),
    ("Edit", "編集"),
    ("Write", "書き込み"),
    ("Run", "実行"),
    ("Check a command", "コマンドの確認"),
    ("Stop a command", "コマンドの停止"),
    ("Skill", "スキル"),
    ("Remember", "覚える"),
    ("Forget", "忘れる"),
    ("Web search", "Web 検索"),
    ("Read the web", "Web 読み取り"),
    // What the agent says.
    (llm::SERVER_GONE, "サーバーから応答がなくなりました。会話はそのまま残っています。サーバーが動いているか確認してから、もう一度送ってください。"),
    (llm::SERVER_OUT_OF_MEMORY, "サーバーがメモリ不足で停止しました。会話はそのまま残っています。サーバーのメモリを空けてから、もう一度送ってください。"),
    (llm::ENGINE_GONE, "エンジンが停止しました。会話はそのまま残っています。もう一度入力するとエンジンが再起動し、ここから続けられます。原因は /log で確認できます。"),
    (llm::ENGINE_OUT_OF_MEMORY, "エンジンがメモリ不足で停止しました。会話はそのまま残っています。他のアプリを閉じてからもう一度入力すると、エンジンが再起動し、ここから続けられます。"),
    (llm::SERVER_BUSY, "サーバーが処理中か読み込み中のため、依頼を受け付けられませんでした。会話はそのまま残っています。少し待ってから、もう一度送ってください。"),
    (llm::KEY_REFUSED, "サーバーが API キーを受け付けませんでした。会話はそのまま残っています。設定でキーを確認してから、もう一度送ってください。"),
    (llm::NOT_FOUND, "サーバーにこのアドレスまたはモデルが見つかりません。会話はそのまま残っています。設定で URL とモデルを確認してください。"),
    (llm::CONTEXT_FULL, "この会話は要約してもモデルには長すぎます。会話はそのまま残っています。/new で新しいセッションを始めるか、/rewind で前のターンに戻ってください。"),
    (llm::MEMORY_FULL, "この長さの会話を処理するにはメモリが足りません。会話はそのまま残っています。他のアプリを閉じてもう一度送るか、/new で新しいセッションを始めてください。"),
    (llm::NOT_READY, "エンジンの準備がまだできていません。メッセージは残っています。準備ができたら、もう一度送ってください。"),
    (llm::UNFINISHED, "サーバーが回答を最後まで返しませんでした。会話はそのまま残っています。もう一度送ってください。"),
    (llm::UNREADABLE, "サーバーから読み取れない応答が届きました。会話はそのまま残っています。もう一度送ってください。"),
    (llm::BROKE_OFF, "回答が途中で切れました。続きを頼んでいます。"),
    (llm::SERVER_RESTARTING, "サーバーが再起動中です。依頼は保持されていて、準備ができ次第送られます（最長 10 分）。停止を押すと待つのをやめます。"),
    (llm::NO_CHECKPOINT, "要約を作れなかったため、いちばん古いターンを削除して圧縮します。"),
    (llm::START_FAILED, "エンジンが読み込み中に停止しました。原因は /log で確認できます。もう一度入力すると再試行します。"),
    (llm::START_OUT_OF_MEMORY, "エンジンが読み込み中にメモリ不足になりました。他のアプリを閉じてから、もう一度入力してください。"),
    ("the Codex CLI was not found: install it, then sign in with ChatGPT", "Codex CLI が見つかりません。インストールしてから ChatGPT でサインインしてください。"),
    ("codex has no stdin", "Codex を起動できませんでした。もう一度送ってください。"),
    ("codex has no stdout", "Codex を起動できませんでした。もう一度送ってください。"),
    ("the Codex app-server ended", "Codex が終了しました。会話はそのまま残っています。もう一度送ってください。"),
    ("the Codex app-server sent nothing for too long", "Codex から長い間応答がありません。会話はそのまま残っています。もう一度送ってください。"),
    ("the Codex app-server returned no thread", "Codex が処理を始められませんでした。会話はそのまま残っています。もう一度送ってください。"),
    ("the Codex app-server did not start", "Codex を起動できませんでした。会話はそのまま残っています。もう一度送ってください。"),
    ("the Codex turn ended with no answer", "Codex が回答せずに終了しました。会話はそのまま残っています。もう一度送ってください。"),
    ("Codex returned malformed structured output", "Codex から読み取れない回答が届きました。会話はそのまま残っています。もう一度送ってください。"),
    ("Codex returned tool_calls with no call", "Codex から読み取れない回答が届きました。会話はそのまま残っています。もう一度送ってください。"),
    ("Codex returned a tool call with no name", "Codex から読み取れない回答が届きました。会話はそのまま残っています。もう一度送ってください。"),
    ("Codex returned an answer of no known kind", "Codex から読み取れない回答が届きました。会話はそのまま残っています。もう一度送ってください。"),
    ("There is nothing to try again.", "やり直せるものがありません。"),
    ("This session has nothing to rewind.", "このセッションには巻き戻せるものがありません。"),
    ("That turn is gone.", "そのターンはもうありません。"),
    ("That part of the conversation was compacted: it cannot go back.", "その部分の会話は圧縮済みのため、戻せません。"),
    ("Session compacted: the earlier conversation is now a summary", "セッションを圧縮しました: これまでの会話を要約に置き換えました"),
    ("There is nothing to compact yet: the conversation is short.", "会話がまだ短いので、圧縮するものはありません。"),
    ("Session compacted: the oldest turns were dropped", "セッションを圧縮しました: いちばん古いターンを削除しました"),
    ("the engine stopped answering: /log shows why, and writing again tries once more", "エンジンの応答が止まりました。原因は /log で確認できます。もう一度入力すると再試行します。"),
    ("another Straydog is running on this PC (process {0}). Two on one GPU fight over its memory and the machine's, so this one was not started. Close it and restart the engine, or reach it as an OpenAI-compatible provider.", "この PC で別の Straydog が動いています (プロセス {0})。1 つの GPU で 2 つ動かすとメモリを奪い合うため、起動しませんでした。それを閉じてからエンジンを起動し直すか、OpenAI 互換の接続先として接続してください。"),
    ("port {0} on this PC is taken. Choose another port for the engine in settings.json, or close what holds it.", "この PC のポート {0} は使用中です。settings.json でエンジンの別のポートを選ぶか、使っているものを閉じてください。"),
    // Pictures.
    (images::NOT_A_PICTURE, "このファイルは添付できる画像ではありません。添付できるのは PNG、JPEG、WebP、GIF です。"),
    (images::DAMAGED, "この画像を読み取れません。ファイルが壊れているか、途中で切れています。"),
    (images::TOO_LARGE, "この画像は大きすぎて添付できません。上限は 32 MB、6400 万ピクセルです。"),
    (images::UNREADABLE_FILE, "このファイルを開けませんでした。"),
    (images::NOT_SAVED, "画像をセッションに保存できなかったため、何も送信していません。"),
    (llm::NO_VISION, "このモデルは画像を読めないため、画像を送信しませんでした。会話と入力した文章は残っています。画像を使うには、画像を読めるモデルを選んでください。この PC のモデルには、モデルの横にビジョンファイルが必要です。"),
    ("Add a picture to the next message: /attach file, or choose one", "次のメッセージに画像を添付: /attach ファイル、または選択"),
    ("Attach a picture", "画像を添付"),
    ("Choose pictures", "画像を選択"),
    ("Pictures", "画像"),
    ("The picture on the clipboard could not be read.", "クリップボードの画像を読み取れませんでした。"),
    ("Up to {0} pictures can go with one message.", "1 つのメッセージに添付できる画像は {0} 枚までです。"),
    ("File not found", "ファイルが見つかりません"),
    ("This picture cannot be shown here", "この画像はここに表示できません"),
    ("{0} is no longer there.", "{0} はもうありません。"),
    ("This model on this PC cannot see pictures: it has no vision file. Not every model has one. If yours does, put its mmproj-*.gguf beside the model, in {0}, and start the engine again.", "この PC のモデルにはビジョンファイルがないため、画像を読めません。ビジョンファイルのないモデルもあります。お使いのモデルにある場合は、mmproj-*.gguf をモデルと同じ場所 ({0} の中) に置いて、エンジンを起動し直してください。"),
    // The transcript and its controls.
    ("{0} pictures", "画像 {0} 枚"),
    ("Delete this session", "このセッションを削除する"),
    ("Stopped\n", "停止しました\n"),
    ("Connecting…", "接続しています…"),
    ("Stopping…", "停止しています…"),
    ("Not sent: what you wrote is back in the box.", "送信しませんでした。入力した内容は入力欄に戻しました。"),
    ("No matches", "見つかりません"),
    ("There is no answer to copy yet.", "コピーできる回答がまだありません。"),
    ("The window is too narrow for the panel: make it wider.", "ウィンドウが狭くて右パネルを表示できません。広げてください。"),
    ("No session in this folder yet.", "このフォルダーのセッションはまだありません。"),
    ("Reload", "再読み込み"),
    ("Open in browser", "ブラウザーで開く"),
    ("Close the preview", "プレビューを閉じる"),
    ("Previous", "前へ"),
    ("Next", "次へ"),
    ("Move up", "上へ移動"),
    ("Move down", "下へ移動"),
    ("Edits to files in this folder", "このフォルダーのファイルの編集"),
    // What is wrong with what was typed.
    ("A connection needs a name.", "接続先には名前が必要です。"),
    ("{0} has no API key yet: paste one in Settings, under Connections. (It would come from the variable {1}, which is not set.)", "{0} の API キーがまだありません。設定の「接続先」で貼り付けてください。(環境変数 {1} から読むようになっていますが、設定されていません。)"),
    ("Windows did not keep the key (Credential Manager, error {0}). A policy on this PC may forbid saved passwords: under More, read it from an environment variable instead.", "Windows がキーを保存しませんでした (資格情報マネージャー、エラー {0})。この PC のポリシーがパスワードの保存を禁止している可能性があります。「詳細」から環境変数で読むようにしてください。"),
    // Settings and the engine.
    ("The best for this PC", "この PC に最適なもの"),
    ("Straydog looks at this machine and chooses", "Straydog がこのマシンを調べて選びます"),
    ("Recommended for this PC  ·  ", "おすすめ  ·  "),
];

pub const HELP_JA: &str = "# はじめに
やってほしいことを書いて Enter で送信します。変更の前にモデルが確認します。
別のセッションを開いても、元のセッションは作業を続けます。

/\tコマンド
@\tこのフォルダーのファイル
Ctrl+V\t画像やファイル

# キー操作
Enter\t送信 (作業中なら次のステップの前に)
Ctrl+Enter\t作業中なら、終わってから送信
Shift+Enter\t改行
Esc\t停止、却下、閉じる
Ctrl+K\tコマンド (どこからでも)
Ctrl+/\tこのキー一覧
Ctrl+N\t新しいセッション
Ctrl+Tab\t次のセッション
Ctrl+1 から 9\tサイドバーのそのセッション
Ctrl+,\t設定
Ctrl+Shift+C\t最後の回答をコピー
Ctrl+B\t右側のパネル
Ctrl+F\tこの会話を検索
Ctrl+Shift+F\t全セッションを検索
F2\tこのセッションに名前を付ける
Y / A / N\t承認、常に許可、却下
N / P\t次と前の変更 (変更の画面で)

# コマンド
{commands}

# 確認のタイミング
計画のみは何も変更しません。毎回確認は、そのたびに確認します。編集を許可は、ファイルの変更を確認なしで行います。自動はコマンドも実行しますが、高リスクのもの、フォルダーの外のもの、ネットワークを使うものは実行しません。
常に許可は、このフォルダーのルールを保存します。上のものには及びません。ルールの一覧は設定にあります。
コマンドが書き込めるのはセッションのフォルダーの中だけで、承認しない限りネットワークは使えません。

# 戻る
/rewind は前のターンの最初に戻ります。ファイル、会話、またはその両方を選べます。戻せるのはエージェント自身の編集だけです。上書きされる内容は先に保存されます。

# その他
貼り付けたキーは Windows の資格情報マネージャーにだけ保存します。
画像: クリップのボタン、/attach、貼り付け、ドロップ。PNG、JPEG、WebP、GIF。
スキル: プロジェクトとホームの .claude/skills、.agents/skills、.codex/skills、.dogmind/skills。/スキル名 で始めます。
MCP サーバー: データフォルダーの mcp.json。アプリと一緒に起動し、呼び出しのたびに確認します。設定 > ツール で返す内容を信頼できます。
Web: 返ってきた内容はデータとして扱い、会話に入ると以後のコマンドはすべて確認します。
カスタム指示 (設定) は全セッション共通、DOGMIND.md はそのプロジェクト用です。/init でモデルに書かせることもできます。
";

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `t("...")` and `tf("...")` written in the code, as the text of its source.
    fn words_in(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        for opening in ["t(\"", "tf(\""] {
            let mut rest = source;
            while let Some(at) = rest.find(opening) {
                let before = rest[..at].chars().last();
                rest = &rest[at + opening.len()..];
                if before.is_some_and(|letter| letter.is_alphanumeric() || letter == '_') {
                    continue;
                }
                let mut escaped = false;
                let end = rest.char_indices().find(|(_, letter)| {
                    let closes = *letter == '"' && !escaped;
                    escaped = *letter == '\\' && !escaped;
                    closes
                });
                if let Some((end, _)) = end {
                    found.push(rest[..end].replace("\\r", "\r").replace("\\n", "\n").replace("\\\"", "\"").replace("\\\\", "\\"));
                }
            }
        }
        found
    }

    #[test]
    fn no_setting_speaks_in_the_program_s_own_terms() {
        for (english, japanese) in JA {
            for word in ["env:NAME", "base URL", "VRAM", "GiB"] {
                assert!(!english.contains(word) && !japanese.contains(word), "{english}");
            }
        }
    }

    #[test]
    fn everything_the_code_asks_for_has_its_japanese() {
        let table: HashMap<&str, &str> = JA.iter().copied().collect();
        let mut missing = Vec::new();
        for source in [include_str!("ui.rs"), include_str!("engine.rs"), include_str!("settings.rs"), include_str!("toolrun.rs"), include_str!("attach.rs"), include_str!("preview.rs")] {
            for words in words_in(source) {
                if !table.contains_key(words.as_str()) {
                    missing.push(words);
                }
            }
        }
        assert!(missing.is_empty(), "no Japanese for {missing:#?}");
    }

    #[test]
    fn what_a_person_reads_when_a_provider_fails_is_all_in_japanese() {
        let table: HashMap<&str, &str> = JA.iter().copied().collect();
        for words in llm::WORDS.into_iter().chain(images::WORDS).chain(dogmind_agent::read_folders::WORDS) {
            assert!(table.contains_key(words), "{words}");
        }
        assert!(table.contains_key(crate::settings::VARIABLE_NAME_ONLY));
        assert!(table.contains_key(crate::settings::NAME_HOLDS_A_KEY) && table.contains_key(crate::settings::ADDRESS_HOLDS_A_KEY));
        assert!(table.contains_key("Connecting…"), "said by a word kept elsewhere");
        for opening in llm::OPENINGS {
            assert!(NOTICES.iter().any(|(english, _)| *english == opening), "{opening}");
        }
        let shown = in_japanese(llm::for_the_engine(llm::SERVER_GONE));
        assert!(shown.contains("再起動") && shown.contains("会話はそのまま残っています"), "{shown}");
        let shown = in_japanese(llm::for_the_engine(llm::SERVER_OUT_OF_MEMORY));
        assert!(shown.contains("メモリ不足") && shown.contains("再起動"), "{shown}");
        assert!(!in_japanese(llm::SERVER_GONE).contains("再起動"), "another machine's server is not this app's to start");
    }

    #[test]
    fn a_detail_keeps_its_words_and_loses_the_system_code() {
        let shown = without_system_code(&in_japanese("the session could not be saved: C:\\a.jsonl: アクセスが拒否されました。 (os error 5)"));
        assert_eq!(shown, "セッションを保存できませんでした: C:\\a.jsonl: アクセスが拒否されました。");
        assert_eq!(without_system_code("a (os error 1) b (os error 2)"), "a b");
        assert_eq!(without_system_code("no such thing (os error"), "no such thing (os error");
    }

    #[test]
    fn a_number_in_the_agents_words_finds_its_place() {
        assert_eq!(in_japanese("stopped after 300 iterations"), "300 回繰り返したところで止めました。もう一度送ると続けられます。");
        assert_eq!(in_japanese("18 tool calls failed in this turn: stopped so that you can look"), "このターンでツール呼び出しが 18 回失敗したため、確認できるように止めました。");
        assert_eq!(in_japanese("Session compacted: old tool outputs shortened by 12 KB"), "セッションを圧縮しました: 古いツール出力を 12 KB 短くしました");
        assert_eq!(in_japanese("Session compacted: every tool output, the newest too, shortened by 30 KB, as nothing else would fit"), "セッションを圧縮しました: ほかに減らせるものがないため、最新のものも含めてツール出力を 30 KB 短くしました");
        assert_eq!(in_japanese("words no table has"), "words no table has");
    }

    #[test]
    fn the_reasons_an_address_asks_are_in_japanese() {
        for reason in ["this address is not from your messages, a search or a page, and an address can carry data out", "web content is in this conversation, and an address can carry data out in its path"] {
            assert_ne!(in_japanese(reason), reason);
        }
    }

    #[test]
    fn a_translation_holds_the_same_values_as_its_english() {
        for (english, japanese) in JA {
            for at in 0..8 {
                let mark = format!("{{{at}}}");
                assert_eq!(english.matches(&mark).count(), japanese.matches(&mark).count(), "{english}");
            }
        }
    }

    #[test]
    fn no_word_is_in_the_table_twice() {
        let mut seen = std::collections::HashSet::new();
        for (english, _) in JA {
            assert!(seen.insert(*english), "{english}");
        }
    }

    #[test]
    fn values_go_where_the_language_wants_them() {
        assert_eq!(fill("{1} / {0}", &[&1, &"b"]), "b / 1");
        assert_eq!(fill("no value", &[]), "no value");
    }

    #[test]
    fn the_system_decides_only_when_asked_to() {
        assert!(resolves_to_japanese("ja", false));
        assert!(!resolves_to_japanese("en", true));
        assert!(resolves_to_japanese("system", true));
        assert!(!resolves_to_japanese("system", false));
    }
}

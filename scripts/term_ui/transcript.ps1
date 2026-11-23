# The conversation as the person sees it, at the screen's own scale (no DOGMIND_TEST_DPI) and with the settings a new profile has: a short sentence, a message with a
# picture, and a long paragraph, in English and in Japanese, a picture after each answer. TERM_UI_SERVER: a model server, as http://host:port/v1. TERM_UI_EXE: the app to
# run (the debug build beside this file where not given). TERM_UI_TAG: what the pictures' names begin with.
. "$PSScriptRoot\lib.ps1"
$ErrorActionPreference = 'Stop'
if (-not $env:TERM_UI_SERVER) { throw 'TERM_UI_SERVER: a model server, as http://host:port/v1' }
$exe = if ($env:TERM_UI_EXE) { $env:TERM_UI_EXE } else { Join-Path $PSScriptRoot '..\..\target\debug\dogmind-app.exe' }
$tag = if ($env:TERM_UI_TAG) { $env:TERM_UI_TAG } else { 'tx' }
$results = @()
function Check([string]$Name, [bool]$Ok, $Detail = '') { $script:results += [pscustomobject]@{ Name = $Name; Ok = $Ok; Detail = "$Detail" }; Write-Host ("{0,-6} {1} {2}" -f $(if ($Ok) { 'ok' } else { 'FAIL' }), $Name, $Detail) }
$asks = @{
  en = @{ short = 'Hello. Answer in one short sentence.'; picture = 'What is in this picture? One sentence.'
          long = 'I have been thinking about how to organise the notes for the project we started last week. There are meeting notes, a list of open questions, the drafts of two letters and a few pictures of the whiteboard, and they are spread over three folders and an email thread. I would like one place where each kind has its own page and the open questions are at the top, so that anyone who joins can read the state of things in a few minutes. Answer in one short sentence: is one notebook with a page per kind a good start?' }
  ja = @{ short = 'こんにちは。短い一文で返してください。'; picture = 'この画像には何が写っていますか。一文で。'
          long = '先週始めたプロジェクトのメモをどうまとめるか考えています。会議のメモ、まだ答えの出ていない質問の一覧、二通の手紙の案、ホワイトボードの写真が何枚かあり、三つのフォルダーとメールのやりとりに分かれています。種類ごとにページがあって、未解決の質問がいちばん上にある場所を一つ作り、途中から加わった人も数分で今の状況が分かるようにしたいです。一文で答えてください。種類ごとに一ページのノートを一冊、というのは良い始め方でしょうか。' }
}
foreach ($display in 'en', 'ja') {
  $theme = if ($display -eq 'en') { 'dark' } else { 'light' }
  $data = New-Data "$tag-$display" -Display $display -Theme $theme -Extra @{ providers = @(@{ name = 'Homelab'; kind = 'server'; base_url = $env:TERM_UI_SERVER }) }
  $work = "$S\work\$tag-$display"
  # (a picture of a window, wider than tall, as a screenshot the person sends is)
  New-Picture "$work\window.png"
  $a = Start-App -Exe $exe -Data $data -Width 2200 -Height 1500 -Env @{ DOGMIND_TEST_TERMINAL = '1' }
  try {
    Start-Sleep -Milliseconds 1200
    Say $a $asks[$display].short
    Check "the short sentence is answered ($display)" (WaitIdle $a 180)
    Shot $a "$tag-1-short-$display" | Out-Null
    Say $a "/attach `"$work\window.png`""; Start-Sleep -Milliseconds 800
    Say $a $asks[$display].picture
    Check "the message with a picture is answered ($display)" (WaitIdle $a 240)
    # (a debug build takes a few seconds to make the small copy of a picture: it is drawn once that has come)
    Start-Sleep -Seconds 3
    Shot $a "$tag-2-picture-$display" | Out-Null
    Say $a $asks[$display].long
    Check "the long paragraph is answered ($display)" (WaitIdle $a 180)
    Shot $a "$tag-3-long-$display" | Out-Null
  } finally { Stop-App $a; if ($a) { [void]$a.Process.WaitForExit(10000) }; Start-Sleep -Seconds 2 }
}
$failed = @($results | ? { -not $_.Ok }).Count
Write-Host "transcript: $($results.Count - $failed) ok, $failed failed"
if ($failed) { exit 1 }

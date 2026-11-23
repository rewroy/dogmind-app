# The owner's 11-22 [91]: a message with a picture sent while the window is narrow, then the window made wide. The person's card is to take the room it has then, and its
# words are not to stand one character to a line. At the screen's own scale, with a new profile's settings. TERM_UI_SERVER, TERM_UI_EXE and TERM_UI_TAG as transcript.ps1 has them.
. "$PSScriptRoot\lib.ps1"
$ErrorActionPreference = 'Stop'
if (-not $env:TERM_UI_SERVER) { throw 'TERM_UI_SERVER: a model server, as http://host:port/v1' }
$exe = if ($env:TERM_UI_EXE) { $env:TERM_UI_EXE } else { Join-Path $PSScriptRoot '..\..\target\debug\dogmind-app.exe' }
$tag = if ($env:TERM_UI_TAG) { $env:TERM_UI_TAG } else { 'narrow' }
$asks = @{ en = 'What is in this picture? One sentence.'; ja = 'この画像には何が写っていますか。一文で。' }
foreach ($display in 'en', 'ja') {
  $theme = if ($display -eq 'en') { 'dark' } else { 'light' }
  $data = New-Data "$tag-n-$display" -Display $display -Theme $theme -Extra @{ providers = @(@{ name = 'Homelab'; kind = 'server'; base_url = $env:TERM_UI_SERVER }) }
  $work = "$S\work\$tag-n-$display"
  New-Picture "$work\window.png"
  # (as narrow as the window goes: the app keeps it no narrower than it can be laid out in)
  $a = Start-App -Exe $exe -Data $data -Width 1000 -Height 1500 -Env @{ DOGMIND_TEST_TERMINAL = '1' }
  try {
    Start-Sleep -Milliseconds 1200
    Say $a "/attach `"$work\window.png`""; Start-Sleep -Milliseconds 800
    Say $a $asks[$display]
    [void](WaitIdle $a 240)
    # (a debug build takes a few seconds to make the small copy of a picture: it is drawn once that has come)
    Start-Sleep -Seconds 3
    Shot $a "$tag-1-sent-narrow-$display" | Out-Null
    [void][UI]::SetWindowPos($a.Window, [IntPtr]::Zero, $script:WinX, 40, 2200, 1500, 0x14)
    Start-Sleep -Milliseconds 1500
    Shot $a "$tag-2-made-wide-$display" | Out-Null
    Start-Sleep -Milliseconds 2000
    Shot $a "$tag-3-wide-later-$display" | Out-Null
    # (and one sent while the window is minimized, which lays the conversation out with no width at all, then the window restored)
    [void][UI]::ShowWindow($a.Window, 6); Start-Sleep -Milliseconds 800
    Say $a "/attach `"$work\window.png`""; Start-Sleep -Milliseconds 800
    Say $a $asks[$display]
    [void](WaitIdle $a 240)
    [void][UI]::ShowWindow($a.Window, 4); Start-Sleep -Milliseconds 1500
    # (a debug build takes a few seconds to make the small copy of a picture: it is drawn once that has come)
    Start-Sleep -Seconds 3
    Shot $a "$tag-4-sent-minimized-$display" | Out-Null
  } finally { Stop-App $a; if ($a) { [void]$a.Process.WaitForExit(10000) }; Start-Sleep -Seconds 2 }
}

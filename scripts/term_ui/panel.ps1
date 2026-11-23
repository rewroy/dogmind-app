# The terminal panel, in English and in Japanese: a new session has no row of views and has the title bar's terminal button, with its tip; after the first message the row
# has the conversation and the timeline; the button opens PowerShell (whatever shell the settings last named) in a panel under the conversation, which stays shown, and
# closes it again. The first message needs a model server: TERM_UI_SERVER, as http://host:port/v1.
. "$PSScriptRoot\lib.ps1"
$ErrorActionPreference = 'Stop'
$env:DOGMIND_TERM_QUIET = '1'
$results = @()
function Check([string]$Name, [bool]$Ok, $Detail = '') { $script:results += [pscustomobject]@{ Name = $Name; Ok = $Ok; Detail = "$Detail" }; Write-Host ("{0,-6} {1} {2}" -f $(if ($Ok) { 'ok' } else { 'FAIL' }), $Name, $Detail) }
function Info($a, $id) { $h = @{}; foreach ($l in (Term-Read $a "info $id" 'info.txt') -split "`r?`n") { if ($l -match '^(\w+)=(.*)$') { $h[$Matches[1]] = $Matches[2] } }; $h }
function Ids($a) { @((Term-Read $a 'list' 'l.txt') -split "`r?`n" | ? { $_ } | % { [int](($_ -split "`t")[0]) }) }
function Wait-Until([scriptblock]$Test, [int]$Seconds = 20) { $t = Get-Date; while (((Get-Date) - $t).TotalSeconds -lt $Seconds) { if (& $Test) { return $true }; Start-Sleep -Milliseconds 100 }; $false }
function Seen($a, [string]$Class) { [bool](Kids $a | ? { [UI]::Cls($_) -eq $Class -and [UI]::IsWindowVisible($_) }) }
if (-not $env:TERM_UI_SERVER) { throw 'TERM_UI_SERVER: a model server for the first message, as http://host:port/v1' }
$tips = @{ en = 'Show or hide the terminal'; ja = 'ターミナルの表示を切り替える' }
$envs = @{ DOGMIND_TEST_DPI = '96'; DOGMIND_TEST_TERMINAL = '1' }

# (TERM_UI_DISPLAYS: the languages to run, as en,ja)
$displays = if ($env:TERM_UI_DISPLAYS) { $env:TERM_UI_DISPLAYS -split ',' } else { 'en', 'ja' }
foreach ($display in $displays) {
  $theme = if ($display -eq 'en') { 'dark' } else { 'light' }
  # (as a file from the Terminal view would have it: that view, and the Command Prompt as the shell last chosen)
  $data = New-Data "panel-$display" -Display $display -Theme $theme -Extra @{ providers = @(@{ name = 'Homelab'; kind = 'server'; base_url = $env:TERM_UI_SERVER }); center_view = 'terminal'; terminal_shell = 'cmd' }
  $a = Start-App -Data $data -Width 1280 -Height 860 -Env $envs
  try {
    Start-Sleep -Milliseconds 900
    $button = Kids $a | ? { [UI]::Cls($_) -eq 'DogButton' -and [UI]::Txt($_) -eq 'terminal' } | Select-Object -First 1
    Check "the title bar has the terminal's button ($display)" ([bool]$button -and [UI]::IsWindowVisible($button))
    $tip = Term-Read $a "tip $([int64]$button)" 'tip.txt'
    Check "its tip is in the window's language ($display)" ($tip -eq $tips[$display]) $tip
    Check "a new session shows its conversation, with no terminal ($display)" (-not (Seen $a 'DogTerm') -and (Ids $a).Count -eq 0)
    Shot $a "panel-1-new-session-$display" | Out-Null

    $ask = if ($display -eq 'en') { 'Hello. Answer in one short sentence.' } else { 'こんにちは。短い一文で返してください。' }
    Say $a $ask
    Check "the first message is answered ($display)" (WaitIdle $a 180)
    Check "and the conversation is shown ($display)" (Seen $a 'DogText')
    Shot $a "panel-2-first-message-$display" | Out-Null

    Press $a 'terminal'
    Check "the button opens a terminal ($display)" (Wait-Until { (Ids $a).Count -eq 1 })
    $id = (Ids $a)[0]
    $info = Info $a $id
    Check "in PowerShell, not the shell the settings named ($display)" ($info.shell -in 'pwsh', 'powershell') $info.shell
    Check "in the panel, with the conversation over it ($display)" ($info.shown -eq 'True' -and (Seen $a 'DogTerm') -and (Seen $a 'DogText'))
    Check "its prompt came ($display)" (Term-Wait $a $id { param($g) $g -match 'PS [A-Z]:\\' })
    Start-Sleep -Milliseconds 400
    Shot $a "panel-3-terminal-open-$display" | Out-Null

    Press $a 'terminal'
    Check "the button closes it, and the terminal goes on ($display)" (Wait-Until { -not (Seen $a 'DogTerm') -and (Ids $a).Count -eq 1 })
    Shot $a "panel-4-terminal-closed-$display" | Out-Null
    Press $a 'terminal'
    Check "and opens it again on the same terminal ($display)" (Wait-Until { (Seen $a 'DogTerm') -and (Ids $a).Count -eq 1 -and (Ids $a)[0] -eq $id })
    Term $a "close $id" | Out-Null
    Check "closing its last terminal closes the panel ($display)" (Wait-Until { -not (Seen $a 'DogTerm') -and (Ids $a).Count -eq 0 })
  } finally {
    # (and gone before the next starts: one started while the last was ending showed no window of its own)
    Stop-App $a; if ($a) { [void]$a.Process.WaitForExit(10000) }; Start-Sleep -Seconds 2
  }
}
$results | Export-Csv "$S\panel-results.csv" -NoTypeInformation -Encoding utf8
$failed = @($results | ? { -not $_.Ok }).Count
Write-Host "panel: $($results.Count - $failed) ok, $failed failed"
if ($failed) { exit 1 }

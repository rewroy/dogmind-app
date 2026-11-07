# The pictures of the Terminal view: the empty page, the shell menu, a PowerShell with colours, Japanese output and an exited shell, in the dark and the light theme.
. "$PSScriptRoot\lib.ps1"
$ErrorActionPreference = 'Stop'
$env:DOGMIND_TERM_QUIET = '1'
$results = @()
function Check([string]$Name, [bool]$Ok, $Detail = '') { $script:results += [pscustomobject]@{ Name = $Name; Ok = $Ok; Detail = "$Detail" }; Write-Host ("{0,-6} {1} {2}" -f $(if ($Ok) { 'ok' } else { 'FAIL' }), $Name, $Detail) }
function Info($a, $id) { $h = @{}; foreach ($l in (Term-Read $a "info $id" 'info.txt') -split "`r?`n") { if ($l -match '^(\w+)=(.*)$') { $h[$Matches[1]] = $Matches[2] } }; $h }
function Alive([int]$ProcId) { [bool](Get-Process -Id $ProcId -EA SilentlyContinue) }
$envs = @{ DOGMIND_TEST_DPI = '96'; DOGMIND_TEST_TERMINAL = '1' }

foreach ($theme in 'dark', 'light') {
  $display = if ($theme -eq 'dark') { 'en' } else { 'ja' }
  $data = New-Data "t-$theme" -Display $display -Theme $theme -Extra @{ right_view = 'terminal' }
  $work = "$S\work\t-$theme"
  Set-Content "$work\alpha.txt" 'a'; Set-Content "$work\beta.txt" 'b'
  $a = Start-App -Data $data -Width 1280 -Height 860 -Env $envs
  Start-Sleep -Milliseconds 900
  Shot $a "term-1-empty-$theme" | Out-Null
  Check "empty page $theme has the panel on the Terminal view" ((Term-Read $a 'list' 'list.txt') -eq '' -or $true)
  # the shell menu on the empty page
  Term $a 'menu shells' | Out-Null; Start-Sleep -Milliseconds 300
  Shot $a "term-2-menu-$theme" | Out-Null
  Term $a 'menu close' | Out-Null
  # a PowerShell with colours
  $shell = 'pwsh'; $id = Term $a 'new pwsh'
  if ($id -le 0) { $shell = 'powershell'; $id = Term $a 'new powershell' }
  Check "a $shell starts ($theme)" ($id -gt 0) $id
  Term $a 'focus on' | Out-Null
  $ok = Term-Wait $a $id { param($g) $g.Trim().Length -gt 10 }
  Check "the $shell prompt came ($theme)" $ok
  $esc = '$e=[char]27; '
  Term $a ('input {0} {1}"$e[31mred$e[32m green$e[33m yellow$e[34m blue$e[35m magenta$e[36m cyan$e[0m  $e[1;31mBOLDRED$e[0m $e[38;5;208morange256$e[0m $e[38;2;80;160;255mtruecolor$e[0m $e[4munderline$e[0m $e[7minverse$e[0m $e[41m redbg $e[0m"\r' -f $id, $esc) | Out-Null
  $ok = Term-Wait $a $id { param($g) $g -match 'truecolor' }
  Check "colours were written ($theme)" $ok
  Term $a ('input {0} "日本語の出力 あいう|"; "abcdef|"\r' -f $id) | Out-Null
  $ok = Term-Wait $a $id { param($g) $g -match 'あいう\|' }
  Check "Japanese was written ($theme)" $ok
  Start-Sleep -Milliseconds 500
  $d0 = Term $a "drawn $id"
  Shot $a "term-3-powershell-$theme" | Out-Null
  $d1 = Term $a "drawn $id"
  Check "the window shows what the screen holds ($theme)" ($d0 -eq 1) "before the shot $d0, after $d1"
  # an exited shell
  $id2 = Term $a 'new cmd'
  Start-Sleep -Milliseconds 1200
  Term $a "input $id2 exit 3\r" | Out-Null
  $t0 = Get-Date; while (((Get-Date) - $t0).TotalSeconds -lt 15 -and (Info $a $id2).over -ne 'true') { Start-Sleep -Milliseconds 100 }
  $i = Info $a $id2
  Check "an exited shell has its code ($theme)" ($i.exit -eq '3' -and $i.over -eq 'true') "exit=$($i.exit)"
  Start-Sleep -Milliseconds 400
  Shot $a "term-4-exited-$theme" | Out-Null
  Stop-App $a
}
# At the scale of this screen, which is not 100% (the others are drawn at 96 dots): the face is tuned to whole pixels at any, and the cells are as many as fit.
$data = New-Data "t-scale" -Display 'en' -Theme 'dark' -Extra @{ right_view = 'terminal' }
$a = Start-App -Data $data -Width 2600 -Height 1400 -Env @{ DOGMIND_TEST_TERMINAL = '1' }
Start-Sleep -Milliseconds 900
$id = Term $a 'new cmd'
Start-Sleep -Milliseconds 1500
Term $a 'focus on' | Out-Null
Term $a "input $id echo plain and あいう.\r" | Out-Null
$ok = Term-Wait $a $id { param($g) $g -match 'plain and あいう\.' }
Check 'output at the screen''s own scale' $ok
$i = Info $a $id
$dpi = [UI]::GetDpiForWindow($a.Window)
Check 'the face is tuned for it' ($i.cell -match '^\d+x\d+$') "dpi $dpi, cell $($i.cell), $($i.cols) by $($i.rows)"
Start-Sleep -Milliseconds 700
Shot $a "term-10-own-scale" | Out-Null
Stop-App $a
$results | ConvertTo-Json | Set-Content "$S\out\results-shots.json"
"{0} checks, {1} failed" -f $results.Count, @($results | ? { -not $_.Ok }).Count




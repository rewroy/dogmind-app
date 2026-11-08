# What the terminals do in the running app: text that lines up, dir in the workspace, a resize, closing a tab, collapsing the panel, renaming, a limit of 8, quitting.
. "$PSScriptRoot\lib.ps1"
$ErrorActionPreference = 'Stop'
$results = @()
function Check([string]$Name, [bool]$Ok, $Detail = '') { $script:results += [pscustomobject]@{ Name = $Name; Ok = $Ok; Detail = "$Detail" }; Write-Host ("{0,-5} {1} {2}" -f $(if ($Ok) { 'ok' } else { 'FAIL' }), $Name, $Detail) }
function Info($a, $id) { $h = @{}; foreach ($l in (Term-Read $a "info $id" 'info.txt') -split "`r?`n") { if ($l -match '^(\w+)=(.*)$') { $h[$Matches[1]] = $Matches[2] } }; $h }
function Alive([int]$ProcId) { [bool](Get-Process -Id $ProcId -EA SilentlyContinue) }
function Wait-Until([scriptblock]$Test, [int]$Seconds = 20) { $t = Get-Date; while (((Get-Date) - $t).TotalSeconds -lt $Seconds) { if (& $Test) { return $true }; Start-Sleep -Milliseconds 100 }; $false }
$envs = @{ DOGMIND_TEST_DPI = '96'; DOGMIND_TEST_TERMINAL = '1' }
$data = New-Data "func" -Display 'en' -Theme 'dark' -Extra @{ right_view = 'terminal' }
$work = "$S\work\func"; Set-Content "$work\alpha.txt" 'a'; Set-Content "$work\beta.txt" 'b'; New-Item -ItemType Directory "$work\sub" -Force | Out-Null
$a = Start-App -Data $data -Width 1280 -Height 860 -Env $envs
Start-Sleep -Milliseconds 900

# 1. plain and Japanese text through cmd /c echo, and that it lines up
$id = Term $a 'new cmd'; Check 'a cmd terminal starts' ($id -gt 0)
Check 'cmd prompt came' (Wait-Until { (Term-Read $a "grid $id" 'g.txt') -match '>' })
Term $a "input $id cmd /c echo plain and あいう.\r" | Out-Null
Check 'plain and Japanese text appears' (Wait-Until { (Term-Read $a "view $id" 'v.txt') -split "`r?`n" -contains 'plain and あいう.' })
$view = (Term-Read $a "view $id" 'v.txt') -split "`r?`n"; $row = [array]::IndexOf($view, 'plain and あいう.')
$cells = (Term-Read $a "cells $id" 'c.txt') -split "`r?`n" | ? { $_ } | % { $p = $_ -split "`t"; [pscustomobject]@{ Row = [int]$p[0]; Col = [int]$p[1]; Ch = $p[2]; W = [int]$p[3] } }
$dot = $cells | ? { $_.Row -eq $row -and $_.Ch -eq '.' }; $ai = $cells | ? { $_.Row -eq $row -and $_.Ch -eq 'あ' }
Check 'Japanese takes two cells each and the dot follows at column 16' ($dot.Col -eq 16 -and $ai.Col -eq 10 -and $ai.W -eq 2) "dot=$($dot.Col) あ=$($ai.Col)/$($ai.W)"
Term $a "input $id cmd /c echo あいう\r" | Out-Null; Term $a "input $id cmd /c echo abcdef\r" | Out-Null
# 2. dir in the workspace
Term $a "input $id dir\r" | Out-Null
Check 'dir lists the workspace' (Wait-Until { $g = Term-Read $a "grid $id" 'g.txt'; $g -match 'alpha\.txt' -and $g -match 'beta\.txt' -and $g -match 'sub' })
$i = Info $a $id; Check 'the shell started in the workspace' ($i.folder -eq $work) $i.folder
# 3. a resize reflows
Term $a "input $id cls\r" | Out-Null; Start-Sleep -Milliseconds 400
$long = ('abcdefghij' * 12)
Term $a "input $id echo $long\r" | Out-Null
Wait-Until { (Term-Read $a "grid $id" 'g.txt') -match 'abcdefghij' } | Out-Null; Start-Sleep -Milliseconds 400
function Long-Rows { (Term-Read $a "grid $id" 'g.txt') -split "`r?`n" | ? { $_ -match '^[a-j]{10,}$' } }
$before = $i.cols; $rowsBefore = @(Long-Rows).Count
[void][UI]::SetWindowPos($a.Window, [IntPtr]::Zero, $script:WinX, 40, 1700, 860, 0x14); Start-Sleep -Milliseconds 800
$i2 = Info $a $id
Check 'a wider window gives the terminal more columns' ([int]$i2.cols -gt [int]$before) "$before -> $($i2.cols)"
Term $a "input $id mode con\r" | Out-Null
Check 'the pseudo console was told the new size' (Wait-Until { ((Term-Read $a "grid $id" 'g.txt') -split "`r?`n" | ? { $_.Trim().EndsWith("$($i2.cols)") }).Count -gt 0 })
$rowsAfter = @(Long-Rows).Count; $maxRow = (Long-Rows | % { $_.Length } | Measure-Object -Maximum).Maximum
Check 'a long line wraps to the new width' ($rowsAfter -lt $rowsBefore -and $maxRow -le [int]$i2.cols) "rows $rowsBefore -> $rowsAfter, longest $maxRow of $($i2.cols)"
[void][UI]::SetWindowPos($a.Window, [IntPtr]::Zero, $script:WinX, 40, 1280, 860, 0x14); Start-Sleep -Milliseconds 600
Check 'and back to fewer' ([int](Info $a $id).cols -eq [int]$before)
# 4. closing the tab ends the process and what it started
$id2 = Term $a 'new cmd'; Start-Sleep -Milliseconds 800
Term $a "input $id2 ping -n 300 127.0.0.1 >nul\r" | Out-Null
Check 'a child is running under the shell' (Wait-Until { @((Info $a $id2).processes -split ',').Count -ge 2 })
$pids = (Info $a $id2).processes -split ',' | % { [int]$_ }
Check 'they are all alive' (@($pids | ? { Alive $_ }).Count -eq $pids.Count) ($pids -join ',')
Term $a "close $id2" | Out-Null
Check 'closing the tab ends the shell and its child' (Wait-Until { @($pids | ? { Alive $_ }).Count -eq 0 }) ($pids -join ',')
Check 'the other terminal is untouched' (Alive ([int](Info $a $id).pid))
# 5. the panel collapsed and opened again keeps the terminal running
$pid1 = [int](Info $a $id).pid
Say $a '/panel'; Start-Sleep -Milliseconds 600
Check 'the terminal is not shown when the panel is collapsed' ((Info $a $id).shown -eq 'false')
Check 'it is still running while collapsed' (Alive $pid1)
Term $a "input $id echo still here\r" | Out-Null
Say $a '/panel'; Start-Sleep -Milliseconds 600
$i3 = Info $a $id
Check 'opened again it is the same shell' ([int]$i3.pid -eq $pid1 -and $i3.shown -eq 'true')
Check 'what it did while hidden is on its screen' (Wait-Until { (Term-Read $a "grid $id" 'g.txt') -match 'still here' })
# 6. rename by two presses on the tab
Term $a 'tab 0' | Out-Null; Term $a 'tab 0' | Out-Null; Start-Sleep -Milliseconds 300
Term $a 'rename Build' | Out-Null
Check 'a tab is renamed' ((Term-Read $a 'list' 'l.txt') -match "`tBuild`t")
# 7. a Japanese keyboard's characters, and a paste, and a selection
Term $a "type $id echo 入力テスト\r" | Out-Null
Check 'characters typed (as an IME result comes) reach the shell' (Wait-Until { (Term-Read $a "grid $id" 'g.txt') -match '入力テスト' })
Term $a "paste $id echo pasted line one\recho pasted line two\r" | Out-Null
Check 'a paste with two lines runs them' (Wait-Until { $g = Term-Read $a "grid $id" 'g.txt'; $g -match 'pasted line one' -and $g -match 'pasted line two' })
$view = (Term-Read $a "view $id" 'v.txt') -split "`r?`n"; $r = ($view | % { $_ } ).IndexOf(($view | ? { $_ -eq 'pasted line one' } | Select -First 1))
Term $a "selection $id $r 0 $r 5" | Out-Null
Check 'a selection reads back' ((Term-Read $a "selected $id" 's.txt').Trim() -eq 'pasted')
# 8. the wheel: back into what scrolled off
Term $a "input $id for /l %i in (1,1,200) do @echo line %i\r" | Out-Null
Check 'two hundred lines were written' (Wait-Until { (Term-Read $a "grid $id" 'g.txt') -match 'line 200' })
Term $a "scroll $id 30" | Out-Null
$i4 = Info $a $id
Check 'the view goes back' ([int]$i4.offset -eq 30) "offset $($i4.offset), history $($i4.history)"
Shot $a 'term-5-scrolled' | Out-Null
Term $a "scroll $id -30" | Out-Null
# 9. a limit of 8
$ids = @(); for ($k = 0; $k -lt 8; $k++) { $n = Term $a 'new cmd'; if ($n -gt 0) { $ids += $n } }
$count = @((Term-Read $a 'list' 'l.txt') -split "`r?`n" | ? { $_ }).Count
Check 'no more than 8 terminals' ($count -eq 8 -and (Term $a 'new cmd') -eq 0) "count=$count"
Shot $a 'term-6-eight-tabs' | Out-Null
# 10. quitting the app ends them all
$all = @(); foreach ($l in (Term-Read $a 'list' 'l.txt') -split "`r?`n" | ? { $_ }) { $tid = [int](($l -split "`t")[0]); $all += (Info $a $tid).processes -split ',' | % { [int]$_ } }
Term $a "input $id ping -n 300 127.0.0.1 >nul\r" | Out-Null; Start-Sleep -Milliseconds 800
$all += (Info $a $id).processes -split ',' | % { [int]$_ }; $all = $all | Sort-Object -Unique
Check 'many shells are running' ($all.Count -ge 9) "$($all.Count) processes"
Say $a '/quit'
Check 'the app ended' ($a.Process.WaitForExit(15000))
Check 'quitting ended every shell and all it started' (Wait-Until { @($all | ? { Alive $_ }).Count -eq 0 }) "$(@($all | ? { Alive $_ }).Count) left"
$results | ConvertTo-Json | Set-Content "$S\out\results-func.json"
"{0} checks, {1} failed" -f $results.Count, @($results | ? { -not $_.Ok }).Count




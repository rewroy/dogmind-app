# The pointer and the buttons, as window messages: the title bar's terminal button, a tab's cross, a drag that selects, a double click that takes a word, a right click and its menu, the wheel.
. "$PSScriptRoot\lib.ps1"
$ErrorActionPreference = 'Stop'
$results = @()
function Check([string]$Name, [bool]$Ok, $Detail = '') { $script:results += [pscustomobject]@{ Name = $Name; Ok = $Ok; Detail = "$Detail" }; Write-Host ("{0,-5} {1} {2}" -f $(if ($Ok) { 'ok' } else { 'FAIL' }), $Name, $Detail) }
function Info($a, $id) { $h = @{}; foreach ($l in (Term-Read $a "info $id" 'info.txt') -split "`r?`n") { if ($l -match '^(\w+)=(.*)$') { $h[$Matches[1]] = $Matches[2] } }; $h }
function Wait-Until([scriptblock]$Test, [int]$Seconds = 20) { $t = Get-Date; while (((Get-Date) - $t).TotalSeconds -lt $Seconds) { if (& $Test) { return $true }; Start-Sleep -Milliseconds 100 }; $false }
function Ids($a) { @((Term-Read $a 'list' 'l.txt') -split "`r?`n" | ? { $_ } | % { [int](($_ -split "`t")[0]) }) }
$data = New-Data "pointer" -Display 'en' -Theme 'dark' -Extra @{ right_view = 'terminal' }
$a = Start-App -Data $data -Width 1280 -Height 860 -Env @{ DOGMIND_TEST_DPI = '96'; DOGMIND_TEST_TERMINAL = '1' }
Start-Sleep -Milliseconds 900
# The title bar's button opens the panel with PowerShell in it; closing its one terminal closes the panel, and the button opens another.
Check 'no terminal yet' ((Ids $a).Count -eq 0)
Press $a 'terminal'
Check 'the button opens a terminal' (Wait-Until { (Ids $a).Count -eq 1 })
$first = (Ids $a)[0]
Check 'in PowerShell' ((Info $a $first).shell -in 'pwsh', 'powershell') (Info $a $first).shell
Term $a "close $first" | Out-Null
Check 'closing it closes the panel' (Wait-Until { (Ids $a).Count -eq 0 -and -not [bool](Kids $a | ? { [UI]::Cls($_) -eq 'DogTerm' -and [UI]::IsWindowVisible($_) }) })
Press $a 'terminal'
Check 'and the button opens another' (Wait-Until { (Ids $a).Count -eq 1 -and (Ids $a)[0] -ne $first })
# (what follows is written for the Command Prompt's echo and for: a terminal in it, by the hook that names a shell, in place of the PowerShell)
Term $a "close $((Ids $a)[0])" | Out-Null
$id = Term $a 'new cmd'
Check 'a Command Prompt for the rest' ($id -gt 0 -and (Wait-Until { (Ids $a).Count -eq 1 })) $id
Check 'it has the keys' (Wait-Until { (Info $a $id).active -eq 'true' })
$hw = ([UI]::Kids($a.Window) | ? { [UI]::Cls($_) -eq 'DogTerm' } | Select-Object -First 1)
Term $a "input $id echo alpha beta あいう gamma\r" | Out-Null
Check 'a line to select' (Wait-Until { ((Term-Read $a "view $id" 'v.txt') -split "`r?`n") -contains 'alpha beta あいう gamma' })
$view = (Term-Read $a "view $id" 'v.txt') -split "`r?`n"; $row = [array]::IndexOf($view, 'alpha beta あいう gamma')
$info = Info $a $id; $win = $info.window -replace '[()]', '' -split ', '
$size = ($info.cell -split 'x'); $cell = @{ w = [int]$size[0]; h = [int]$size[1] }
function Lparam($x, $y) { [IntPtr](($y -shl 16) -bor ($x -band 0xFFFF)) }
function At($col, $row, $dx = 3) { Lparam (6 + $col * $cell.w + $dx) (6 + $row * $cell.h + 4) }
# a drag from the 'b' of beta (column 6) to the end of 'あいう' (column 15): its text
[void][UI]::PostMessageW($hw, 0x0201, [IntPtr]1, (At 6 $row 1)); Start-Sleep -Milliseconds 100
[void][UI]::PostMessageW($hw, 0x0200, [IntPtr]1, (At 12 $row)); Start-Sleep -Milliseconds 100
[void][UI]::PostMessageW($hw, 0x0200, [IntPtr]1, (At 15 $row 6)); Start-Sleep -Milliseconds 100
[void][UI]::PostMessageW($hw, 0x0202, [IntPtr]0, (At 15 $row 6)); Start-Sleep -Milliseconds 300
$sel = (Term-Read $a "selected $id" 's.txt').Trim()
Check 'a drag selects what it passed over' ($sel -eq 'beta あいう') "'$sel'"
Shot $a 'term-8-selection' | Out-Null
# a double click takes a word
[void][UI]::PostMessageW($hw, 0x0201, [IntPtr]1, (At 2 $row)); [void][UI]::PostMessageW($hw, 0x0202, [IntPtr]0, (At 2 $row)); [void][UI]::PostMessageW($hw, 0x0203, [IntPtr]1, (At 2 $row)); Start-Sleep -Milliseconds 100; [void][UI]::PostMessageW($hw, 0x0202, [IntPtr]0, (At 2 $row)); Start-Sleep -Milliseconds 300
Check 'a double click takes the word' ((Term-Read $a "selected $id" 's.txt').Trim() -eq 'alpha') ''
# a right click: the menu (Copy is there with a selection)
[void][UI]::PostMessageW($hw, 0x0205, [IntPtr]0, (At 3 $row)); Start-Sleep -Milliseconds 400
Shot $a 'term-9-context-menu' | Out-Null
Term $a 'menu choose 2' | Out-Null   # Select all
Check 'select all in the menu selects the screen' ((Term-Read $a "selected $id" 's.txt') -match 'alpha beta') ''
Term $a 'menu close' | Out-Null
# the wheel: back, as the lines are written
Term $a "input $id for /l %i in (1,1,120) do @echo n%i\r" | Out-Null
# (and the prompt after them: a line that comes after the notch moves what is shown back by one more)
Check 'lines written' (Wait-Until { (Term-Read $a "grid $id" 'g.txt') -match '(?s)n120.*>\s*$' })
$wheel = [IntPtr]((120 -shl 16) -band 0xFFFFFFFF)   # one notch up
$before = Info $a $id
[void][UI]::PostMessageW($hw, 0x020A, [IntPtr]([int64]120 -shl 16), (Lparam 100 100)); Start-Sleep -Milliseconds 300
$after = Info $a $id
Check 'a notch of the wheel goes back three lines' ([int]$after.offset - [int]$before.offset -eq 3) "offset $($before.offset) to $($after.offset), rows $($before.rows) to $($after.rows), history $($before.history) to $($after.history), selected $($before.selected)"
# a tab's cross, by a press on the button
Term $a 'new cmd' | Out-Null; Start-Sleep -Milliseconds 800
Check 'two tabs' ((Ids $a).Count -eq 2)
Press $a 'x'
Check 'the cross of the tab shown closes it' (Wait-Until { (Ids $a).Count -eq 1 })
Stop-App $a
$results | ConvertTo-Json | Set-Content "$S\out\results-pointer.json"
"{0} checks, {1} failed" -f $results.Count, @($results | ? { -not $_.Ok }).Count






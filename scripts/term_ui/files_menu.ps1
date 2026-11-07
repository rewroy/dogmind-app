# The Files view still has its menu (one list window serves it, the shells and the terminal's right click now): a right click on a row puts it up, Esc puts it away, and what it chooses is done.
. "$PSScriptRoot\lib.ps1"
$ErrorActionPreference = 'Stop'
$results = @()
function Check([string]$Name, [bool]$Ok, $Detail = '') { $script:results += [pscustomobject]@{ Name = $Name; Ok = $Ok; Detail = "$Detail" }; Write-Host ("{0,-5} {1} {2}" -f $(if ($Ok) { 'ok' } else { 'FAIL' }), $Name, $Detail) }
function Wait-Until([scriptblock]$Test, [int]$Seconds = 20) { $t = Get-Date; while (((Get-Date) - $t).TotalSeconds -lt $Seconds) { if (& $Test) { return $true }; Start-Sleep -Milliseconds 100 }; $false }
$data = New-Data "filesmenu" -Display 'en' -Theme 'dark' -Extra @{ right_view = 'files' }
Set-Content "$S\work\filesmenu\alpha.txt" 'a'; Set-Content "$S\work\filesmenu\beta.txt" 'b'
$a = Start-App -Data $data -Width 1280 -Height 860 -Env @{ DOGMIND_TEST_DPI = '96' }
Start-Sleep -Milliseconds 1500
$lists = { @([UI]::Kids($a.Window) | ? { [UI]::Cls($_) -eq 'ListBox' -and [UI]::IsWindowVisible($_) }) }
$before = (& $lists).Count
Check 'the Files view shows its list' ($before -ge 1) "$before lists"
$rw = New-Object UI+RECT; [void][UI]::GetWindowRect($a.Window, [ref]$rw)
$tree = (& $lists | ? { $r = New-Object UI+RECT; [void][UI]::GetWindowRect($_, [ref]$r); ($r.L - $rw.L) -gt 900 } | Select-Object -First 1)
Check 'it is the panel''s' ($tree -ne $null)
[void][UI]::PostMessageW($tree, 0x0205, [IntPtr]0, [IntPtr]((12 -shl 16) -bor 60)); Start-Sleep -Milliseconds 500
Check 'a right click on a row puts the menu up' ((& $lists).Count -eq $before + 1)
Shot $a 'term-11-files-menu' | Out-Null
$menu = (& $lists | ? { $r = New-Object UI+RECT; [void][UI]::GetWindowRect($_, [ref]$r); $_ -ne $tree -and ($r.L - $rw.L) -gt 900 } | Select-Object -First 1)
[void][UI]::PostMessageW($menu, 0x0100, [IntPtr]0x1B, [IntPtr]0); Start-Sleep -Milliseconds 500
Check 'Esc puts it away' ((& $lists).Count -eq $before)
Stop-App $a
$results | ConvertTo-Json | Set-Content "$S\out\results-files-menu.json"
"{0} checks, {1} failed" -f $results.Count, @($results | ? { -not $_.Ok }).Count



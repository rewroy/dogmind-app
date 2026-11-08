# Keys, characters and a paste as window messages, a program that takes the whole screen, ^C, and bracketed paste.
. "$PSScriptRoot\lib.ps1"
$ErrorActionPreference = 'Stop'
$results = @()
function Check([string]$Name, [bool]$Ok, $Detail = '') { $script:results += [pscustomobject]@{ Name = $Name; Ok = $Ok; Detail = "$Detail" }; Write-Host ("{0,-5} {1} {2}" -f $(if ($Ok) { 'ok' } else { 'FAIL' }), $Name, $Detail) }
function Info($a, $id) { $h = @{}; foreach ($l in (Term-Read $a "info $id" 'info.txt') -split "`r?`n") { if ($l -match '^(\w+)=(.*)$') { $h[$Matches[1]] = $Matches[2] } }; $h }
function Wait-Until([scriptblock]$Test, [int]$Seconds = 20) { $t = Get-Date; while (((Get-Date) - $t).TotalSeconds -lt $Seconds) { if (& $Test) { return $true }; Start-Sleep -Milliseconds 100 }; $false }
$data = New-Data "input" -Display 'ja' -Theme 'dark' -Extra @{ right_view = 'terminal' }
$work = "$S\work\input"; Set-Content "$work\alpha.txt" 'a'
$a = Start-App -Data $data -Width 1280 -Height 860 -Env @{ DOGMIND_TEST_DPI = '96'; DOGMIND_TEST_TERMINAL = '1' }
Start-Sleep -Milliseconds 800
$id = Term $a 'new cmd'
Start-Sleep -Milliseconds 1200
$hw = ([UI]::Kids($a.Window) | ? { [UI]::Cls($_) -eq 'DogTerm' } | Select-Object -First 1)
function Type-Text($text) { foreach ($c in $text.ToCharArray()) { [void][UI]::PostMessageW($hw, 0x0102, [IntPtr][int]$c, [IntPtr]0) } }   # WM_CHAR, as a keyboard or an IME makes it
function Press-Vk([int]$vk) { [void][UI]::PostMessageW($hw, 0x0100, [IntPtr]$vk, [IntPtr]0); Start-Sleep -Milliseconds 60 }   # WM_KEYDOWN, through the window's key handling
Type-Text 'echo 日本語入力 ok'
Press-Vk 0x0D
Check 'characters and Enter as messages run a command' (Wait-Until { (Term-Read $a "view $id" 'v.txt') -split "`r?`n" -contains '日本語入力 ok' })
Press-Vk 0x26; Press-Vk 0x0D
Check 'the Up arrow recalls it and Enter runs it again' (Wait-Until { @((Term-Read $a "view $id" 'v.txt') -split "`r?`n" | ? { $_ -eq '日本語入力 ok' }).Count -ge 2 })
[void][UI]::PostMessageW($hw, 0x010D, [IntPtr]0, [IntPtr]0); [void][UI]::PostMessageW($hw, 0x010F, [IntPtr]0, [IntPtr]0)   # an IME begins composing: its window is put at the cursor
Type-Text 'dir alp'; Press-Vk 0x09; Press-Vk 0x0D
Check 'Tab goes to the shell, which completes the name' (Wait-Until { (Term-Read $a "grid $id" 'g.txt') -match 'alpha\.txt' })
# programs that use the whole screen, a progress bar, colours from git and python
Term $a "input $id git -c color.ui=always log --oneline -3 --no-pager\r" | Out-Null
Check 'git writes its log' (Wait-Until { (Term-Read $a "grid $id" 'g.txt') -match 'fatal|[0-9a-f]{7} ' })
Term $a "input $id python -c ""print('\x1b[1;32mgreen bold\x1b[0m done')""\r" | Out-Null
Check 'python writes with colours' (Wait-Until { (Term-Read $a "grid $id" 'g.txt') -match 'green bold done' })
Term $a "input $id ping -n 2 127.0.0.1 >nul\r" | Out-Null
[void](Wait-Until { @((Info $a $id).processes -split ',').Count -ge 2 })
Check 'a program that ends is gone, and the shell is at its prompt' (Wait-Until { @((Info $a $id).processes -split ',').Count -le 1 })
Start-Sleep -Milliseconds 400
# (vim is Git's: on PATH in a Git Bash, and not in every shell this is run from)
$vim = (Get-Command vim -EA SilentlyContinue).Source
if (-not $vim) { $vim = "$env:ProgramFiles\Git\usr\bin\vim.exe" }
Term $a "input $id ""$vim"" -u NONE -N\r" | Out-Null
Check 'vim takes the alternate screen' ((Test-Path $vim) -and (Wait-Until { (Info $a $id).alternate -eq 'true' })) $vim
if ((Info $a $id).alternate -ne 'true') { Write-Host '--- what the screen had:'; (Term-Read $a "view $id" 'v.txt') -split "`r?`n" | ? { $_ } | Select-Object -Last 12 | % { Write-Host "    $_" } }
Shot $a 'term-7-vim' | Out-Null
Term $a "input $id :q\r" | Out-Null
Check 'and gives it back' (Wait-Until { (Info $a $id).alternate -eq 'false' })
# ^C stops a running program (the key's own bytes: the shell's)
Term $a "input $id ping -n 60 127.0.0.1 >nul\r" | Out-Null
Check 'a ping runs' (Wait-Until { @((Info $a $id).processes -split ',').Count -ge 2 })
Term $a "input $id \x03" | Out-Null
Check '^C stops it' (Wait-Until { @((Info $a $id).processes -split ',').Count -le 1 })
# a shell that asks for bracketed paste (bash does), and a paste is then a paste
$p = Term $a 'new git-bash'
Check 'Git Bash starts in the workspace' (Wait-Until { ((Term-Read $a "grid $p" 'g.txt') -replace "`r?`n", '') -match 'work/input' } 25)
Check 'Git Bash asks for bracketed paste' (Wait-Until { (Info $a $p).bracketed -eq 'true' } 25)
Term $a "paste $p echo first
echo second" | Out-Null
Start-Sleep -Milliseconds 800
$g = Term-Read $a "grid $p" 'g.txt'
Check 'a bracketed paste is not run' ($g -match 'echo first' -and -not (($g -split "`r?`n") -contains 'first')) ''
Term $a "input $p " | Out-Null
Term $a "paste $p echo plain" | Out-Null
Term $a "input $p 
" | Out-Null
Check 'and a line that is typed after it runs' (Wait-Until { (($g = Term-Read $a "grid $p" 'g.txt') -split "`r?`n") -contains 'plain' })
Stop-App $a
$results | ConvertTo-Json | Set-Content "$S\out\results-input.json"
"{0} checks, {1} failed" -f $results.Count, @($results | ? { -not $_.Ok }).Count




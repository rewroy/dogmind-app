# What a shell in a terminal takes its console to be, and what a person sees of it: PowerShell's own answers (is its output redirected, does it do virtual terminal, how does it render output), git's colour
# in PowerShell and in Git Bash, a NO_COLOR in the person's environment (kept: the shell prints plain text), and an app that was itself started with its output in files (its shell has the console, not those files).
. "$PSScriptRoot\lib.ps1"
$ErrorActionPreference = 'Stop'
$env:DOGMIND_TERM_QUIET = '1'
$results = @()
function Check([string]$Name, [bool]$Ok, $Detail = '') { $script:results += [pscustomobject]@{ Name = $Name; Ok = $Ok; Detail = "$Detail" }; Write-Host ("{0,-5} {1} {2}" -f $(if ($Ok) { 'ok' } else { 'FAIL' }), $Name, $Detail) }
function Wait-Until([scriptblock]$Test, [int]$Seconds = 20) { $t = Get-Date; while (((Get-Date) - $t).TotalSeconds -lt $Seconds) { if (& $Test) { return $true }; Start-Sleep -Milliseconds 100 }; $false }
function Flat($a, $id) { (Term-Read $a "grid $id" 'g.txt') -replace "`r?`n", '' }
$envs = @{ DOGMIND_TEST_DPI = '96'; DOGMIND_TEST_TERMINAL = '1' }
$data = New-Data 'redirect' -Display 'en' -Theme 'dark' -Extra @{ right_view = 'terminal' }
$work = "$S\work\redirect"
git -C $work init -q; git -C $work -c user.name=t -c user.email=t@t commit -q --allow-empty -m 'first'; Set-Content "$work\a.txt" 'a'; git -C $work add a.txt; git -C $work -c user.name=t -c user.email=t@t commit -q -m 'second'; Set-Content "$work\b.txt" 'b'

# The cells with a colour on the rows of the screen that begin as `Pattern` says: how many.
function Coloured($a, $id, [string]$Pattern) {
  $view = (Term-Read $a "view $id" 'v.txt') -split "`r?`n"
  $rows = @(0..($view.Count - 1) | ? { $view[$_] -match $Pattern })
  # (a line of the hook's table: row, column, the character, its colour, the colour of its ground, bold, underline)
  $cells = (Term-Read $a "colors $id" 'col.txt') -split "`r?`n" | ? { $_ -match '^\d+\t\d+\t' } | % { $f = $_ -split "`t"; [pscustomobject]@{ Row = [int]$f[0]; Fg = $f[3]; Bg = $f[4] } }
  [pscustomobject]@{ Rows = $rows; Cells = @($cells | ? { $rows -contains $_.Row -and ($_.Fg -ne 'Default' -or $_.Bg -ne 'Default') }).Count }
}
# A PowerShell in the app: what it says of its console, in one line, and the words it is said in.
function Powershell-Says($a, [string]$Section) {
  $p = Term $a 'new pwsh'
  Check "PowerShell starts ($Section)" ($p -gt 0)
  Term $a 'focus on' | Out-Null
  [void](Term-Wait $a $p { param($g) $g.Trim().Length -gt 10 })
  Start-Sleep -Milliseconds 1500
  Term $a "input $p 'REDIR=' + [Console]::IsOutputRedirected + ' INREDIR=' + [Console]::IsInputRedirected + ' ERRREDIR=' + [Console]::IsErrorRedirected + ' VT=' + `$Host.UI.SupportsVirtualTerminal + ' RENDER=' + `$PSStyle.OutputRendering + ' NOCOLOR=' + `$env:NO_COLOR + '.'\r" | Out-Null
  [void](Wait-Until { (Flat $a $p) -match 'REDIR=\w+ INREDIR=\w+ ERRREDIR=\w+ VT=\w+ RENDER=\w+ NOCOLOR=\S*\.' })
  $said = [regex]::Matches((Flat $a $p), 'REDIR=\w+ INREDIR=\w+ ERRREDIR=\w+ VT=\w+ RENDER=\w+ NOCOLOR=\S*\.') | Select-Object -Last 1
  Write-Host ("      " + $said.Value)
  [pscustomobject]@{ Id = $p; Said = $said.Value }
}

# 1. An app started as a person starts it, with no NO_COLOR in its environment.
$a = Start-App -Data $data -Width 1280 -Height 860 -Env $envs
Start-Sleep -Milliseconds 900
$ps = Powershell-Says $a 'plain start'; $p = $ps.Id
Check 'its output is not redirected' ($ps.Said -match 'REDIR=False INREDIR=False ERRREDIR=False')
Check 'it supports virtual terminal' ($ps.Said -match 'VT=True')
Check 'it renders output with its colours (not as plain text)' ($ps.Said -notmatch 'RENDER=PlainText' -and $ps.Said -match 'NOCOLOR=\.')
Term $a "input $p git -c color.ui=auto log --oneline --decorate -3\r" | Out-Null
Start-Sleep -Milliseconds 1500
$c = Coloured $a $p '^[0-9a-f]{7} '
Check 'git in PowerShell shows its colours on its log rows (its own check of a terminal)' ($c.Rows.Count -ge 2 -and $c.Cells -ge 7) "rows $($c.Rows -join ','), coloured cells $($c.Cells)"
Term $a "input $p git -c color.ui=always status -s\r" | Out-Null
Start-Sleep -Milliseconds 1500
$c = Coloured $a $p '^\?\? b\.txt'
Check 'git in PowerShell, told to colour, shows its colours' ($c.Rows.Count -ge 1 -and $c.Cells -ge 2) "rows $($c.Rows -join ','), coloured cells $($c.Cells)"
# Git Bash, where there is one
$gb = Term $a 'new git-bash'
if ($gb -gt 0) {
  Term $a 'focus on' | Out-Null
  [void](Term-Wait $a $gb { param($g) $g.Trim().Length -gt 10 })
  Start-Sleep -Milliseconds 1500
  Term $a "input $gb test -t 1 && echo TTY=yes || echo TTY=no\r" | Out-Null
  Check 'Git Bash takes its output for a terminal' (Wait-Until { (Term-Read $a "grid $gb" 'g.txt') -match '(?m)^TTY=yes' })
  Term $a "input $gb git -c color.ui=auto log --oneline --decorate -3\r" | Out-Null
  Start-Sleep -Milliseconds 1500
  $c = Coloured $a $gb '^[0-9a-f]{7} '
  Check 'git in Git Bash shows its colours on its log rows' ($c.Rows.Count -ge 2 -and $c.Cells -ge 7) "rows $($c.Rows -join ','), coloured cells $($c.Cells)"
  Term $a "input $gb git -c color.ui=always status -s\r" | Out-Null
  Start-Sleep -Milliseconds 1500
  $c = Coloured $a $gb '^\?\? b\.txt'
  Check 'git in Git Bash, told to colour, shows its colours' ($c.Rows.Count -ge 1 -and $c.Cells -ge 2) "rows $($c.Rows -join ','), coloured cells $($c.Cells)"
} else { Write-Host '      (no Git Bash here: its two checks are left out)' }
Stop-App $a

# 2. A NO_COLOR in the person's environment is theirs: the shell has it, and prints plain text.
$a = Start-App -Data $data -Width 1280 -Height 860 -Env ($envs + @{ NO_COLOR = '1' })
Start-Sleep -Milliseconds 900
$ps = Powershell-Says $a 'NO_COLOR set'
Check 'a NO_COLOR the person has is passed on, and PowerShell prints plain text for it' ($ps.Said -match 'RENDER=PlainText' -and $ps.Said -match 'NOCOLOR=1\.') $ps.Said
Stop-App $a

# 3. An app that was started with its output in files, as a program started from a pipe is: its shell has the console.
$a = Start-App -Data $data -Width 1280 -Height 860 -Env $envs -Piped
Start-Sleep -Milliseconds 900
$ps = Powershell-Says $a 'piped parent'; $p = $ps.Id
Check 'a shell of a piped app: its output is not redirected' ($ps.Said -match 'REDIR=False INREDIR=False ERRREDIR=False')
Check 'a shell of a piped app: it renders output with its colours' ($ps.Said -notmatch 'RENDER=PlainText')
Term $a "input $p 'PIPED' + 'MARK'\r" | Out-Null
$shown = Wait-Until { (Term-Read $a "grid $p" 'g.txt') -match '(?m)^PIPEDMARK' }
Check 'a shell of a piped app writes to the terminal' $shown
$cmd = Term $a 'new cmd'
Term $a 'focus on' | Out-Null
Start-Sleep -Milliseconds 1500
Term $a "input $cmd echo CMD-PIPED-MARK\r" | Out-Null
$shown = Wait-Until { (Term-Read $a "grid $cmd" 'g.txt') -match '(?m)^CMD-PIPED-MARK' }
Check 'a Command Prompt of a piped app writes to the terminal' $shown
Term $a "input $p git -c color.ui=auto log --oneline --decorate -3\r" | Out-Null
Start-Sleep -Milliseconds 1500
$c = Coloured $a $p '^[0-9a-f]{7} '
Check 'git in a PowerShell of a piped app shows its colours' ($c.Rows.Count -ge 2 -and $c.Cells -ge 7) "rows $($c.Rows -join ','), coloured cells $($c.Cells)"
Stop-App $a
Start-Sleep -Milliseconds 500
$leaked = (Get-Content "$S\out\app-stdout.txt", "$S\out\app-stderr.txt" -Raw -EA SilentlyContinue) -join ''
Check 'nothing the shells wrote went to the files the app was started with' (($leaked -notmatch 'PIPEDMARK') -and ($leaked -notmatch 'CMD-PIPED-MARK') -and ($leaked -notmatch 'REDIR=')) "$($leaked.Length) characters in those files"
$results | ConvertTo-Json | Set-Content "$S\out\results-redirect.json"
"{0} checks, {1} failed" -f $results.Count, @($results | ? { -not $_.Ok }).Count

# What a screen reader is given: the window named Terminal, its text, the line at the cursor, ranges moved by lines, text found, where it is on the screen.
. "$PSScriptRoot\lib.ps1"
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$results = @()
function Check([string]$Name, [bool]$Ok, $Detail = '') { $script:results += [pscustomobject]@{ Name = $Name; Ok = $Ok; Detail = "$Detail" }; Write-Host ("{0,-5} {1} {2}" -f $(if ($Ok) { 'ok' } else { 'FAIL' }), $Name, $Detail) }
$data = New-Data "uia" -Display 'en' -Theme 'dark' -Extra @{ right_view = 'terminal' }
$a = Start-App -Data $data -Width 1280 -Height 860 -Env @{ DOGMIND_TEST_DPI = '96'; DOGMIND_TEST_TERMINAL = '1' }
Start-Sleep -Milliseconds 800
$id = Term $a 'new cmd'
Start-Sleep -Milliseconds 1500
Term $a "input $id echo hello reader あいう.\r" | Out-Null
Start-Sleep -Milliseconds 1200
$hwnd = ([UI]::Kids($a.Window) | ? { [UI]::Cls($_) -eq 'DogTerm' } | Select-Object -First 1)
$el = [System.Windows.Automation.AutomationElement]::FromHandle($hwnd)
Check 'the window is named Terminal' ($el.Current.Name -eq 'Terminal') $el.Current.Name
Check 'it is a document' ($el.Current.ControlType.ProgrammaticName -eq 'ControlType.Document') $el.Current.ControlType.ProgrammaticName
$tp = $el.GetCurrentPattern([System.Windows.Automation.TextPattern]::Pattern)
$all = $tp.DocumentRange.GetText(-1)
Check 'its text can be read' ($all -match 'hello reader あいう\.') ($all.Substring(0, [Math]::Min(80, $all.Length)) -replace "`r?`n", ' | ')
$view = (Term-Read $a "view $id" 'v.txt').TrimEnd() -replace "`r", ''
Check 'it is the lines in view, as the screen has them' (($all -replace "`r", '').TrimEnd() -eq $view.TrimEnd()) ''
$sel = $tp.GetSelection()
Check 'the cursor is the selection' ($sel.Count -eq 1)
$caret = $sel[0]; $caret.ExpandToEnclosingUnit([System.Windows.Automation.Text.TextUnit]::Line)
$line = $caret.GetText(-1)
Check 'the line at the cursor is readable' ($line -match '>$' -or $line -match '>') $line
# a word, a line before, and a range moved by lines
$doc = $tp.DocumentRange; $first = $doc.Clone(); $first.ExpandToEnclosingUnit([System.Windows.Automation.Text.TextUnit]::Line)
Check 'the first line' ($first.GetText(-1) -match 'Microsoft') $first.GetText(-1)
[void]$first.Move([System.Windows.Automation.Text.TextUnit]::Line, 1)
Check 'a move by a line goes to the next' ($first.GetText(-1) -match 'Corporation') $first.GetText(-1)
$found = $doc.FindText('reader', $false, $true)
Check 'text can be found' ($found -ne $null -and $found.GetText(-1) -eq 'reader')
$rects = $caret.GetBoundingRectangles()
Check 'a range says where it is on the screen' ($rects.Count -ge 1 -and $rects[0].Width -gt 0) "$($rects[0])"
# the Japanese line
$jp = $doc.FindText('あいう', $false, $true)
$jl = $jp.Clone(); $jl.ExpandToEnclosingUnit([System.Windows.Automation.Text.TextUnit]::Line)
Check 'Japanese is read whole' ($jl.GetText(-1) -match 'あいう') $jl.GetText(-1)
Stop-App $a
$results | ConvertTo-Json | Set-Content "$S\out\results-uia.json"
"{0} checks, {1} failed" -f $results.Count, @($results | ? { -not $_.Ok }).Count



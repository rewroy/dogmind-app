# The driver of the running app for the Terminal view's tests: windows by messages, a picture through PrintWindow, a data folder of its own for each run,
# the window shown off every screen and never given the focus, and the hooks of a debug build that has DOGMIND_TEST_TERMINAL set (see ui/term_panel/hooks.rs).
# Dot-source it. Build first with `cargo build -p dogmind-app`; what is written (pictures, data folders, what a hook read) goes to $env:TERM_UI_OUT, or to `out` beside this file.
# Drives the app with messages only, off screen from the first moment. Dot-source it.
# The window is started hidden (STARTF_USESHOWWINDOW), made a tool window so it has no taskbar button, moved to -20000, and only then shown without activation.
$script:S = if ($env:TERM_UI_OUT) { $env:TERM_UI_OUT } else { Join-Path $PSScriptRoot 'out' }; New-Item -ItemType Directory -Force $script:S | Out-Null
$script:Shots = "$S\shots"; New-Item -ItemType Directory -Force $Shots | Out-Null
Add-Type -AssemblyName System.Drawing
if (-not ('UI' -as [type])) {
Add-Type @"
using System; using System.Text; using System.Collections.Generic; using System.Runtime.InteropServices; using System.Diagnostics;
public static class UI {
  public delegate bool EnumP(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumP e, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr p, EnumP e, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder b, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder b, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr SendMessageW(IntPtr h, uint m, IntPtr w, string l);
  [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int w, int ht, uint f);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f);
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] public static extern long GetWindowLongPtrW(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern long SetWindowLongPtrW(IntPtr h, int i, long v);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);  [DllImport("user32.dll")] public static extern IntPtr GetParent(IntPtr h);
  [DllImport("shell32.dll", CharSet=CharSet.Unicode)] public static extern bool Shell_NotifyIconW(uint m, ref ND d);
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] public struct ND { public uint cb; public IntPtr h; public uint id; public uint flags; public uint cbm; public IntPtr icon; [MarshalAs(UnmanagedType.ByValTStr, SizeConst=128)] public string tip; public uint state, mask; [MarshalAs(UnmanagedType.ByValTStr, SizeConst=256)] public string info; public uint ver; [MarshalAs(UnmanagedType.ByValTStr, SizeConst=64)] public string title; public uint infoFlags; public Guid guid; public IntPtr balloon; }
  public static void NoTray(IntPtr h) { var d = new ND(); d.cb = (uint)Marshal.SizeOf(typeof(ND)); d.h = h; d.id = 1; Shell_NotifyIconW(2, ref d); }
  public struct RECT { public int L, T, R, B; }
  public struct POINT { public int X, Y; }
  public static IntPtr FindTop(int pid, string cls) { IntPtr found = IntPtr.Zero; EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && Cls(h) == cls) { found = h; return false; } return true; }, IntPtr.Zero); return found; }
  public static List<IntPtr> Kids(IntPtr p) { var k = new List<IntPtr>(); EnumChildWindows(p, (h, l) => { k.Add(h); return true; }, IntPtr.Zero); return k; }
  public static string Cls(IntPtr h) { var b = new StringBuilder(64); GetClassNameW(h, b, 64); return b.ToString(); }
  public static string Txt(IntPtr h) { var b = new StringBuilder(512); GetWindowTextW(h, b, 512); return b.ToString(); }
}
"@
}
[void][UI]::SetThreadDpiAwarenessContext([IntPtr](-4))

function Start-App {
  param([string]$Exe = (Join-Path $PSScriptRoot '..\..\target\debug\dogmind-app.exe'), [string]$Data, [int]$Width = 1280, [int]$Height = 860, [hashtable]$Env = @{}, [string[]]$AppArgs = @())
  $saved = @{}
  foreach ($k in @{ DOGMIND_DATA = $Data; DOGMIND_TEST_KEYS = '1'; DOGMIND_TEST_IGNORE_ENGINES = '1' }.GetEnumerator()) { $saved[$k.Key] = [Environment]::GetEnvironmentVariable($k.Key); [Environment]::SetEnvironmentVariable($k.Key, $k.Value) }
  foreach ($k in $Env.GetEnumerator()) { $saved[$k.Key] = [Environment]::GetEnvironmentVariable($k.Key); [Environment]::SetEnvironmentVariable($k.Key, $k.Value) }
  $p = if ($AppArgs.Count) { Start-Process $Exe -ArgumentList ($AppArgs | % { '"' + $_ + '"' }) -WindowStyle Hidden -PassThru } else { Start-Process $Exe -WindowStyle Hidden -PassThru }
  foreach ($k in $saved.GetEnumerator()) { [Environment]::SetEnvironmentVariable($k.Key, $k.Value) }
  $h = [IntPtr]::Zero; $t = Get-Date
  while ($h -eq [IntPtr]::Zero -and ((Get-Date) - $t).TotalSeconds -lt 20) { $h = [UI]::FindTop($p.Id, 'DogmindApp'); if ($h -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 20 } }
  if ($h -eq [IntPtr]::Zero) { Stop-Process -Id $p.Id -Force -Confirm:$false -EA SilentlyContinue; throw 'no window' }
  # Before anything shows: no taskbar button, no place on a screen, no tray icon.
  [void][UI]::ShowWindow($h, 0)
  $ex = [UI]::GetWindowLongPtrW($h, -20); [void][UI]::SetWindowLongPtrW($h, -20, $ex -bor 0x80 -band (-bnot 0x40000))
  # On a desktop of its own (uidesk.ps1) it stands at an ordinary place, and is asked to paint; otherwise off every screen.
  $script:WinX = if ($env:UI_ON_DESK) { 0 } else { -20000 }
  [void][UI]::SetWindowPos($h, [IntPtr]::Zero, $script:WinX, 40, $Width, $Height, 0x14)
  [void][UI]::ShowWindow($h, 4)
  [UI]::NoTray($h)
  Start-Sleep -Milliseconds 800
  [UI]::NoTray($h)
  [pscustomobject]@{ Process = $p; Window = $h; Rich = @([UI]::Kids($h) | ? { [UI]::Cls($_) -in 'RICHEDIT50W', 'DogText' }) }
}
function Stop-App($a) { if ($a -and $a.Process -and -not $a.Process.HasExited) { Stop-Process -Id $a.Process.Id -Force -Confirm:$false -EA SilentlyContinue } }
function Shot($a, [string]$Name, [int]$Flag = 3) {
  # (the app draws first: the system keeps what a window last drew, and a picture taken in the moment after a change can be of the one before)
  if ($env:DOGMIND_TEST_TERMINAL) { [void][Hook]::Send($a.Window, 'repaint'); Start-Sleep -Milliseconds 250 }
  $r = New-Object UI+RECT; if ($Flag -band 1) { [void][UI]::GetClientRect($a.Window, [ref]$r) } else { [void][UI]::GetWindowRect($a.Window, [ref]$r) }
  $b = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T); $g = [System.Drawing.Graphics]::FromImage($b); $d = $g.GetHdc(); [void][UI]::PrintWindow($a.Window, $d, $Flag); $g.ReleaseHdc($d)
  # The terminal's window is asked for its own picture (it draws itself on the device it is given), and put where it stands: of what it holds now, and not of what the system last composed of it.
  $term = [UI]::Kids($a.Window) | ? { [UI]::Cls($_) -eq 'DogTerm' -and [UI]::IsWindowVisible($_) } | Select-Object -First 1
  if ($term) {
    $own = New-Object UI+RECT; [void][UI]::GetClientRect($term, [ref]$own)
    $corner = New-Object UI+POINT; [void][UI]::ClientToScreen($term, [ref]$corner)
    $origin = New-Object UI+POINT; [void][UI]::ClientToScreen($a.Window, [ref]$origin)
    $piece = New-Object System.Drawing.Bitmap $own.R, $own.B
    $pg = [System.Drawing.Graphics]::FromImage($piece); $pd = $pg.GetHdc(); [void][UI]::PrintWindow($term, $pd, 1); $pg.ReleaseHdc($pd); $pg.Dispose()
    $g.DrawImage($piece, ($corner.X - $origin.X), ($corner.Y - $origin.Y)); $piece.Dispose()
  }
  $g.Dispose()
  $file = "$Shots\$Name.png"; $b.Save($file, [System.Drawing.Imaging.ImageFormat]::Png); $b.Dispose(); $file
}
function Kids($a) { @([UI]::Kids($a.Window)) }
function Rich($a) { $a.Rich }
function Buttons($a) { Kids $a | ? { [UI]::Cls($_) -eq 'DogButton' -and [UI]::IsWindowVisible($_) } | % { [UI]::Txt($_) } }
function Say($a, [string]$Text) { $c = (Rich $a)[1]; [void][UI]::SendMessageW($c, 0x000C, [IntPtr]::Zero, $Text); Start-Sleep -Milliseconds 150; [void][UI]::PostMessageW($c, 0x0100, [IntPtr]0x0D, [IntPtr]0) }
function Press($a, [string]$Match) {
  $b = Kids $a | ? { [UI]::Cls($_) -eq 'DogButton' -and [UI]::IsWindowVisible($_) -and [UI]::Txt($_) -like $Match } | Select -First 1
  if (-not $b) { throw "no button $Match among: $((Buttons $a) -join ' / ')" }
  [void][UI]::PostMessageW($b, 0x0201, [IntPtr]1, [IntPtr]0x00050005); [void][UI]::PostMessageW($b, 0x0202, [IntPtr]0, [IntPtr]0x00050005)
}
function Key($a, [int]$Vk) { [void][UI]::PostMessageW($a.Window, 0x0100, [IntPtr]$Vk, [IntPtr]0) }
function WaitIdle($a, [int]$Seconds = 60) { $t = Get-Date; $busy = $false; while (((Get-Date) - $t).TotalSeconds -lt $Seconds) { $b = @(Buttons $a); if ($b -contains 'square') { $busy = $true }; if ($busy -and $b -contains 'arrow-up') { Start-Sleep -Milliseconds 700; return $true }; Start-Sleep -Milliseconds 150 }; $false }
function New-Data([string]$Name, [string]$Display = 'ja', [string]$Theme = 'dark', [switch]$Fresh, [hashtable]$Extra = @{}, [int]$Port = 18490) {
  $data = "$S\data\$Name"; Remove-Item $data -Recurse -Force -EA SilentlyContinue; $work = "$S\work\$Name"; Remove-Item $work -Recurse -Force -EA SilentlyContinue; New-Item -ItemType Directory -Force $data, $work | Out-Null
  if (-not $Fresh) {
    $settings = @{ providers = @(@{ name = 'Homelab'; kind = 'server'; base_url = "http://127.0.0.1:$Port/v1" }); active = 'Homelab'; workspace = $work; workspaces = @($work); set_up = $true; show_thinking = $false; language = 'auto'; display_language = $Display; theme = $Theme; tray_told = $true; web_tools = $false; mode = 'auto' }
    foreach ($k in $Extra.GetEnumerator()) { $settings[$k.Key] = $k.Value }
    $settings | ConvertTo-Json -Depth 6 | Set-Content "$data\settings.json" -Encoding utf8
  }
  $data
}
if (-not ('Hook' -as [type])) {
Add-Type @"
using System; using System.Runtime.InteropServices; using System.Text;
public static class Hook {
  [StructLayout(LayoutKind.Sequential)] public struct CDS { public IntPtr dwData; public int cbData; public IntPtr lpData; }
  [DllImport("user32.dll")] static extern IntPtr SendMessageW(IntPtr h, uint m, IntPtr w, ref CDS c);
  public static long Send(IntPtr h, string cmd) {
    byte[] b = Encoding.UTF8.GetBytes(cmd); IntPtr p = Marshal.AllocHGlobal(b.Length + 1); Marshal.Copy(b, 0, p, b.Length);
    CDS c = new CDS(); c.dwData = (IntPtr)0x5445524D; c.cbData = b.Length; c.lpData = p;
    IntPtr r = SendMessageW(h, 0x4A, IntPtr.Zero, ref c); Marshal.FreeHGlobal(p); return (long)r;
  }
}
"@
}
function Term($a, [string]$Cmd) { [Hook]::Send($a.Window, $Cmd) }
function Term-Read($a, [string]$Cmd, [string]$Name) {
  $f = "$S\out\$Name"; New-Item -ItemType Directory -Force "$S\out" | Out-Null; Remove-Item $f -EA SilentlyContinue
  [void](Term $a "$Cmd $f"); if (Test-Path $f) { Get-Content $f -Raw -Encoding utf8 } else { $null }
}
function Term-Wait($a, [int]$Id, [scriptblock]$Test, [int]$Seconds = 20) {
  $t = Get-Date
  while (((Get-Date) - $t).TotalSeconds -lt $Seconds) { $g = Term-Read $a "grid $Id" "grid.txt"; if ($g -ne $null -and (& $Test $g)) { return $true }; Start-Sleep -Milliseconds 100 }
  $false
}






# What a hook says of a terminal, as a table of its words.
function Info($a, $id) { $h = @{}; foreach ($l in (Term-Read $a "info $id" 'info.txt') -split "`r?`n") { if ($l -match '^(\w+)=(.*)$') { $h[$Matches[1]] = $Matches[2] } }; $h }
# How many pixels of a row of cells in a picture of the app have a colour (not a grey): the row, the picture, the terminal's place in the window and the size of a cell.
function Colourful([string]$File, [int]$Row, $Info) {
  $win = ($Info.window -replace '[()]', '') -split ', '; $cell = $Info.cell -split 'x'
  $src = [System.Drawing.Image]::FromFile($File)
  $crop = [System.Drawing.Rectangle]::new([int]$win[0] + 6, [int]$win[1] + 6 + $Row * [int]$cell[1], [int]$win[2] - 12, [int]$cell[1])
  $bmp = New-Object System.Drawing.Bitmap $crop.Width, $crop.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp); $g.DrawImage($src, (New-Object System.Drawing.Rectangle 0, 0, $bmp.Width, $bmp.Height), $crop, 'Pixel'); $g.Dispose()
  $count = 0; for ($y = 0; $y -lt $bmp.Height; $y++) { for ($x = 0; $x -lt $bmp.Width; $x++) { $p = $bmp.GetPixel($x, $y); if ([Math]::Abs($p.R - $p.G) -gt 60 -or [Math]::Abs($p.G - $p.B) -gt 60) { $count++ } } }
  $bmp.Dispose(); $src.Dispose(); $count
}


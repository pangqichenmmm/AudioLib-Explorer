# tools/validate-window-repaint.ps1
# Native Windows exposure regression; only the launched isolated test window is captured.
param(
    [Parameter(Mandatory=$true)][string]$BinaryDirectory,
    [ValidateSet('default','studio')][string]$Theme = 'default'
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'console-utf8.ps1')
. (Join-Path $PSScriptRoot 'native-desktop-window.ps1')
try {
$taskRoot = Split-Path -Parent $PSScriptRoot
$binaryRoot = (Resolve-Path -LiteralPath $BinaryDirectory).Path
$desktop = Join-Path $binaryRoot 'AudioLibExplorer.exe'
$cli = Join-Path $binaryRoot 'AudioLibCli.exe'
if (!(Test-Path -LiteralPath $desktop) -or !(Test-Path -LiteralPath $cli)) { throw 'Both application binaries are required.' }
$tempParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\','/')
$testRoot = Join-Path $tempParent ('audiolib-window-repaint-' + [Guid]::NewGuid().ToString('N'))
$artifacts = Join-Path $taskRoot '.rust-validation/window-repaint'
$artifacts = Join-Path $artifacts ($Theme + '-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $testRoot,$artifacts | Out-Null
Add-Type -AssemblyName System.Windows.Forms,System.Drawing
if (!('AudioLibRepaintNative' -as [type])) {
    $nativeReferences = if ($PSVersionTable.PSEdition -eq 'Core') {
        @('System.Windows.Forms','System.Drawing.Common','System.Drawing.Primitives','System.ComponentModel.Primitives','System.Private.Windows.GdiPlus','System.Private.Windows.Core')
    } else { @('System.Windows.Forms','System.Drawing') }
    Add-Type -ReferencedAssemblies $nativeReferences -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Drawing;
using System.Windows.Forms;
public sealed class AudioLibRepaintCover : Form {
    public bool ActivateCover;
    protected override bool ShowWithoutActivation { get { return !ActivateCover; } }
    protected override CreateParams CreateParams {
        get {
            var p = base.CreateParams;
            // Establish topmost/toolwindow identity when the HWND is created;
            // ShowWithoutActivation must not depend on a later activation.
            p.ExStyle |= 0x00000008 | 0x00000080;
            if (!ActivateCover) p.ExStyle |= 0x08000000;
            return p;
        }
    }
}
public static class AudioLibRepaintNative {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int command);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll", SetLastError=true)] private static extern bool AttachThreadInput(uint first, uint second, bool attach);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT point);
    [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint flags);
    [DllImport("user32.dll", EntryPoint="GetWindowLongPtrW")] private static extern IntPtr GetWindowLongPtr(IntPtr h, int index);
    [DllImport("user32.dll")] private static extern IntPtr GetWindow(IntPtr h, uint command);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int z, uint flags);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint message, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
    [DllImport("user32.dll", SetLastError=true)] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
    [DllImport("user32.dll")] public static extern IntPtr GetThreadDpiAwarenessContext();
    [DllImport("user32.dll")] public static extern bool AreDpiAwarenessContextsEqual(IntPtr first, IntPtr second);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll", SetLastError=true)] public static extern IntPtr GetDC(IntPtr h);
    [DllImport("user32.dll")] public static extern int ReleaseDC(IntPtr h, IntPtr dc);
    [DllImport("gdi32.dll", SetLastError=true)] public static extern bool BitBlt(IntPtr destination, int x, int y, int width, int height, IntPtr source, int sourceX, int sourceY, uint operation);
    [DllImport("user32.dll", SetLastError=true)] public static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
    [DllImport("kernel32.dll")] private static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] private static extern IntPtr GetThreadDesktop(uint thread);
    [DllImport("user32.dll", EntryPoint="GetUserObjectInformationW", SetLastError=true)]
    private static extern bool GetUserObjectInformation(IntPtr handle, int index, out int value, uint length, out uint needed);
    [DllImport("user32.dll")] public static extern bool CloseDesktop(IntPtr desktop);
    public static bool IsOwnedWindow(IntPtr window, uint expectedProcess) {
        uint owner;
        return IsWindow(window) && GetWindowThreadProcessId(window,out owner)!=0 && owner==expectedProcess;
    }
    public static void AssertOwnedWindow(IntPtr window, uint expectedProcess) {
        if (!IsOwnedWindow(window,expectedProcess)) throw new Exception("Target is not an owned test window");
    }
    public static bool FocusOwnedWindow(IntPtr window, uint expectedProcess) {
        AssertOwnedWindow(window,expectedProcess);
        return SetForegroundWindow(window);
    }
    public static bool FocusFromOwnedWindow(IntPtr window, uint expectedProcess, IntPtr from, uint fromProcess) {
        AssertOwnedWindow(window,expectedProcess);
        AssertOwnedWindow(from,fromProcess);
        uint owner;
        uint fromThread=GetWindowThreadProcessId(from,out owner), current=GetCurrentThreadId();
        if (current==fromThread) return SetForegroundWindow(window);
        // Couple only our two isolated test queues while requesting the native
        // transition. Never attach to a user's unrelated foreground window.
        if (!AttachThreadInput(current,fromThread,true)) return false;
        try { return SetForegroundWindow(window); }
        finally {
            if (!AttachThreadInput(current,fromThread,false)) throw new Exception("Could not detach owned test input queues");
        }
    }
    public static bool CloseOwnedWindow(IntPtr window, uint expectedProcess) {
        if (!IsOwnedWindow(window,expectedProcess)) return false;
        return PostMessage(window,0x0010,IntPtr.Zero,IntPtr.Zero);
    }
    public static string CaptureEnvironment() {
        IntPtr desktop = OpenInputDesktop(0,false,1);
        if (desktop==IntPtr.Zero) return "Input-desktop READOBJECTS unavailable; Win32="+Marshal.GetLastWin32Error();
        CloseDesktop(desktop);
        return "Input-desktop READOBJECTS available";
    }
    public static bool InputDesktopAvailable() {
        IntPtr desktop = OpenInputDesktop(0,false,1);
        if (desktop==IntPtr.Zero) return false;
        CloseDesktop(desktop);
        // OpenInputDesktop alone can succeed in a disconnected session.
        // UOI_IO must confirm that our actual desktop is receiving input.
        int receivingInput; uint needed;
        return GetUserObjectInformation(GetThreadDesktop(GetCurrentThreadId()),6,out receivingInput,4,out needed) && receivingInput!=0;
    }
    public static Rectangle Client(IntPtr h) {
        RECT r; if (!GetClientRect(h, out r)) throw new Exception("GetClientRect failed");
        POINT p = new POINT(); if (!ClientToScreen(h, ref p)) throw new Exception("ClientToScreen failed");
        return new Rectangle(p.X, p.Y, r.Right-r.Left, r.Bottom-r.Top);
    }
    public static string CoverageFailure(IntPtr app, IntPtr cover) {
        if (!IsWindowVisible(cover)) return "cover-hidden";
        var a = Client(app); var c = Client(cover);
        if (!c.Contains(a)) return "cover-client-does-not-contain-app: app="+a+"; cover="+c;
        var points = new [] {
            new POINT { X=a.Left+1, Y=a.Top+1 },
            new POINT { X=a.Right-2, Y=a.Top+1 },
            new POINT { X=a.Left+1, Y=a.Bottom-2 },
            new POINT { X=a.Right-2, Y=a.Bottom-2 },
            new POINT { X=a.Left+a.Width/2, Y=a.Top+a.Height/2 }
        };
        var failures = "";
        foreach (var p in points) {
            var top = GetAncestor(WindowFromPoint(p),2);
            if (top!=cover) {
                uint owner;
                GetWindowThreadProcessId(top,out owner);
                failures += "uncovered-point="+p.X+","+p.Y+"; top-HWND="+top+"; top-PID="+owner+"; ";
            }
        }
        if (failures.Length==0) return null;
        failures += "app-exstyle="+GetWindowLongPtr(app,-20)+"; cover-HWND="+cover+"; cover-exstyle="+GetWindowLongPtr(cover,-20)+"; preceding=";
        var preceding = GetWindow(cover,3);
        for (int i=0;i<8 && preceding!=IntPtr.Zero;i++,preceding=GetWindow(preceding,3)) failures += preceding+",";
        return failures;
    }
    public static string WindowState(IntPtr window) {
        return "HWND="+window+"; exstyle="+GetWindowLongPtr(window,-20)+"; owner="+GetWindow(window,4)+"; preceding="+GetWindow(window,3);
    }
    public static Bitmap Capture(IntPtr h) {
        if (!IsWindow(h)) throw new Exception("Capture HWND is no longer valid: " + h);
        var r = Client(h);
        if (r.Width<=0 || r.Height<=0) throw new Exception("Capture client has no area: " + r);
        IntPtr source = GetDC(h);
        if (source==IntPtr.Zero) throw new Exception("GetDC(owned HWND) failed: " + Marshal.GetLastWin32Error());
        Bitmap bitmap = null;
        try {
            bitmap = new Bitmap(r.Width, r.Height);
            using (var graphics = Graphics.FromImage(bitmap)) {
                IntPtr destination = graphics.GetHdc();
                try {
                    // Read only the already presented owned client pixels. No
                    // PrintWindow/WM_PRINT or redraw may repair the frame first.
                    if (!BitBlt(destination,0,0,r.Width,r.Height,source,0,0,0x00CC0020))
                        throw new Exception("BitBlt SRCCOPY failed: HWND="+h+", client="+r+", Win32="+Marshal.GetLastWin32Error());
                } finally { graphics.ReleaseHdc(destination); }
            }
            return bitmap;
        } catch (Exception error) {
            if (bitmap!=null) bitmap.Dispose();
            throw new Exception("Owned client capture failed: HWND="+h+", valid="+IsWindow(h)+", client="+r+", sourceDC="+source,error);
        } finally { ReleaseDC(h,source); }
    }
    public static double Difference(Bitmap before, Bitmap after) {
        if (before.Size != after.Size) throw new Exception("Client size changed");
        long changed = 0, total = 0;
        // Sample every fourth pixel across the entire client, not a local button.
        for (int y=0; y<before.Height; y+=4) for (int x=0; x<before.Width; x+=4) {
            Color a=before.GetPixel(x,y), b=after.GetPixel(x,y); total++;
            if (Math.Abs(a.R-b.R)+Math.Abs(a.G-b.G)+Math.Abs(a.B-b.B)>24) changed++;
        }
        return (double)changed/total;
    }
    public static bool HasContent(Bitmap bitmap) {
        int dark=765, bright=0, lightPixels=0;
        for (int y=0; y<bitmap.Height; y+=4) for (int x=0; x<bitmap.Width; x+=4) {
            Color p=bitmap.GetPixel(x,y); int level=p.R+p.G+p.B;
            dark=Math.Min(dark,level); bright=Math.Max(bright,level);
            if (level>384) lightPixels++;
        }
        return bright-dark>180 && lightPixels>100;
    }
}
'@
}
$savedData = $env:AUDIOLIB_DATA_DIR
$savedBackend = $env:SLINT_BACKEND
$savedLog = $env:RUST_LOG
$savedUserName = $env:USERNAME
$process = $null
$before = $null
$handle = [IntPtr]::Zero
$phase = 'initialize'
$cases = @()
$savedDpiContext = [IntPtr]::Zero
function Assert-CurrentApplicationWindow {
    [AudioLibDesktopWindowSelector]::AssertApplicationWindow($handle,$process.Id)
}
function Get-CanonicalSettingsPath([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path)) { return $null }
    if ($Path.StartsWith('\\?\UNC\',[StringComparison]::OrdinalIgnoreCase)) { $Path = '\\' + $Path.Substring(8) }
    elseif ($Path.StartsWith('\\?\',[StringComparison]::OrdinalIgnoreCase)) { $Path = $Path.Substring(4) }
    return [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $Path).ProviderPath)
}
try {
    # Window/client DC coordinates and cover bounds must all use physical pixels.
    # PowerShell does not guarantee a DPI-aware thread; keep this local to the
    # validation thread and restore its previous context even after a failure.
    $perMonitorV2 = [IntPtr](-4)
    $savedDpiContext = [AudioLibRepaintNative]::SetThreadDpiAwarenessContext($perMonitorV2)
    if ($savedDpiContext -eq [IntPtr]::Zero -or ![AudioLibRepaintNative]::AreDpiAwarenessContextsEqual([AudioLibRepaintNative]::GetThreadDpiAwarenessContext(),$perMonitorV2)) {
        throw 'Unable to establish per-monitor DPI awareness for native capture.'
    }
    $env:AUDIOLIB_DATA_DIR = $testRoot
    $env:SLINT_BACKEND = 'winit-software'
    $env:RUST_LOG = 'info'
    # Playback control is per USERNAME, independent of the data directory.
    $env:USERNAME = 'AudioLibRepaint_' + [Guid]::NewGuid().ToString('N')
    # Persist the owned library before any CLI/GUI startup can discover legacy inputs.
    $settings = @{ active_database=(Join-Path $testRoot 'library.db'); language='en'; theme=$Theme; playback=@{volume=0.0}; allowed_scan_roots=@(); data_source=@{mode='local';server_url=''} } | ConvertTo-Json -Depth 5
    [IO.File]::WriteAllText((Join-Path $testRoot 'settings.json'),$settings,[Text.UTF8Encoding]::new($false))
    $created = (& $cli db create (Join-Path $testRoot 'library.db') | Out-String) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or !$created.success) { throw 'Isolated empty library creation failed.' }
    $initial = (& $cli stats | Out-String) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or !$initial.success -or $initial.data.audio_files -ne 0) { throw 'Isolated empty library initialization failed.' }
    $process = Start-Process -FilePath $desktop -ArgumentList @('--smoke-ui','--smoke-view','window-repaint','--smoke-delay-ms','58000','--smoke-duration-ms','60000','--screenshot',('"' + (Join-Path $artifacts 'final-software.png') + '"')) -WindowStyle Hidden -PassThru
    $phase = 'application-window'
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        Start-Sleep -Milliseconds 100
        $process.Refresh()
        if ($process.HasExited) { throw "Test desktop exited early: $($process.ExitCode)" }
        $handle = [AudioLibDesktopWindowSelector]::FindApplicationWindow($process.Id)
    } while ($handle -eq [IntPtr]::Zero -and [DateTime]::UtcNow -lt $deadline)
    if ($handle -eq 0) { throw 'Isolated desktop window did not open.' }
    $phase = 'controller-ready'
    $expectedSettings = Get-CanonicalSettingsPath (Join-Path $testRoot 'settings.json')
    $readyMatches = $false
    do {
        $ready = (& $cli status 2>$null | Out-String) | ConvertFrom-Json
        if ($ready.success) {
            $actualSettings = Get-CanonicalSettingsPath ([string]$ready.metadata.settings_path)
            $readyMatches = $null -ne $actualSettings -and $actualSettings.Equals($expectedSettings,[StringComparison]::OrdinalIgnoreCase)
        }
        if (!$readyMatches) { Start-Sleep -Milliseconds 100 }
    } while (!$readyMatches -and [DateTime]::UtcNow -lt $deadline)
    if (!$readyMatches) { throw 'Owned controller did not become ready with the exact isolated settings identity.' }
    Assert-CurrentApplicationWindow
    [AudioLibRepaintNative]::ShowWindow($handle,9) | Out-Null
    Assert-CurrentApplicationWindow
    $dpi = [AudioLibRepaintNative]::GetDpiForWindow($handle)
    $workArea = [Windows.Forms.Screen]::FromHandle($handle).WorkingArea
    $width = [Math]::Min([Math]::Ceiling(1120*$dpi/96),$workArea.Width-60)
    $height = [Math]::Min([Math]::Ceiling(740*$dpi/96),$workArea.Height-60)
    # Keep the application in its normal non-topmost band. A topmost app can
    # overtake a later non-activating topmost cover without losing foreground.
    if (![AudioLibRepaintNative]::SetWindowPos($handle,[IntPtr](-2),$workArea.Left+30,$workArea.Top+30,$width,$height,0x0040)) { throw 'Could not position the owned application window.' }
    $initialFocusRequested = [AudioLibRepaintNative]::FocusOwnedWindow($handle,$process.Id)
    Start-Sleep -Milliseconds 1800
    $inputDesktopAvailable = [AudioLibRepaintNative]::InputDesktopAvailable()
    $initialForeground = [AudioLibRepaintNative]::GetForegroundWindow() -eq $handle
    if (!$inputDesktopAvailable) { throw 'Native foreground acceptance requires an accessible input desktop.' }
    if (!$initialForeground) {
        $foregroundHandle = [AudioLibRepaintNative]::GetForegroundWindow()
        [uint32]$foregroundOwner = 0
        $foregroundThread = [AudioLibRepaintNative]::GetWindowThreadProcessId($foregroundHandle,[ref]$foregroundOwner)
        throw "The available input desktop did not give the owned test window foreground focus: requested=$initialFocusRequested; owned HWND=$handle; foreground HWND=$foregroundHandle; foreground PID=$foregroundOwner; foreground TID=$foregroundThread; owned visible=$([AudioLibRepaintNative]::IsWindowVisible($handle))."
    }
    $phase = 'baseline'
    Assert-CurrentApplicationWindow
    $before = [AudioLibRepaintNative]::Capture($handle)
    $before.Save((Join-Path $artifacts 'before.png'))
    if (![AudioLibRepaintNative]::HasContent($before)) { throw 'The baseline client did not contain visible application controls. Captured baseline retained as before.png.' }
    $dpi = [AudioLibRepaintNative]::GetDpiForWindow($handle)
    foreach ($action in @('refocus','occlusion','minimize-restore')) {
      foreach ($cycle in 1..3) {
        $phase = "$action-$cycle"
        Assert-CurrentApplicationWindow
        $inputBefore = [AudioLibRepaintNative]::InputDesktopAvailable()
        $appForegroundBefore = [AudioLibRepaintNative]::GetForegroundWindow() -eq $handle
        $coverVisible = $null
        $coverFocusRequested = $null
        $coverForeground = $null
        $appForegroundWhileCovered = $null
        $minimizeLostForeground = $null
        $coverActuallyCovered = $null
        $coverClient = $null
        $coveredClient = $null
        $wasMinimized = $null
        $minimizeCoverForeground = $null
        $coverageFailure = $null
        $coverTrace = @()
        if ($action -eq 'minimize-restore') {
            Assert-CurrentApplicationWindow
            $coveredClient = [AudioLibRepaintNative]::Client($handle)
            [AudioLibRepaintNative]::ShowWindow($handle,6) | Out-Null
            Start-Sleep -Milliseconds 250
            $wasMinimized = [AudioLibRepaintNative]::IsIconic($handle)
            # Provide our own next foreground window rather than depending on
            # unrelated windows being available when the application minimizes.
            $alternate = [AudioLibRepaintCover]::new()
            try {
                $alternate.ActivateCover = $true
                $alternate.FormBorderStyle = 'None'
                $alternate.StartPosition = 'Manual'
                $alternate.AutoScaleMode = 'None'
                $alternate.ShowInTaskbar = $false
                $alternate.Bounds = $coveredClient
                $alternate.Show()
                [Windows.Forms.Application]::DoEvents()
                Assert-CurrentApplicationWindow
                [AudioLibRepaintNative]::FocusFromOwnedWindow($alternate.Handle,[Diagnostics.Process]::GetCurrentProcess().Id,$handle,$process.Id) | Out-Null
                $alternateDeadline = [DateTime]::UtcNow.AddMilliseconds(500)
                do {
                    [Windows.Forms.Application]::DoEvents()
                    Start-Sleep -Milliseconds 15
                } while ([AudioLibRepaintNative]::GetForegroundWindow() -ne $alternate.Handle -and [DateTime]::UtcNow -lt $alternateDeadline)
                $minimizeCoverForeground = [AudioLibRepaintNative]::GetForegroundWindow() -eq $alternate.Handle
                $minimizeLostForeground = [AudioLibRepaintNative]::GetForegroundWindow() -ne $handle
                $alternate.Close()
                [Windows.Forms.Application]::DoEvents()
            } finally { $alternate.Dispose() }
            Assert-CurrentApplicationWindow
            [AudioLibRepaintNative]::ShowWindow($handle,9) | Out-Null
        } else {
            $cover = [AudioLibRepaintCover]::new()
            try {
                $cover.ActivateCover = $action -eq 'refocus'
                $cover.FormBorderStyle = 'None'
                $cover.StartPosition = 'Manual'
                $cover.AutoScaleMode = 'None'
                $cover.ShowInTaskbar = $false
                $cover.Bounds = [AudioLibRepaintNative]::Client($handle)
                $cover.TopMost = $true
                $cover.BackColor = [Drawing.Color]::Black
                $cover.Show()
                $coverTrace += 'shown: '+[AudioLibRepaintNative]::WindowState($cover.Handle)
                [Windows.Forms.Application]::DoEvents()
                # WinForms initial placement/scaling must not move the cover
                # away from the physical client rectangle being tested.
                $coveredClient = [AudioLibRepaintNative]::Client($handle)
                # A borderless DWM window can have rounded corners. Extend the
                # owned cover so all app-client corners lie inside its interior.
                $coverMargin = [int][Math]::Ceiling(16*$dpi/96)
                if (![AudioLibRepaintNative]::SetWindowPos($cover.Handle,[IntPtr](-1),$coveredClient.Left-$coverMargin,$coveredClient.Top-$coverMargin,$coveredClient.Width+2*$coverMargin,$coveredClient.Height+2*$coverMargin,0x0050)) { throw 'Could not cover the owned application client.' }
                $coverTrace += 'positioned: '+[AudioLibRepaintNative]::WindowState($cover.Handle)
                [Windows.Forms.Application]::DoEvents()
                $coverTrace += 'pumped: '+[AudioLibRepaintNative]::WindowState($cover.Handle)
                if ($action -eq 'refocus') {
                    Assert-CurrentApplicationWindow
                    $coverFocusRequested = [AudioLibRepaintNative]::FocusFromOwnedWindow($cover.Handle,[Diagnostics.Process]::GetCurrentProcess().Id,$handle,$process.Id)
                    [Windows.Forms.Application]::DoEvents()
                }
                # Service the cover's UI queue during exposure so native hit
                # testing can reach it even after repeated non-active shows.
                $coverDeadline = [DateTime]::UtcNow.AddMilliseconds(250)
                do {
                    [Windows.Forms.Application]::DoEvents()
                    Start-Sleep -Milliseconds 15
                } while ([DateTime]::UtcNow -lt $coverDeadline)
                [Windows.Forms.Application]::DoEvents()
                $coverVisible = [AudioLibRepaintNative]::IsWindowVisible($cover.Handle)
                $coverForeground = [AudioLibRepaintNative]::GetForegroundWindow() -eq $cover.Handle
                $appForegroundWhileCovered = [AudioLibRepaintNative]::GetForegroundWindow() -eq $handle
                $coverageFailure = [AudioLibRepaintNative]::CoverageFailure($handle,$cover.Handle)
                $coverActuallyCovered = $null -eq $coverageFailure
                $coverClient = [AudioLibRepaintNative]::Client($cover.Handle)
                $coverTrace += 'sampled: '+[AudioLibRepaintNative]::WindowState($cover.Handle)
                $cover.Close()
                [Windows.Forms.Application]::DoEvents()
            } finally { $cover.Dispose() }
        }
        $appFocusRequested = [AudioLibRepaintNative]::FocusOwnedWindow($handle,$process.Id)
        Start-Sleep -Milliseconds 400
        $appForegroundAfter = [AudioLibRepaintNative]::GetForegroundWindow() -eq $handle
        $caseInputAvailable = $inputBefore -and [AudioLibRepaintNative]::InputDesktopAvailable()
        $foregroundVerified = $caseInputAvailable -and $appForegroundBefore -and $appForegroundAfter
        if ($action -eq 'refocus') { $foregroundVerified = $foregroundVerified -and $coverVisible -and $coverForeground -and $coverActuallyCovered }
        elseif ($action -eq 'occlusion') { $foregroundVerified = $foregroundVerified -and $coverVisible -and $appForegroundWhileCovered -and $coverActuallyCovered }
        else { $foregroundVerified = $foregroundVerified -and $minimizeLostForeground -and $minimizeCoverForeground -and $wasMinimized -and ![AudioLibRepaintNative]::IsIconic($handle) }
        $facts = [PSCustomObject]@{action=$action;cycle=$cycle;hwnd=$handle.ToInt64();process_id=$process.Id;input_desktop_available=$caseInputAvailable;app_foreground_before=$appForegroundBefore;app_foreground_after=$appForegroundAfter;cover_visible=$coverVisible;cover_foreground=$coverForeground;cover_actually_covered=$coverActuallyCovered;coverage_failure=$coverageFailure;app_client="$coveredClient";cover_client="$coverClient";app_foreground_while_covered=$appForegroundWhileCovered;was_minimized=$wasMinimized;foreground_verified=$foregroundVerified}
        $facts | Add-Member -NotePropertyName cover_trace -NotePropertyValue $coverTrace
        $facts | Add-Member -NotePropertyName minimize_cover_foreground -NotePropertyValue $minimizeCoverForeground
        $facts | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $artifacts ($phase+'-facts.json')) -Encoding utf8
        if (!$foregroundVerified) { throw "$action cycle $cycle did not perform the required owned-window transition. Facts retained in $phase-facts.json." }
        Assert-CurrentApplicationWindow
        $after = [AudioLibRepaintNative]::Capture($handle)
        try {
            $after.Save((Join-Path $artifacts ($action + '-' + $cycle + '.png')))
            $difference = [AudioLibRepaintNative]::Difference($before,$after)
            if ($difference -gt 0.02) { throw "$action lost window contents: $difference changed pixels." }
            $cases += [PSCustomObject]@{ action=$action; cycle=$cycle; changed_fraction=$difference; full_client_preserved=$true; input_desktop_available=$caseInputAvailable; foreground_exposure_verified=$foregroundVerified; app_foreground_before=$appForegroundBefore; cover_visible=$coverVisible; cover_actually_covered=$coverActuallyCovered; cover_focus_request_succeeded=$coverFocusRequested; cover_was_foreground=$coverForeground; app_foreground_while_covered=$appForegroundWhileCovered; minimize_lost_foreground=$minimizeLostForeground; was_minimized=$wasMinimized; minimize_cover_foreground=$minimizeCoverForeground; app_focus_request_succeeded=$appFocusRequested; app_foreground_after=$appForegroundAfter }
        } finally { $after.Dispose() }
      }
    }
    # WM_CLOSE uses the application's drain path; never terminate a user instance.
    $phase = 'close'
    Assert-CurrentApplicationWindow
    if (![AudioLibRepaintNative]::CloseOwnedWindow($handle,$process.Id)) { throw 'Could not close the owned test window.' }
    if (!$process.WaitForExit(12000)) { throw 'The isolated test window did not close gracefully; its data is retained.' }
    if ($process.ExitCode -ne 0) { throw "The isolated desktop failed: $($process.ExitCode)" }
    $shutdownLog = (@(Get-ChildItem -LiteralPath (Join-Path $testRoot 'logs') -File) | ForEach-Object { Get-Content -LiteralPath $_.FullName -Raw }) -join "`n"
    if ($shutdownLog -notmatch 'Desktop shutdown complete') { throw 'The owned application did not complete its normal shutdown path.' }
    $foregroundExposureVerified = @($cases | Where-Object { !$_.foreground_exposure_verified }).Count -eq 0
    $limitations = if ($foregroundExposureVerified) { @() } else { @('Input desktop was inaccessible for one or more cycles; retained client pixels were verified, but foreground exposure and focus restoration were not verified.') }
    $report = [PSCustomObject]@{ success=$true; binary_sha256=(Get-FileHash -LiteralPath $desktop -Algorithm SHA256).Hash; native_client_size=@($before.Width,$before.Height); window_dpi=$dpi; capture_thread_dpi_context='per-monitor-aware-v2'; capture_method='owned-client-DC BitBlt SRCCOPY'; input_desktop_available=$inputDesktopAvailable; foreground_exposure_verified=$foregroundExposureVerified; initial_focus_request_succeeded=$initialFocusRequested; initial_app_foreground=$initialForeground; limitations=@($limitations); isolated_empty_library=$true; synthetic_unsaved_form=$true; cases=$cases }
    $report | Add-Member -NotePropertyName theme -NotePropertyValue $Theme
    $report | Add-Member -NotePropertyName application_window -NotePropertyValue @{hwnd=$handle.ToInt64();process_id=$process.Id;title='AudioLib Explorer';class='Window Class';controller_settings_verified=$readyMatches}
    $report | Add-Member -NotePropertyName normal_shutdown -NotePropertyValue $true
    $report | Add-Member -NotePropertyName report_path -NotePropertyValue (Join-Path $artifacts 'report.json')
    $report | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $artifacts 'report.json') -Encoding UTF8
    $report | ConvertTo-Json -Depth 5
} catch {
    @{success=$false;phase=$phase;theme=$Theme;process_id=$(if($process){$process.Id}else{$null});hwnd=$handle.ToInt64();error=$_.Exception.Message;cases=$cases;test_root=$testRoot} | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $artifacts 'failure-private.json') -Encoding utf8
    if (Test-Path -LiteralPath (Join-Path $testRoot 'logs')) {
        foreach ($logFile in @(Get-ChildItem -LiteralPath (Join-Path $testRoot 'logs') -File)) { Copy-Item -LiteralPath $logFile.FullName -Destination (Join-Path $artifacts ('failed-' + $logFile.Name)) -Force }
    }
    Write-Warning ('Native exposure regression failed: ' + $_.Exception.ToString())
    Write-Warning ([AudioLibRepaintNative]::CaptureEnvironment())
    throw
} finally {
    if ($savedDpiContext -ne [IntPtr]::Zero) {
        [AudioLibRepaintNative]::SetThreadDpiAwarenessContext($savedDpiContext) | Out-Null
    }
    if ($before) { $before.Dispose() }
    try {
        if ($process -and !$process.HasExited) {
            $cleanupHandle = [AudioLibDesktopWindowSelector]::FindApplicationWindow($process.Id)
            if ($cleanupHandle -ne [IntPtr]::Zero) {
                [AudioLibDesktopWindowSelector]::AssertApplicationWindow($cleanupHandle,$process.Id)
                if ([AudioLibRepaintNative]::CloseOwnedWindow($cleanupHandle,$process.Id)) { $process.WaitForExit(12000) | Out-Null }
            }
            if (!$process.HasExited) { Write-Warning "Owned test process $($process.Id) remains alive; data retained. No utility window was closed." }
        }
    } catch {
        Write-Warning ('Owned-window cleanup failed; original acceptance result is retained: ' + $_.Exception.Message)
    } finally {
        $env:AUDIOLIB_DATA_DIR = $savedData
        $env:SLINT_BACKEND = $savedBackend
        $env:RUST_LOG = $savedLog
        $env:USERNAME = $savedUserName
    }
    if (!$process -or $process.HasExited) {
        $resolvedTest = [IO.Path]::GetFullPath($testRoot)
        if (!$resolvedTest.StartsWith($tempParent + [IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)) { throw 'Refusing cleanup outside the owned temporary root.' }
        $pending = [Collections.Generic.Stack[string]]::new()
        $pending.Push($resolvedTest)
        while ($pending.Count -gt 0) {
            $candidate = $pending.Pop()
            $item = Get-Item -LiteralPath $candidate -Force
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Refusing cleanup of a temporary subtree containing a reparse point.' }
            if ($item.PSIsContainer) { foreach ($child in Get-ChildItem -LiteralPath $candidate -Force) { $pending.Push($child.FullName) } }
        }
        Remove-Item -LiteralPath $resolvedTest -Recurse -Force
    }
}
} finally {
    Restore-AudioLibConsoleEncoding
}

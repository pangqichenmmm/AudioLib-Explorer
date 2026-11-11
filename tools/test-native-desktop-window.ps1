# tools/test-native-desktop-window.ps1
# Exercise real Win32 selection against owned off-screen fixtures; never start or control the application.
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'native-desktop-window.ps1')
if (!('AudioLibDesktopWindowFixture' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;

public static class AudioLibDesktopWindowFixture {
    private delegate IntPtr WindowProcedure(IntPtr window, uint message, IntPtr wParam, IntPtr lParam);
    private delegate bool EnumWindowCallback(IntPtr window, IntPtr parameter);
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct WindowClass {
        public uint Size, Style;
        public WindowProcedure Procedure;
        public int ClassExtra, WindowExtra;
        public IntPtr Instance, Icon, Cursor, Background;
        public string MenuName, ClassName;
        public IntPtr SmallIcon;
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr GetModuleHandle(string name);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern ushort RegisterClassEx(ref WindowClass windowClass);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool UnregisterClass(string name, IntPtr instance);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr CreateWindowEx(uint extendedStyle, string className, string title,
        uint style, int x, int y, int width, int height, IntPtr parent, IntPtr menu, IntPtr instance, IntPtr parameter);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr DefWindowProc(IntPtr window, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool DestroyWindow(IntPtr window);
    [DllImport("user32.dll")]
    private static extern bool IsWindow(IntPtr window);
    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool SetWindowPos(IntPtr window, IntPtr after, int x, int y, int width, int height, uint flags);
    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool EnumWindows(EnumWindowCallback callback, IntPtr parameter);

    private static readonly WindowProcedure Procedure = DefWindowProc;
    private static readonly IntPtr Instance = GetModuleHandle(null);
    private static readonly HashSet<string> Classes = new HashSet<string>(StringComparer.Ordinal);
    private static readonly HashSet<IntPtr> Windows = new HashSet<IntPtr>();
    public static uint ProcessId { get { return (uint)Process.GetCurrentProcess().Id; } }

    public static IntPtr Create(string className, string title, bool visible) {
        if (!Classes.Contains(className)) {
            var kind = new WindowClass();
            kind.Size = (uint)Marshal.SizeOf(typeof(WindowClass));
            kind.Procedure = Procedure;
            kind.Instance = Instance;
            kind.ClassName = className;
            if (RegisterClassEx(ref kind) == 0)
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot register owned fixture class");
            Classes.Add(className);
        }
        // WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE; keep every fixture off-screen.
        // WS_VISIBLE lets selection exercise real visibility without activation.
        var window = CreateWindowEx(0x08000080, className, title,
            0x80000000u | (visible ? 0x10000000u : 0u), -32000, -32000, 160, 90,
            IntPtr.Zero, IntPtr.Zero, Instance, IntPtr.Zero);
        if (window == IntPtr.Zero)
            throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot create owned fixture window");
        Windows.Add(window);
        return window;
    }

    public static void PutBefore(IntPtr first, IntPtr second) {
        AssertOwned(first);
        AssertOwned(second);
        // HWND_TOP, SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE: no focus request.
        if (!SetWindowPos(first, IntPtr.Zero, 0, 0, 0, 0, 0x0013))
            throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot order owned fixture windows");
        var order = new List<IntPtr>();
        if (!EnumWindows((window, parameter) => {
            if (window == first || window == second) order.Add(window);
            return true;
        }, IntPtr.Zero)) throw new Win32Exception(Marshal.GetLastWin32Error());
        if (order.Count != 2 || order[0] != first || order[1] != second)
            throw new InvalidOperationException("Utility fixture was not enumerated before the application fixture");
    }

    private static void AssertOwned(IntPtr window) {
        uint process;
        if (!Windows.Contains(window) || !IsWindow(window) ||
            GetWindowThreadProcessId(window, out process) == 0 || process != ProcessId)
            throw new InvalidOperationException("Refusing to modify a window outside the owned fixture set");
    }

    public static void Destroy(IntPtr window) {
        AssertOwned(window);
        if (!DestroyWindow(window))
            throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot destroy owned fixture window");
        Windows.Remove(window);
    }

    public static void Cleanup() {
        foreach (var window in new List<IntPtr>(Windows)) Destroy(window);
        foreach (var name in new List<string>(Classes)) {
            if (!UnregisterClass(name, Instance))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot unregister owned fixture class");
            Classes.Remove(name);
        }
    }
}
'@
}

function Assert-SelectorRejected([string]$Name, [scriptblock]$Operation, [string]$ExpectedMessage) {
    $caught = $false
    try { & $Operation } catch {
        if (!$_.Exception.ToString().Contains($ExpectedMessage)) { throw }
        $caught = $true
    }
    if (!$caught) { throw "$Name was accepted." }
}

$fixtureProcess = [AudioLibDesktopWindowFixture]::ProcessId
try {
    $utility = [AudioLibDesktopWindowFixture]::Create('Winit Thread Event Target', 'AudioLib Explorer', $true)
    $wrongTitle = [AudioLibDesktopWindowFixture]::Create('Window Class', 'Owned selector fixture', $true)
    $hidden = [AudioLibDesktopWindowFixture]::Create('Window Class', 'AudioLib Explorer', $false)
    if ([AudioLibDesktopWindowSelector]::FindApplicationWindow($fixtureProcess) -ne [IntPtr]::Zero) {
        throw 'Utility, wrong-title or hidden window was selected as the application.'
    }
    Assert-SelectorRejected 'Utility target' { [AudioLibDesktopWindowSelector]::AssertApplicationWindow($utility, $fixtureProcess) } 'not the visible owned'
    Assert-SelectorRejected 'Wrong title' { [AudioLibDesktopWindowSelector]::AssertApplicationWindow($wrongTitle, $fixtureProcess) } 'not the visible owned'
    Assert-SelectorRejected 'Hidden target' { [AudioLibDesktopWindowSelector]::AssertApplicationWindow($hidden, $fixtureProcess) } 'not the visible owned'

    $application = [AudioLibDesktopWindowFixture]::Create('Window Class', 'AudioLib Explorer', $true)
    [AudioLibDesktopWindowFixture]::PutBefore($utility, $application)
    if ([AudioLibDesktopWindowSelector]::FindApplicationWindow($fixtureProcess) -ne $application) {
        throw 'A utility-first enumeration did not select the owned application UI.'
    }
    [AudioLibDesktopWindowSelector]::AssertApplicationWindow($application, $fixtureProcess)
    # PID zero cannot own this fixture: rejection must use actual native ownership.
    Assert-SelectorRejected 'Foreign process identity' { [AudioLibDesktopWindowSelector]::AssertApplicationWindow($application, [uint32]0) } 'not the visible owned'
    if ([AudioLibDesktopWindowSelector]::FindApplicationWindow([uint32]0) -ne [IntPtr]::Zero) {
        throw 'A foreign process identity selected the owned fixture.'
    }

    $duplicate = [AudioLibDesktopWindowFixture]::Create('Window Class', 'AudioLib Explorer', $true)
    Assert-SelectorRejected 'Ambiguous selection' { [AudioLibDesktopWindowSelector]::FindApplicationWindow($fixtureProcess) } 'Multiple visible'
    Assert-SelectorRejected 'Cached handle amid ambiguity' { [AudioLibDesktopWindowSelector]::AssertApplicationWindow($application, $fixtureProcess) } 'Multiple visible'
    [AudioLibDesktopWindowFixture]::Destroy($duplicate)
    [AudioLibDesktopWindowSelector]::AssertApplicationWindow($application, $fixtureProcess)
    [AudioLibDesktopWindowFixture]::Destroy($application)
    Assert-SelectorRejected 'Destroyed handle' { [AudioLibDesktopWindowSelector]::AssertApplicationWindow($application, $fixtureProcess) } 'not the visible owned'
    if ([AudioLibDesktopWindowSelector]::FindApplicationWindow($fixtureProcess) -ne [IntPtr]::Zero) {
        throw 'A destroyed application handle or remaining utility window was selected.'
    }
    Write-Host 'PASS: utility-first, wrong title, hidden window, foreign PID, ambiguity and destroyed HWND selection.'
} finally {
    # Cleanup only handles and classes created by this fixture; no application is launched.
    [AudioLibDesktopWindowFixture]::Cleanup()
}

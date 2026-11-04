# tools/native-desktop-window.ps1
# Read-only native selection of the owned Slint application UI, never winit's utility window.
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Native desktop window selection requires Windows.'
}
if (!('AudioLibDesktopWindowSelector' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class AudioLibDesktopWindowSelector {
    private delegate bool EnumWindowCallback(IntPtr window, IntPtr parameter);
    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool EnumWindows(EnumWindowCallback callback, IntPtr parameter);
    [DllImport("user32.dll")]
    private static extern bool IsWindow(IntPtr window);
    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowText(IntPtr window, StringBuilder text, int count);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetClassName(IntPtr window, StringBuilder text, int count);

    private static bool MatchesApplicationWindow(IntPtr window, uint process) {
        uint owner;
        if (window == IntPtr.Zero || !IsWindow(window) || !IsWindowVisible(window) ||
            GetWindowThreadProcessId(window, out owner) == 0 || owner != process) return false;
        var title = new StringBuilder(256);
        var kind = new StringBuilder(256);
        GetWindowText(window, title, title.Capacity);
        GetClassName(window, kind, kind.Capacity);
        return String.Equals(title.ToString(), "AudioLib Explorer", StringComparison.Ordinal) &&
            String.Equals(kind.ToString(), "Window Class", StringComparison.Ordinal);
    }

    // MainWindowHandle can identify winit's utility target. Enumerate every
    // top-level window and refuse an ambiguous application identity instead.
    public static IntPtr FindApplicationWindow(uint process) {
        var matches = new List<IntPtr>();
        if (!EnumWindows((window, parameter) => {
            if (MatchesApplicationWindow(window, process)) matches.Add(window);
            return true;
        }, IntPtr.Zero)) throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot enumerate desktop windows");
        if (matches.Count > 1)
            throw new InvalidOperationException("Multiple visible AudioLib application windows belong to the owned process");
        if (matches.Count == 0 || !MatchesApplicationWindow(matches[0], process)) return IntPtr.Zero;
        return matches[0];
    }

    // Recheck identity immediately before an operation; a PID alone also admits
    // the event-loop utility target, which must never receive UI/close messages.
    public static void AssertApplicationWindow(IntPtr window, uint process) {
        if (!MatchesApplicationWindow(window, process))
            throw new InvalidOperationException("Target is not the visible owned AudioLib application window");
        if (FindApplicationWindow(process) != window || !MatchesApplicationWindow(window, process))
            throw new InvalidOperationException("Target is not the current unique owned AudioLib application window");
    }
}
'@
}

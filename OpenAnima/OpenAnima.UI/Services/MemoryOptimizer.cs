using System;
using System.Diagnostics;
using System.Runtime.InteropServices;

namespace OpenAnima.UI.Services;

/// <summary>
/// Memory management utility for flushing unused working set pages to the OS.
/// </summary>
public static class MemoryOptimizer
{
    [DllImport("kernel32.dll", EntryPoint = "SetProcessWorkingSetSize", SetLastError = true)]
    private static extern bool SetProcessWorkingSetSize(IntPtr hProcess, IntPtr dwMinimumWorkingSetSize, IntPtr dwMaximumWorkingSetSize);

    [DllImport("psapi.dll", EntryPoint = "EmptyWorkingSet", SetLastError = true)]
    private static extern bool EmptyWorkingSet(IntPtr hProcess);

    /// <summary>
    /// Forces a full GC pass and trims the process working set down to the absolute active minimum.
    /// Ideal when minimizing to system tray, hiding the window, or after stopping animations.
    /// </summary>
    public static void TrimWorkingSet()
    {
        try
        {
            GC.Collect(2, GCCollectionMode.Aggressive, true, true);
            GC.WaitForPendingFinalizers();
            GC.Collect(2, GCCollectionMode.Aggressive, true, true);

            IntPtr handle = Process.GetCurrentProcess().Handle;
            if (handle != IntPtr.Zero)
            {
                // Passing (IntPtr)(-1), (IntPtr)(-1) flushes unused pages from the working set
                SetProcessWorkingSetSize(handle, (IntPtr)(-1), (IntPtr)(-1));
                EmptyWorkingSet(handle);
            }
        }
        catch
        {
            // Best effort; silently continue if OS denies rights
        }
    }
}

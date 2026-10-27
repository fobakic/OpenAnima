using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Interop;
using OpenAnima.UI.Models;

namespace OpenAnima.UI.Services;

public sealed class HotkeyService : IDisposable
{
    private const int WM_HOTKEY = 0x0312;
    private const uint MOD_NOREPEAT = 0x4000;
    private const int ERROR_HOTKEY_ALREADY_REGISTERED = 1409;
    private const int ERROR_HOTKEY_NOT_REGISTERED = 1418;

    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool RegisterHotKey(IntPtr hWnd, int id, uint fsModifiers, uint vk);

    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool UnregisterHotKey(IntPtr hWnd, int id);

    public event Action<string>? OnProfileHotkeyPressed;

    private IntPtr _hwnd = IntPtr.Zero;
    private HwndSource? _source;
    private readonly Dictionary<int, string> _registeredIds = [];
    private int _nextId = 2000;
    private bool _disposed;

    public void Initialize(Window window)
    {
        var helper = new WindowInteropHelper(window);
        _hwnd = helper.EnsureHandle();

        _source = HwndSource.FromHwnd(_hwnd);
        _source?.AddHook(WndProc);
    }

    public void RegisterProfileHotkeys(IEnumerable<ProfileModel> profiles, bool enabled)
    {
        UnregisterAll();

        if (!enabled || _hwnd == IntPtr.Zero)
            return;

        foreach (var profile in profiles)
        {
            if (profile.Hotkey == null || !profile.Hotkey.IsEnabled)
                continue;

            profile.Hotkey.HasConflict = false;

            if (profile.Hotkey.VirtualKey == 0)
                continue;

            int id = ++_nextId;
            uint modifiers = profile.Hotkey.Modifiers | MOD_NOREPEAT;

            bool success = RegisterHotKey(_hwnd, id, modifiers, profile.Hotkey.VirtualKey);
            if (success)
            {
                _registeredIds[id] = profile.Id;
            }
            else
            {
                int error = Marshal.GetLastWin32Error();
                if (error == ERROR_HOTKEY_ALREADY_REGISTERED || error == ERROR_HOTKEY_NOT_REGISTERED)
                {
                    profile.Hotkey.HasConflict = true;
                    App.Log($"[HotkeyService] Hotkey '{profile.Hotkey.DisplayText}' for profile '{profile.Name}' is already registered by another application (Win32 error {error}).");
                }
                else
                {
                    App.Log($"[HotkeyService] Failed to register hotkey for profile '{profile.Name}'. Error code: {error}");
                }
            }
        }
    }

    public void UnregisterAll()
    {
        if (_hwnd == IntPtr.Zero) return;

        foreach (int id in _registeredIds.Keys)
        {
            UnregisterHotKey(_hwnd, id);
        }
        _registeredIds.Clear();
    }

    private IntPtr WndProc(IntPtr hwnd, int msg, IntPtr wParam, IntPtr lParam, ref bool handled)
    {
        if (msg == WM_HOTKEY)
        {
            int id = wParam.ToInt32();
            if (_registeredIds.TryGetValue(id, out string? profileId) && !string.IsNullOrEmpty(profileId))
            {
                handled = true;
                OnProfileHotkeyPressed?.Invoke(profileId);
            }
        }
        return IntPtr.Zero;
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;

        UnregisterAll();

        if (_source != null)
        {
            _source.RemoveHook(WndProc);
            _source = null;
        }
    }
}

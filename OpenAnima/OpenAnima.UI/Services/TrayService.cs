using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Interop;

namespace OpenAnima.UI.Services;

public sealed class TrayService : IDisposable
{
    private const int WM_USER = 0x0400;
    public const int WM_TRAYICON = WM_USER + 1024;

    private const int WM_LBUTTONUP = 0x0202;
    private const int WM_LBUTTONDBLCLK = 0x0203;
    private const int WM_RBUTTONUP = 0x0205;

    private const uint NIM_ADD = 0x00000000;
    private const uint NIM_MODIFY = 0x00000001;
    private const uint NIM_DELETE = 0x00000002;

    private const uint NIF_MESSAGE = 0x00000001;
    private const uint NIF_ICON = 0x00000002;
    private const uint NIF_TIP = 0x00000004;

    private const uint IMAGE_ICON = 1;
    private const uint LR_LOADFROMFILE = 0x0010;
    private const uint LR_DEFAULTSIZE = 0x0040;

    private IntPtr _hwnd = IntPtr.Zero;
    private IntPtr _hIcon = IntPtr.Zero;
    private HwndSource? _hwndSource;
    private bool _isCreated;
    private bool _disposed;

    public event Action? OnTrayDoubleClicked;
    public event Action? OnTrayRightClicked;
    public event Action? OnRequestShowWindow;

    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern uint RegisterWindowMessageW(string lpString);

    private uint _showWindowMsg = 0;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct NOTIFYICONDATA
    {
        public int cbSize;
        public IntPtr hWnd;
        public uint uID;
        public uint uFlags;
        public uint uCallbackMessage;
        public IntPtr hIcon;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)]
        public string szTip;
    }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    private static extern bool Shell_NotifyIconW(uint dwMessage, ref NOTIFYICONDATA lpData);

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    private static extern uint ExtractIconExW(string szFileName, int nIconIndex, out IntPtr phiconLarge, out IntPtr phiconSmall, uint nIcons);

    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern IntPtr LoadImageW(IntPtr hinst, string lpszName, uint uType, int cxDesired, int cyDesired, uint fuLoad);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern IntPtr LoadIconW(IntPtr hInstance, IntPtr lpIconName);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool DestroyIcon(IntPtr hIcon);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hWnd);

    public void Initialize(Window window, string tooltip = "OpenAnima")
    {
        var helper = new WindowInteropHelper(window);
        _hwnd = helper.Handle;

        if (_hwnd == IntPtr.Zero)
        {
            helper.EnsureHandle();
            _hwnd = helper.Handle;
        }

        _hwndSource = HwndSource.FromHwnd(_hwnd);
        _hwndSource?.AddHook(WndProc);

        _showWindowMsg = RegisterWindowMessageW("OpenAnima_ShowWindow");

        // Load icon
        _hIcon = LoadAppIcon();

        var nid = new NOTIFYICONDATA
        {
            cbSize = Marshal.SizeOf<NOTIFYICONDATA>(),
            hWnd = _hwnd,
            uID = 1,
            uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage = WM_TRAYICON,
            hIcon = _hIcon,
            szTip = tooltip.Length > 127 ? tooltip[..127] : tooltip
        };

        _isCreated = Shell_NotifyIconW(NIM_ADD, ref nid);
    }

    private static IntPtr LoadAppIcon()
    {
        string baseDir = AppDomain.CurrentDomain.BaseDirectory;
        string[] candidates =
        [
            Path.Combine(baseDir, "Assets", "openanima.ico"),
            Path.GetFullPath(Path.Combine(baseDir, @"..\..\..\Assets\openanima.ico")),
            Path.Combine(baseDir, "openanima.ico")
        ];

        foreach (var path in candidates)
        {
            if (File.Exists(path))
            {
                IntPtr h = LoadImageW(IntPtr.Zero, path, IMAGE_ICON, 16, 16, LR_LOADFROMFILE);
                if (h != IntPtr.Zero)
                    return h;
            }
        }

        // Try extracting icon from running executable
        try
        {
            string? procPath = Environment.ProcessPath;
            if (!string.IsNullOrEmpty(procPath) && File.Exists(procPath))
            {
                IntPtr largeIcon, smallIcon;
                uint count = ExtractIconExW(procPath, 0, out largeIcon, out smallIcon, 1);
                if (count > 0 && smallIcon != IntPtr.Zero)
                {
                    if (largeIcon != IntPtr.Zero) DestroyIcon(largeIcon);
                    return smallIcon;
                }
                if (count > 0 && largeIcon != IntPtr.Zero)
                {
                    return largeIcon;
                }
            }
        }
        catch { }

        // Fallback: standard system application icon
        return LoadIconW(IntPtr.Zero, (IntPtr)32512); // IDI_APPLICATION
    }

    private IntPtr WndProc(IntPtr hwnd, int msg, IntPtr wParam, IntPtr lParam, ref bool handled)
    {
        if (_showWindowMsg != 0 && (uint)msg == _showWindowMsg)
        {
            OnRequestShowWindow?.Invoke();
            handled = true;
            return IntPtr.Zero;
        }

        if (msg == WM_TRAYICON)
        {
            int mouseMsg = (int)lParam;
            if (mouseMsg == WM_LBUTTONDBLCLK || mouseMsg == WM_LBUTTONUP)
            {
                OnTrayDoubleClicked?.Invoke();
                handled = true;
            }
            else if (mouseMsg == WM_RBUTTONUP)
            {
                OnTrayRightClicked?.Invoke();
                handled = true;
            }
        }

        return IntPtr.Zero;
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;

        if (_isCreated && _hwnd != IntPtr.Zero)
        {
            var nid = new NOTIFYICONDATA
            {
                cbSize = Marshal.SizeOf<NOTIFYICONDATA>(),
                hWnd = _hwnd,
                uID = 1
            };
            Shell_NotifyIconW(NIM_DELETE, ref nid);
            _isCreated = false;
        }

        if (_hwndSource != null)
        {
            _hwndSource.RemoveHook(WndProc);
            _hwndSource = null;
        }

        if (_hIcon != IntPtr.Zero)
        {
            DestroyIcon(_hIcon);
            _hIcon = IntPtr.Zero;
        }
    }
}

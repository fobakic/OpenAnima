using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows;
using System.Windows.Threading;
using OpenAnima.UI.Services;

namespace OpenAnima.UI;

/// <summary>
/// Interaction logic for App.xaml
/// </summary>
public partial class App : Application
{
    private static Mutex? _singleInstanceMutex;

    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
    private static extern uint RegisterWindowMessage(string lpString);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool PostMessage(IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam);

    private static readonly IntPtr HWND_BROADCAST = new(0xFFFF);

    private static readonly string LogDir = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
        "OpenAnima",
        "logs"
    );
    private static readonly string LogPath = Path.Combine(LogDir, "openanima_ui.log");

    public static void Log(string message)
    {
        try
        {
            if (!Directory.Exists(LogDir))
            {
                Directory.CreateDirectory(LogDir);
            }
            var fileInfo = new FileInfo(LogPath);
            if (fileInfo.Exists && fileInfo.Length > 5 * 1024 * 1024)
            {
                string oldPath = LogPath + ".old";
                if (File.Exists(oldPath)) File.Delete(oldPath);
                File.Move(LogPath, oldPath);
            }
            File.AppendAllText(LogPath, $"[{DateTime.Now:HH:mm:ss.fff}] {message}\n");
        }
        catch { }
    }

    protected override void OnStartup(StartupEventArgs e)
    {
        // Enforce single instance per user session
        const string mutexName = "Local\\OpenAnima_SingleInstance_Mutex";
        _singleInstanceMutex = new Mutex(true, mutexName, out bool createdNew);
        if (!createdNew)
        {
            uint msg = RegisterWindowMessage("OpenAnima_ShowWindow");
            if (msg != 0)
            {
                PostMessage(HWND_BROADCAST, msg, IntPtr.Zero, IntPtr.Zero);
            }
            Shutdown();
            return;
        }

        Log("App.OnStartup called");
        base.OnStartup(e);

        AppDomain.CurrentDomain.ProcessExit += (s, ev) => Log("AppDomain.ProcessExit called!");
        Exit += (s, ev) => Log($"Application.Exit called with code: {ev.ApplicationExitCode}");

        Wpf.Ui.Appearance.ApplicationThemeManager.Apply(Wpf.Ui.Appearance.ApplicationTheme.Dark);

        DispatcherUnhandledException += OnDispatcherUnhandledException;
        AppDomain.CurrentDomain.UnhandledException += OnAppDomainUnhandledException;
    }

    protected override void OnExit(ExitEventArgs e)
    {
        if (_singleInstanceMutex != null)
        {
            try { _singleInstanceMutex.ReleaseMutex(); } catch { }
            _singleInstanceMutex.Dispose();
            _singleInstanceMutex = null;
        }
        base.OnExit(e);
    }

    private void OnDispatcherUnhandledException(object sender, DispatcherUnhandledExceptionEventArgs e)
    {
        Log($"DispatcherUnhandledException: {e.Exception}");
        bool isRu = LocalizationService.CurrentLanguage == "ru";
        string title = isRu ? "OpenAnima Ошибка" : "OpenAnima Error";
        string msg = isRu
            ? $"Произошла ошибка при работе интерфейса:\n\n{e.Exception.Message}\n\nСтек вызова:\n{e.Exception.StackTrace}"
            : $"An unexpected UI error occurred:\n\n{e.Exception.Message}\n\nStack trace:\n{e.Exception.StackTrace}";

        MessageBox.Show(msg, title, MessageBoxButton.OK, MessageBoxImage.Error);
        e.Handled = true;
    }

    private void OnAppDomainUnhandledException(object sender, UnhandledExceptionEventArgs e)
    {
        Log($"AppDomainUnhandledException: {e.ExceptionObject}");
        if (e.ExceptionObject is Exception ex)
        {
            bool isRu = LocalizationService.CurrentLanguage == "ru";
            string title = isRu ? "OpenAnima Критическая ошибка" : "OpenAnima Critical Error";
            string msg = isRu
                ? $"Критическая системная ошибка:\n\n{ex.Message}\n\nСтек:\n{ex.StackTrace}"
                : $"Critical system error:\n\n{ex.Message}\n\nStack trace:\n{ex.StackTrace}";

            MessageBox.Show(msg, title, MessageBoxButton.OK, MessageBoxImage.Error);
        }
    }
}

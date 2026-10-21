using System;
using System.IO;
using System.Windows;
using System.Windows.Threading;

namespace OpenAnima.UI;

/// <summary>
/// Interaction logic for App.xaml
/// </summary>
public partial class App : Application
{
    private static readonly string LogPath = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "openanima_ui.log");

    public static void Log(string message)
    {
        try
        {
            File.AppendAllText(LogPath, $"[{DateTime.Now:HH:mm:ss.fff}] {message}\n");
        }
        catch { }
    }

    protected override void OnStartup(StartupEventArgs e)
    {
        Log("App.OnStartup called");
        base.OnStartup(e);

        AppDomain.CurrentDomain.ProcessExit += (s, ev) => Log("AppDomain.ProcessExit called!");
        Exit += (s, ev) => Log($"Application.Exit called with code: {ev.ApplicationExitCode}");

        Wpf.Ui.Appearance.ApplicationThemeManager.Apply(Wpf.Ui.Appearance.ApplicationTheme.Dark);

        DispatcherUnhandledException += OnDispatcherUnhandledException;
        AppDomain.CurrentDomain.UnhandledException += OnAppDomainUnhandledException;
    }

    private void OnDispatcherUnhandledException(object sender, DispatcherUnhandledExceptionEventArgs e)
    {
        Log($"DispatcherUnhandledException: {e.Exception}");
        MessageBox.Show(
            $"Произошла ошибка при работе интерфейса:\n\n{e.Exception.Message}\n\nСтек вызова:\n{e.Exception.StackTrace}",
            "OpenAnima Ошибка",
            MessageBoxButton.OK,
            MessageBoxImage.Error);

        e.Handled = true;
    }

    private void OnAppDomainUnhandledException(object sender, UnhandledExceptionEventArgs e)
    {
        Log($"AppDomainUnhandledException: {e.ExceptionObject}");
        if (e.ExceptionObject is Exception ex)
        {
            MessageBox.Show(
                $"Критическая системная ошибка:\n\n{ex.Message}\n\nСтек:\n{ex.StackTrace}",
                "OpenAnima Критическая ошибка",
                MessageBoxButton.OK,
                MessageBoxImage.Error);
        }
    }
}

using System;
using System.Diagnostics;
using System.IO;
using System.Text.Json;
using System.Threading.Tasks;
using Microsoft.Win32;
using OpenAnima.UI.Models;

namespace OpenAnima.UI.Services;

public class ConfigService
{
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        WriteIndented = true,
        PropertyNameCaseInsensitive = true
    };

    private static string ConfigPath =>
        Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData),
            "OpenAnima",
            "config.json"
        );

    public async Task<AppConfig> LoadAsync()
    {
        try
        {
            string path = ConfigPath;
            if (File.Exists(path))
            {
                string json = await File.ReadAllTextAsync(path);
                var config = JsonSerializer.Deserialize<AppConfig>(json, JsonOptions);
                if (config != null)
                {
                    config.AutoStartWithWindows = IsAutoStartEnabled();
                    return config;
                }
            }
        }
        catch (Exception ex)
        {
            App.Log($"[ConfigService] Error loading config: {ex.Message}");
        }

        var defaultCfg = new AppConfig();
        defaultCfg.AutoStartWithWindows = IsAutoStartEnabled();
        return defaultCfg;
    }

    public async Task SaveAsync(AppConfig config)
    {
        try
        {
            string path = ConfigPath;
            string? dir = Path.GetDirectoryName(path);
            if (!string.IsNullOrEmpty(dir) && !Directory.Exists(dir))
            {
                Directory.CreateDirectory(dir);
            }

            string json = JsonSerializer.Serialize(config, JsonOptions);
            await File.WriteAllTextAsync(path, json);
            App.Log($"[ConfigService] Config saved to {path}");

            // Apply autostart setting
            SetAutoStart(config.AutoStartWithWindows);
        }
        catch (Exception ex)
        {
            App.Log($"[ConfigService] Error saving config: {ex.Message}");
        }
    }

    public static bool IsAutoStartEnabled()
    {
        try
        {
            using var key = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run", false);
            return key?.GetValue("OpenAnima") != null;
        }
        catch
        {
            return false;
        }
    }

    public static void SetAutoStart(bool enable)
    {
        try
        {
            using var key = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run", true);
            if (key == null) return;

            string? exePath = Environment.ProcessPath;
            if (string.IsNullOrEmpty(exePath))
            {
                exePath = Process.GetCurrentProcess().MainModule?.FileName;
            }

            if (string.IsNullOrEmpty(exePath)) return;

            if (enable)
            {
                key.SetValue("OpenAnima", $"\"{exePath}\" --minimized");
            }
            else
            {
                key.DeleteValue("OpenAnima", false);
            }
        }
        catch (Exception ex)
        {
            App.Log($"[ConfigService] Error setting autostart: {ex.Message}");
        }
    }
}

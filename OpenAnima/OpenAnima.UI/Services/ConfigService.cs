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

    private static readonly SemaphoreSlim SaveLock = new(1, 1);

    private static string ConfigPath =>
        Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData),
            "OpenAnima",
            "config.json"
        );

    public async Task<AppConfig> LoadAsync()
    {
        await SaveLock.WaitAsync().ConfigureAwait(false);
        try
        {
            string path = ConfigPath;
            string bakPath = path + ".bak";

            // 1. Try main config file first
            if (File.Exists(path))
            {
                try
                {
                    string json = await File.ReadAllTextAsync(path).ConfigureAwait(false);
                    var config = JsonSerializer.Deserialize<AppConfig>(json, JsonOptions);
                    if (config != null)
                    {
                        config.AutoStartWithWindows = IsAutoStartEnabled();
                        EnsureDefaultProfiles(config);
                        return config;
                    }
                }
                catch (Exception ex)
                {
                    App.Log($"[ConfigService] Error reading main config, attempting backup: {ex.Message}");
                }
            }

            // 2. Try backup file if main was missing or corrupt
            if (File.Exists(bakPath))
            {
                try
                {
                    string json = await File.ReadAllTextAsync(bakPath).ConfigureAwait(false);
                    var config = JsonSerializer.Deserialize<AppConfig>(json, JsonOptions);
                    if (config != null)
                    {
                        config.AutoStartWithWindows = IsAutoStartEnabled();
                        EnsureDefaultProfiles(config);
                        App.Log("[ConfigService] Successfully restored config from backup (.bak)");
                        return config;
                    }
                }
                catch (Exception ex)
                {
                    App.Log($"[ConfigService] Error reading backup config: {ex.Message}");
                }
            }

            var defaultCfg = new AppConfig();
            defaultCfg.AutoStartWithWindows = IsAutoStartEnabled();
            EnsureDefaultProfiles(defaultCfg);
            return defaultCfg;
        }
        finally
        {
            SaveLock.Release();
        }
    }

    public async Task SaveAsync(AppConfig config)
    {
        await SaveLock.WaitAsync().ConfigureAwait(false);
        try
        {
            string path = ConfigPath;
            string? dir = Path.GetDirectoryName(path);
            if (!string.IsNullOrEmpty(dir) && !Directory.Exists(dir))
            {
                Directory.CreateDirectory(dir);
            }

            string json = JsonSerializer.Serialize(config, JsonOptions);
            string tmpPath = path + ".tmp";
            string bakPath = path + ".bak";

            await File.WriteAllTextAsync(tmpPath, json).ConfigureAwait(false);

            if (File.Exists(path))
            {
                try
                {
                    var fi = new FileInfo(path);
                    if (fi.Length > 0)
                    {
                        File.Replace(tmpPath, path, bakPath, ignoreMetadataErrors: true);
                    }
                    else
                    {
                        File.Move(tmpPath, path, overwrite: true);
                    }
                }
                catch
                {
                    File.Move(tmpPath, path, overwrite: true);
                }
            }
            else
            {
                File.Move(tmpPath, path, overwrite: true);
            }

            App.Log($"[ConfigService] Config saved atomically to {path}");
            SetAutoStart(config.AutoStartWithWindows);
        }
        catch (Exception ex)
        {
            App.Log($"[ConfigService] Error saving config: {ex.Message}");
        }
        finally
        {
            SaveLock.Release();
        }
    }

    public void SaveSync(AppConfig config)
    {
        SaveLock.Wait();
        try
        {
            string path = ConfigPath;
            string? dir = Path.GetDirectoryName(path);
            if (!string.IsNullOrEmpty(dir) && !Directory.Exists(dir))
            {
                Directory.CreateDirectory(dir);
            }

            string json = JsonSerializer.Serialize(config, JsonOptions);
            string tmpPath = path + ".tmp";
            string bakPath = path + ".bak";

            File.WriteAllText(tmpPath, json);

            if (File.Exists(path))
            {
                try
                {
                    var fi = new FileInfo(path);
                    if (fi.Length > 0)
                    {
                        File.Replace(tmpPath, path, bakPath, ignoreMetadataErrors: true);
                    }
                    else
                    {
                        File.Move(tmpPath, path, overwrite: true);
                    }
                }
                catch
                {
                    File.Move(tmpPath, path, overwrite: true);
                }
            }
            else
            {
                File.Move(tmpPath, path, overwrite: true);
            }

            App.Log($"[ConfigService] Config saved atomically (sync) to {path}");
            SetAutoStart(config.AutoStartWithWindows);
        }
        catch (Exception ex)
        {
            App.Log($"[ConfigService] Error saving config (sync): {ex.Message}");
        }
        finally
        {
            SaveLock.Release();
        }
    }

    public static void EnsureDefaultProfiles(AppConfig config)
    {
        if (config.Profiles != null && config.Profiles.Count > 0)
            return;

        config.Profiles =
        [
            new ProfileModel
            {
                Id = "default",
                Name = "Default",
                Icon = "Sparkle24",
                IsBuiltIn = true,
                Hotkey = new HotkeyConfig
                {
                    Modifiers = 0x0002 | 0x0004, // MOD_CONTROL | MOD_SHIFT
                    VirtualKey = 0x44,           // 'D'
                    DisplayText = "Ctrl+Shift+D",
                    IsEnabled = true
                }
            },
            new ProfileModel
            {
                Id = "gaming",
                Name = "Gaming",
                Icon = "Games24",
                IsBuiltIn = true,
                ForceClickThrough = true,
                DisablePhysics = true,
                TargetAlpha = 0.35,
                Hotkey = new HotkeyConfig
                {
                    Modifiers = 0x0002 | 0x0004, // MOD_CONTROL | MOD_SHIFT
                    VirtualKey = 0x47,           // 'G'
                    DisplayText = "Ctrl+Shift+G",
                    IsEnabled = true
                }
            },
            new ProfileModel
            {
                Id = "work",
                Name = "Work",
                Icon = "Briefcase24",
                IsBuiltIn = true,
                ForceClickThrough = true,
                DisablePhysics = true,
                TargetScale = 12.0,
                TargetAlpha = 0.60,
                Hotkey = new HotkeyConfig
                {
                    Modifiers = 0x0002 | 0x0004, // MOD_CONTROL | MOD_SHIFT
                    VirtualKey = 0x57,           // 'W'
                    DisplayText = "Ctrl+Shift+W",
                    IsEnabled = true
                }
            },
            new ProfileModel
            {
                Id = "chill",
                Name = "Chill",
                Icon = "DrinkCoffee24",
                IsBuiltIn = true,
                ForceClickThrough = false,
                DisablePhysics = false,
                TargetAlpha = 1.0,
                TargetScale = 20.0,
                Hotkey = new HotkeyConfig
                {
                    Modifiers = 0x0002 | 0x0004, // MOD_CONTROL | MOD_SHIFT
                    VirtualKey = 0x43,           // 'C'
                    DisplayText = "Ctrl+Shift+C",
                    IsEnabled = true
                }
            }
        ];

        if (string.IsNullOrEmpty(config.ActiveProfileId))
        {
            config.ActiveProfileId = "default";
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

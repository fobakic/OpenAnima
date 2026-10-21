using System;
using System.Linq;
using System.Windows;

namespace OpenAnima.UI.Services;

public static class LocalizationService
{
    public static string CurrentLanguage { get; private set; } = "en";

    public static event Action? LanguageChanged;

    public static void SetLanguage(string lang)
    {
        lang = lang?.ToLowerInvariant() ?? "en";
        if (lang != "en" && lang != "ru")
        {
            lang = "en";
        }

        CurrentLanguage = lang;

        try
        {
            var dictUri = new Uri($"pack://application:,,,/OpenAnima;component/Resources/Strings.{lang}.xaml", UriKind.Absolute);
            var newDict = new ResourceDictionary { Source = dictUri };

            var merged = Application.Current.Resources.MergedDictionaries;
            var existing = merged.FirstOrDefault(d => d.Source != null && d.Source.OriginalString.Contains("Strings."));
            if (existing != null)
            {
                int index = merged.IndexOf(existing);
                merged[index] = newDict;
            }
            else
            {
                merged.Add(newDict);
            }

            LanguageChanged?.Invoke();
        }
        catch (Exception ex)
        {
            App.Log($"[LocalizationService.SetLanguage] Error: {ex}");
        }
    }

    public static string GetString(string key, string fallback = "")
    {
        try
        {
            if (Application.Current != null)
            {
                var val = Application.Current.TryFindResource(key);
                if (val is string s)
                {
                    return s;
                }
            }
        }
        catch { }
        return fallback;
    }

    public static string Format(string key, params object[] args)
    {
        string pattern = GetString(key);
        if (string.IsNullOrEmpty(pattern))
        {
            return string.Join(" ", args);
        }
        try
        {
            return string.Format(pattern, args);
        }
        catch
        {
            return pattern;
        }
    }
}

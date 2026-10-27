using System.Collections.Generic;

namespace OpenAnima.UI.Models;

public class AppConfig
{
    public string Language { get; set; } = "en";
    public bool MinimizeToTrayOnClose { get; set; } = true;
    public bool StartMinimized { get; set; } = false;
    public bool AutoStartWithWindows { get; set; } = false;
    public bool RestoreOverlaysOnStartup { get; set; } = true;
    public bool AggressiveMemoryTrimming { get; set; } = true;

    public string ActiveProfileId { get; set; } = "default";
    public bool GlobalHotkeysEnabled { get; set; } = true;
    public List<ProfileModel> Profiles { get; set; } = [];

    public List<AnimationConfigItem> Animations { get; set; } = [];
}

public class AnimationConfigItem
{
    public string FilePath { get; set; } = string.Empty;
    public int X { get; set; } = 200;
    public int Y { get; set; } = 200;
    public double Alpha { get; set; } = 1.0;
    public double Scale { get; set; } = 20.0;
    public bool Smooth { get; set; } = true;
    public bool AlwaysOnTop { get; set; } = true;
    public bool ClickThrough { get; set; } = false;
    public bool Physics { get; set; } = false;
    public bool Bouncy { get; set; } = true;
    public bool ChromaKey { get; set; } = false;
    public string ChromaColor { get; set; } = "#FFFFFF";
    public double ChromaTolerance { get; set; } = 15.0;
    public bool ChromaContiguous { get; set; } = true;
    public bool IsPlaying { get; set; } = false;
}

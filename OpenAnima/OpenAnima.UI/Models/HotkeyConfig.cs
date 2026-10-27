using System.Text.Json.Serialization;

namespace OpenAnima.UI.Models;

public class HotkeyConfig
{
    public uint Modifiers { get; set; }
    public uint VirtualKey { get; set; }
    public string DisplayText { get; set; } = string.Empty;
    public bool IsEnabled { get; set; } = true;

    [JsonIgnore]
    public bool HasConflict { get; set; } = false;
}

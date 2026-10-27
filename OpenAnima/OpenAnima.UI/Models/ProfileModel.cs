using System;
using System.Collections.Generic;

namespace OpenAnima.UI.Models;

public class ProfileModel
{
    public string Id { get; set; } = Guid.NewGuid().ToString("N");
    public string Name { get; set; } = string.Empty;
    public string Icon { get; set; } = "Emoji24";
    public bool IsBuiltIn { get; set; } = false;
    public HotkeyConfig? Hotkey { get; set; }

    // Global Overlay Rules for this profile
    public bool GlobalHideAll { get; set; } = false;
    public bool? ForceClickThrough { get; set; }
    public bool? DisablePhysics { get; set; }
    public double? TargetAlpha { get; set; }
    public double? TargetScale { get; set; }

    // Per-animation overrides keyed by FilePath (case-insensitive)
    public Dictionary<string, AnimationProfileState> AnimationStates { get; set; } = new(StringComparer.OrdinalIgnoreCase);
}

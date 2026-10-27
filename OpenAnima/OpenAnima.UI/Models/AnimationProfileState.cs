namespace OpenAnima.UI.Models;

public class AnimationProfileState
{
    public bool IsActive { get; set; } = true;
    public double? Alpha { get; set; }
    public double? Scale { get; set; }
    public bool? ClickThrough { get; set; }
    public bool? Physics { get; set; }
    public int? X { get; set; }
    public int? Y { get; set; }
}

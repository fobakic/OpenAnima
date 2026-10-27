using System;
using System.Collections.ObjectModel;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using OpenAnima.UI.Models;

namespace OpenAnima.UI.Services;

public class ProfileService
{
    private readonly AnimationService _animationService;
    private readonly SemaphoreSlim _switchLock = new(1, 1);

    public event Action<ProfileModel>? ActiveProfileChanged;

    public ProfileModel? CurrentProfile { get; private set; }

    public ProfileService(AnimationService animationService)
    {
        _animationService = animationService;
    }

    public void Initialize(AppConfig config)
    {
        ConfigService.EnsureDefaultProfiles(config);
        CurrentProfile = config.Profiles.FirstOrDefault(p => p.Id == config.ActiveProfileId) 
                         ?? config.Profiles.FirstOrDefault(p => p.Id == "default")
                         ?? config.Profiles.FirstOrDefault();
    }

    public async Task<bool> SwitchProfileAsync(string profileId, AppConfig config, ObservableCollection<AnimationItem> animations)
    {
        if (!_switchLock.Wait(0))
            return false;

        try
        {
            var target = config.Profiles.FirstOrDefault(p => p.Id.Equals(profileId, StringComparison.OrdinalIgnoreCase));
            if (target == null)
                return false;

            CurrentProfile = target;
            config.ActiveProfileId = target.Id;

            if (target.GlobalHideAll)
            {
                await _animationService.HideAllAsync().ConfigureAwait(false);
            }
            else
            {
                await _animationService.ShowAllAsync().ConfigureAwait(false);

                // Apply effective profile rules to all currently running overlays
                foreach (var anim in animations)
                {
                    if (!anim.IsPlaying || !anim.OverlayId.HasValue)
                        continue;

                    // Resolve per-animation profile state or global rule
                    target.AnimationStates.TryGetValue(anim.FilePath, out var overrideState);

                    bool effectiveClickThrough = target.ForceClickThrough 
                                                 ?? overrideState?.ClickThrough 
                                                 ?? anim.ClickThrough;

                    bool effectivePhysics = target.DisablePhysics == true 
                                            ? false 
                                            : (overrideState?.Physics ?? anim.Physics);

                    // If physics is active, click-through must be disabled so mouse can drag
                    if (effectivePhysics && effectiveClickThrough)
                    {
                        effectiveClickThrough = false;
                    }

                    double effectiveAlpha = target.TargetAlpha 
                                            ?? overrideState?.Alpha 
                                            ?? anim.Alpha;

                    double effectiveScalePct = target.TargetScale 
                                               ?? overrideState?.Scale 
                                               ?? anim.Scale;

                    await _animationService.UpdateAsync(
                        anim.OverlayId.Value,
                        alpha: effectiveAlpha,
                        scale: effectiveScalePct / 100.0,
                        clickThrough: effectiveClickThrough,
                        physics: effectivePhysics,
                        visible: overrideState?.IsActive ?? true
                    ).ConfigureAwait(false);
                }
            }

            ActiveProfileChanged?.Invoke(target);
            return true;
        }
        finally
        {
            _switchLock.Release();
        }
    }
}

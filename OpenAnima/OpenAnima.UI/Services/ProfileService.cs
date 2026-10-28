using System;
using System.Collections.ObjectModel;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using OpenAnima.UI.Models;

namespace OpenAnima.UI.Services;

public class ProfileService
{
    private class BaseItemState
    {
        public double Alpha { get; set; }
        public double Scale { get; set; }
        public bool ClickThrough { get; set; }
        public bool Physics { get; set; }
    }

    private readonly AnimationService _animationService;
    private readonly SemaphoreSlim _switchLock = new(1, 1);
    private readonly Dictionary<string, BaseItemState> _baseSnapshots = new(StringComparer.OrdinalIgnoreCase);

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

            // Safe snapshot of animations collection to prevent cross-thread modification exceptions
            var animsArray = animations.ToArray();

            // If we are transitioning away from default for the first time, snapshot the baseline settings
            if ((CurrentProfile == null || CurrentProfile.Id.Equals("default", StringComparison.OrdinalIgnoreCase)) && _baseSnapshots.Count == 0)
            {
                foreach (var anim in animsArray)
                {
                    _baseSnapshots[anim.FilePath] = new BaseItemState
                    {
                        Alpha = anim.Alpha,
                        Scale = anim.Scale,
                        ClickThrough = anim.ClickThrough,
                        Physics = anim.Physics
                    };
                }
            }

            CurrentProfile = target;
            config.ActiveProfileId = target.Id;

            if (target.GlobalHideAll)
            {
                await _animationService.HideAllAsync().ConfigureAwait(false);
            }
            else
            {
                await _animationService.ShowAllAsync().ConfigureAwait(false);

                bool isDefault = target.Id.Equals("default", StringComparison.OrdinalIgnoreCase);

                if (isDefault)
                {
                    // Restore baseline states to both the UI model and running overlays
                    foreach (var anim in animsArray)
                    {
                        if (_baseSnapshots.TryGetValue(anim.FilePath, out var baseState))
                        {
                            anim.Alpha = baseState.Alpha;
                            anim.Scale = baseState.Scale;
                            anim.ClickThrough = baseState.ClickThrough;
                            anim.Physics = baseState.Physics;
                        }

                        if (anim.IsPlaying && anim.OverlayId.HasValue)
                        {
                            await _animationService.UpdateAsync(
                                anim.OverlayId.Value,
                                alpha: anim.Alpha,
                                scale: anim.Scale / 100.0,
                                clickThrough: anim.ClickThrough,
                                physics: anim.Physics,
                                visible: true
                            ).ConfigureAwait(false);
                        }
                    }

                    _baseSnapshots.Clear();
                }
                else
                {
                    // Apply effective profile rules to running overlays without destroying user baseline
                    foreach (var anim in animsArray)
                    {
                        if (!anim.IsPlaying || !anim.OverlayId.HasValue)
                            continue;

                        target.AnimationStates.TryGetValue(anim.FilePath, out var overrideState);

                        var baseState = _baseSnapshots.TryGetValue(anim.FilePath, out var bs) ? bs : null;
                        double baseAlpha = baseState?.Alpha ?? anim.Alpha;
                        double baseScale = baseState?.Scale ?? anim.Scale;
                        bool baseClickThrough = baseState?.ClickThrough ?? anim.ClickThrough;
                        bool basePhysics = baseState?.Physics ?? anim.Physics;

                        bool effectiveClickThrough = target.ForceClickThrough 
                                                     ?? overrideState?.ClickThrough 
                                                     ?? baseClickThrough;

                        bool effectivePhysics = target.DisablePhysics == true 
                                                ? false 
                                                : (overrideState?.Physics ?? basePhysics);

                        if (effectivePhysics && effectiveClickThrough)
                        {
                            effectiveClickThrough = false;
                        }

                        double effectiveAlpha = target.TargetAlpha 
                                                ?? overrideState?.Alpha 
                                                ?? baseAlpha;

                        double effectiveScalePct = target.TargetScale 
                                                   ?? overrideState?.Scale 
                                                   ?? baseScale;

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

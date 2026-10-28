using System;
using System.Collections.ObjectModel;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using OpenAnima.UI.Models;

namespace OpenAnima.UI.Services;

public class ProfileService
{
    private readonly record struct BaseItemState(double Alpha, double Scale, bool ClickThrough, bool Physics);

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
        CurrentProfile = config.Profiles?.FirstOrDefault(p => p.Id == config.ActiveProfileId) 
                         ?? config.Profiles?.FirstOrDefault(p => p.Id == "default")
                         ?? config.Profiles?.FirstOrDefault();
    }

    /// <summary>
    /// Registers or updates the authoritative baseline state of an animation when edited by the user.
    /// </summary>
    public void NotifyBaseAnimationModified(string filePath, double alpha, double scale, bool clickThrough, bool physics)
    {
        if (CurrentProfile == null || CurrentProfile.Id.Equals("default", StringComparison.OrdinalIgnoreCase))
        {
            _baseSnapshots[filePath] = new BaseItemState(alpha, scale, clickThrough, physics);
        }
    }

    public async Task<bool> SwitchProfileAsync(string profileId, AppConfig config, ObservableCollection<AnimationItem> animations)
    {
        if (!await _switchLock.WaitAsync(1500).ConfigureAwait(false))
            return false;

        try
        {
            var target = config.Profiles?.FirstOrDefault(p => p.Id.Equals(profileId, StringComparison.OrdinalIgnoreCase));
            if (target == null)
                return false;

            // Safe snapshot of animations collection to prevent cross-thread modification exceptions
            var animsArray = animations.ToArray();

            // If we are transitioning away from default for the first time, snapshot the baseline settings
            if ((CurrentProfile == null || CurrentProfile.Id.Equals("default", StringComparison.OrdinalIgnoreCase)) && _baseSnapshots.Count == 0)
            {
                foreach (var anim in animsArray)
                {
                    _baseSnapshots[anim.FilePath] = new BaseItemState(anim.Alpha, anim.Scale, anim.ClickThrough, anim.Physics);
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
                    // Restore baseline states to UI models on the UI thread to prevent thread-affinity exceptions
                    void RestoreUiModels()
                    {
                        foreach (var anim in animsArray)
                        {
                            if (_baseSnapshots.TryGetValue(anim.FilePath, out var baseState))
                            {
                                anim.Alpha = baseState.Alpha;
                                anim.Scale = baseState.Scale;
                                anim.ClickThrough = baseState.ClickThrough;
                                anim.Physics = baseState.Physics;
                            }
                        }
                    }

                    if (System.Windows.Application.Current?.Dispatcher is { } dispatcher && !dispatcher.CheckAccess())
                    {
                        dispatcher.Invoke(RestoreUiModels);
                    }
                    else
                    {
                        RestoreUiModels();
                    }

                    // Parallelize updates to active overlays in Core
                    var updateTasks = new List<Task>();
                    foreach (var anim in animsArray)
                    {
                        if (anim.IsPlaying && anim.OverlayId.HasValue)
                        {
                            updateTasks.Add(_animationService.UpdateAsync(
                                anim.OverlayId.Value,
                                alpha: anim.Alpha,
                                scale: anim.Scale / 100.0,
                                clickThrough: anim.ClickThrough,
                                physics: anim.Physics,
                                visible: true
                            ));
                        }
                    }
                    await Task.WhenAll(updateTasks).ConfigureAwait(false);
                    _baseSnapshots.Clear();
                }
                else
                {
                    // Apply effective profile rules to running overlays without destroying user baseline
                    var updateTasks = new List<Task>();
                    foreach (var anim in animsArray)
                    {
                        if (!anim.IsPlaying || !anim.OverlayId.HasValue)
                            continue;

                        AnimationProfileState? overrideState = null;
                        target.AnimationStates?.TryGetValue(anim.FilePath, out overrideState);

                        var baseState = _baseSnapshots.TryGetValue(anim.FilePath, out var bs) ? bs : default;
                        double baseAlpha = _baseSnapshots.ContainsKey(anim.FilePath) ? baseState.Alpha : anim.Alpha;
                        double baseScale = _baseSnapshots.ContainsKey(anim.FilePath) ? baseState.Scale : anim.Scale;
                        bool baseClickThrough = _baseSnapshots.ContainsKey(anim.FilePath) ? baseState.ClickThrough : anim.ClickThrough;
                        bool basePhysics = _baseSnapshots.ContainsKey(anim.FilePath) ? baseState.Physics : anim.Physics;

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

                        updateTasks.Add(_animationService.UpdateAsync(
                            anim.OverlayId.Value,
                            alpha: effectiveAlpha,
                            scale: effectiveScalePct / 100.0,
                            clickThrough: effectiveClickThrough,
                            physics: effectivePhysics,
                            visible: overrideState?.IsActive ?? true
                        ));
                    }
                    await Task.WhenAll(updateTasks).ConfigureAwait(false);
                }
            }

            if (System.Windows.Application.Current?.Dispatcher is { } disp && !disp.CheckAccess())
            {
                _ = disp.BeginInvoke(() => ActiveProfileChanged?.Invoke(target));
            }
            else
            {
                ActiveProfileChanged?.Invoke(target);
            }

            return true;
        }
        finally
        {
            _switchLock.Release();
        }
    }
}

using System;
using System.Collections.ObjectModel;
using System.IO;
using System.Linq;
using System.Text.Json.Nodes;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using Microsoft.Win32;
using OpenAnima.UI.Models;
using OpenAnima.UI.Services;
using Wpf.Ui.Controls;

namespace OpenAnima.UI;

public partial class MainWindow : Window
{
    private readonly CoreSupervisor _supervisor = new();
    private readonly AnimationService _animationService;
    private readonly TrayService _trayService = new();
    private readonly ConfigService _configService = new();
    private readonly ObservableCollection<AnimationItem> _allAnimations = [];

    public double CardWidth
    {
        get => (double)GetValue(CardWidthProperty);
        set => SetValue(CardWidthProperty, value);
    }

    public static readonly DependencyProperty CardWidthProperty =
        DependencyProperty.Register(nameof(CardWidth), typeof(double), typeof(MainWindow), new PropertyMetadata(340.0));

    private void AnimationsScrollViewer_SizeChanged(object sender, SizeChangedEventArgs e)
    {
        double avail = e.NewSize.Width - 28;
        if (avail < 300) avail = 300;
        int minW = 330;
        int gap = 14;
        int cols = Math.Max(1, (int)((avail + gap) / (minW + gap)));
        double w = Math.Floor((avail - (cols - 1) * gap) / cols);
        CardWidth = Math.Max(280, w);
    }

    private sealed class PendingSliderUpdate
    {
        public long OverlayId { get; set; }
        public double? Alpha { get; set; }
        public double? Scale { get; set; }
        public double? ChromaTolerance { get; set; }
    }

    private readonly System.Collections.Concurrent.ConcurrentDictionary<long, PendingSliderUpdate> _pendingSliderUpdates = new();
    private readonly System.Windows.Threading.DispatcherTimer _sliderThrottleTimer;
    private bool _isSendingSliderUpdate = false;

    private AppConfig _config = new();
    private readonly HotkeyService _hotkeyService = new();
    private ProfileService? _profileService;
    private bool _isUpdatingProfileCombo = false;
    private string _filter = string.Empty;
    private bool _isExiting = false;
    private bool _allOverlaysHidden = false;
    private readonly HashSet<string> _isProcessingPlay = new(StringComparer.OrdinalIgnoreCase);
    private readonly System.Windows.Threading.DispatcherTimer _saveDebounceTimer;

    public MainWindow()
    {
        App.Log("MainWindow.ctor started");
        InitializeComponent();
        App.Log("MainWindow.InitializeComponent finished");
        _animationService = new AnimationService(_supervisor);
        AnimationsItemsControl.ItemsSource = _allAnimations;

        _sliderThrottleTimer = new System.Windows.Threading.DispatcherTimer
        {
            Interval = TimeSpan.FromMilliseconds(20)
        };
        _sliderThrottleTimer.Tick += OnSliderThrottleTick;
        _sliderThrottleTimer.Start();

        _saveDebounceTimer = new System.Windows.Threading.DispatcherTimer
        {
            Interval = TimeSpan.FromMilliseconds(400)
        };
        _saveDebounceTimer.Tick += async (_, _) =>
        {
            _saveDebounceTimer.Stop();
            await SaveConfigAsync();
        };

        _allAnimations.CollectionChanged += (_, _) =>
        {
            EmptyStateBorder.Visibility = _allAnimations.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        };

        LocalizationService.LanguageChanged += OnLanguageChanged;

        Loaded += OnLoaded;
        Closing += OnClosing;
        StateChanged += OnWindowStateChanged;
    }

    private void OnLanguageChanged()
    {
        UpdateActiveBadgeText();
        if (_allOverlaysHidden)
        {
            MenuToggleOverlays.Header = LocalizationService.GetString("Tray_ShowAll");
            BtnToggleAllOverlays.Content = LocalizationService.GetString("Header_ShowAll");
        }
        else
        {
            MenuToggleOverlays.Header = LocalizationService.GetString("Tray_HideAll");
            BtnToggleAllOverlays.Content = LocalizationService.GetString("Header_HideAll");
        }
    }

    private async void OnSliderThrottleTick(object? sender, EventArgs e)
    {
        if (_isSendingSliderUpdate || _pendingSliderUpdates.IsEmpty)
            return;

        _isSendingSliderUpdate = true;
        try
        {
            var keys = _pendingSliderUpdates.Keys.ToList();
            foreach (var key in keys)
            {
                if (_pendingSliderUpdates.TryRemove(key, out var pending))
                {
                    await _animationService.UpdateAsync(
                        pending.OverlayId,
                        alpha: pending.Alpha,
                        scale: pending.Scale,
                        chromaTolerance: pending.ChromaTolerance
                    );
                }
            }
        }
        catch { }
        finally
        {
            _isSendingSliderUpdate = false;
        }
    }

    private void OnWindowStateChanged(object? sender, EventArgs e)
    {
        if (WindowState == WindowState.Minimized)
        {
            if (_config.MinimizeToTrayOnClose)
            {
                Hide();
            }
            if (_config.AggressiveMemoryTrimming)
            {
                MemoryOptimizer.TrimWorkingSet();
            }
        }
    }

    private void QueueSaveConfig()
    {
        _saveDebounceTimer.Stop();
        _saveDebounceTimer.Start();
    }

    private bool _isUpdatingLanguageCombo = false;

    private void CmbLanguage_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_isUpdatingLanguageCombo) return;
        if (CmbLanguage.SelectedItem is ComboBoxItem item && item.Tag is string lang)
        {
            _config.Language = lang;
            LocalizationService.SetLanguage(lang);
            UpdateActiveBadgeText();
            _ = _supervisor.SendCommandAsync("core.set_language", new System.Text.Json.Nodes.JsonObject { ["lang"] = lang });
            _ = RefreshCoreStatusAsync();
            QueueSaveConfig();
        }
    }

    private void UpdateActiveBadgeText()
    {
        int activeCount = _allAnimations.Count(a => a.IsPlaying);
        if (activeCount == 0)
        {
            BadgeActiveCount.Content = LocalizationService.GetString("Active_Count_Zero");
        }
        else if (activeCount == 1)
        {
            BadgeActiveCount.Content = LocalizationService.GetString("Active_Count_Singular");
        }
        else
        {
            BadgeActiveCount.Content = LocalizationService.Format("Active_Count_Plural", activeCount);
        }
        BadgeActiveCount.Appearance = activeCount > 0 ? Wpf.Ui.Controls.ControlAppearance.Success : Wpf.Ui.Controls.ControlAppearance.Secondary;
    }

    private async void OnLoaded(object sender, RoutedEventArgs e)
    {
        App.Log("MainWindow.OnLoaded started");
        try
        {
            // Initialize tray icon
            App.Log("OnLoaded: step 1 - TrayService.Initialize");
            _trayService.Initialize(this, LocalizationService.GetString("App_Title"));
            _trayService.OnTrayDoubleClicked += ToggleWindowVisibility;
            _trayService.OnTrayRightClicked += ShowTrayContextMenu;
            _trayService.OnRequestShowWindow += ShowAndActivateWindow;

            _supervisor.OnBroadcastEventReceived += (eventName, eventData) =>
            {
                if (eventName == "overlay.destroyed" && eventData != null)
                {
                    if (eventData.TryGetPropertyValue("overlay_id", out var idNode) && idNode != null)
                    {
                        long destroyedId = idNode.GetValue<long>();
                        Dispatcher.Invoke(() =>
                        {
                            var target = _allAnimations.FirstOrDefault(a => a.OverlayId == destroyedId);
                            if (target != null)
                            {
                                target.IsPlaying = false;
                                target.OverlayId = null;
                                UpdateActiveBadgeText();
                                QueueSaveConfig();
                            }
                        });
                    }
                }
            };

            _supervisor.OnCoreExited += (exitCode) =>
            {
                Dispatcher.Invoke(() =>
                {
                    foreach (var anim in _allAnimations)
                    {
                        anim.IsPlaying = false;
                        anim.OverlayId = null;
                    }
                    UpdateActiveBadgeText();
                    TxtStatusBar.Text = LocalizationService.CurrentLanguage == "ru"
                        ? $"Ядро оверлеев неожиданно завершилось (код: {exitCode}). Перезапустите ядро в Настройках."
                        : $"Overlay core unexpectedly exited (code: {exitCode}). Restart core in Settings.";
                });
            };

            // Load saved configuration
            App.Log("OnLoaded: step 2 - LoadAsync config");
            _config = await _configService.LoadAsync();
            App.Log($"OnLoaded: step 2 done, language in config: '{_config.Language}'");

            // Set interface language (default: en)
            string lang = !string.IsNullOrWhiteSpace(_config.Language) ? _config.Language : "en";
            _config.Language = lang;
            App.Log($"OnLoaded: step 3 - SetLanguage({lang})");
            LocalizationService.SetLanguage(lang);
            _ = _supervisor.SendCommandAsync("core.set_language", new System.Text.Json.Nodes.JsonObject { ["lang"] = lang });

            App.Log("OnLoaded: step 4 - Select language in combo");
            _isUpdatingLanguageCombo = true;
            try
            {
                foreach (ComboBoxItem item in CmbLanguage.Items)
                {
                    if (item.Tag?.ToString() == lang)
                    {
                        CmbLanguage.SelectedItem = item;
                        break;
                    }
                }
                if (CmbLanguage.SelectedItem == null && CmbLanguage.Items.Count > 0)
                {
                    CmbLanguage.SelectedIndex = 0;
                }
            }
            finally
            {
                _isUpdatingLanguageCombo = false;
            }

            App.Log("OnLoaded: step 5 - Setting checkbox states");
            ChkMinimizeToTray.IsChecked = _config.MinimizeToTrayOnClose;
            ChkStartMinimized.IsChecked = _config.StartMinimized;
            ChkAutoStart.IsChecked = _config.AutoStartWithWindows;
            ChkRestoreOverlays.IsChecked = _config.RestoreOverlaysOnStartup;
            ChkAggressiveMemory.IsChecked = _config.AggressiveMemoryTrimming;
            ChkGlobalHotkeys.IsChecked = _config.GlobalHotkeysEnabled;

            // Initialize profiles and hotkeys
            _profileService = new ProfileService(_animationService);
            _profileService.Initialize(_config);
            UpdateProfileSelectorUi(_config.ActiveProfileId);

            _hotkeyService.Initialize(this);
            _hotkeyService.OnProfileHotkeyPressed += async (profileId) =>
            {
                await Dispatcher.InvokeAsync(async () =>
                {
                    await SwitchProfileInternalAsync(profileId);
                });
            };
            _hotkeyService.RegisterProfileHotkeys(_config.Profiles, _config.GlobalHotkeysEnabled);
            UpdateHotkeyConflictWarning();

            App.Log("OnLoaded: step 6 - RefreshCoreStatusAsync");
            await RefreshCoreStatusAsync();
            App.Log("OnLoaded: step 6 done");

            // Populate animations from saved config or load samples
            App.Log($"OnLoaded: step 7 - Animations count: {_config.Animations.Count}");
            if (_config.Animations.Count > 0)
            {
                foreach (var saved in _config.Animations)
                {
                    if (File.Exists(saved.FilePath))
                    {
                        var item = new AnimationItem
                        {
                            FilePath = saved.FilePath,
                            X = saved.X,
                            Y = saved.Y,
                            Alpha = saved.Alpha,
                            Scale = saved.Scale > 0 ? saved.Scale : 20.0,
                            Smooth = saved.Smooth,
                            AlwaysOnTop = saved.AlwaysOnTop,
                            ClickThrough = saved.ClickThrough,
                            Physics = saved.Physics,
                            Bouncy = saved.Bouncy,
                            ChromaKey = saved.ChromaKey,
                            ChromaColor = !string.IsNullOrEmpty(saved.ChromaColor) ? saved.ChromaColor : "#FFFFFF",
                            ChromaTolerance = saved.ChromaTolerance > 0 ? saved.ChromaTolerance : 15.0,
                            ChromaContiguous = saved.ChromaContiguous
                        };
                        _allAnimations.Add(item);

                        if (_config.RestoreOverlaysOnStartup && saved.IsPlaying)
                        {
                            _ = StartAnimationItemAsync(item);
                        }
                    }
                }
            }
            else
            {
                LoadInitialSampleAnimations();
                _ = SaveConfigAsync();
            }

            await RefreshCoreStatusAsync();

            App.Log("OnLoaded: step 8 - Checking minimized flag");
            // Check if started minimized via command line or config
            string[] args = Environment.GetCommandLineArgs();

            int tabIdx = Array.IndexOf(args, "--tab");
            if (tabIdx >= 0 && tabIdx + 1 < args.Length)
            {
                string tab = args[tabIdx + 1].ToLowerInvariant();
                if (tab == "settings") MainTabControl.SelectedIndex = 3;
                else if (tab == "tools") MainTabControl.SelectedIndex = 2;
                else if (tab == "community") MainTabControl.SelectedIndex = 1;
            }

            int snapIdx = Array.IndexOf(args, "--snapshot");
            if (snapIdx >= 0 && snapIdx + 1 < args.Length)
            {
                string snapPath = args[snapIdx + 1];
                bool exitAfter = args.Contains("--exit-after-snapshot");
                _ = Dispatcher.InvokeAsync(async () =>
                {
                    await Task.Delay(800);
                    SaveWindowSnapshot(snapPath);
                    if (exitAfter)
                    {
                        _isExiting = true;
                        Close();
                    }
                }, System.Windows.Threading.DispatcherPriority.Loaded);
            }

            bool startMin = _config.StartMinimized || args.Any(a => a.Equals("--minimized", StringComparison.OrdinalIgnoreCase) || a.Equals("--tray", StringComparison.OrdinalIgnoreCase));
            if (startMin && snapIdx < 0)
            {
                Hide();
                if (_config.AggressiveMemoryTrimming)
                {
                    MemoryOptimizer.TrimWorkingSet();
                }
            }
            App.Log("OnLoaded: all steps completed successfully!");
        }
        catch (Exception ex)
        {
            App.Log($"MainWindow.OnLoaded error: {ex}");
        }
    }

    private void SaveWindowSnapshot(string targetPath)
    {
        try
        {
            UpdateLayout();
            int w = (int)Math.Max(ActualWidth > 0 ? ActualWidth : Width, 1080);
            int h = (int)Math.Max(ActualHeight > 0 ? ActualHeight : Height, 720);
            var rtb = new RenderTargetBitmap(w, h, 96, 96, PixelFormats.Pbgra32);
            rtb.Render(this);
            var encoder = new PngBitmapEncoder();
            encoder.Frames.Add(BitmapFrame.Create(rtb));
            using var fs = File.Open(targetPath, FileMode.Create, FileAccess.Write, FileShare.None);
            encoder.Save(fs);
            App.Log($"Window snapshot saved to {targetPath}");
        }
        catch (Exception ex)
        {
            App.Log($"SaveWindowSnapshot error: {ex}");
        }
    }

    private async Task RefreshCoreStatusAsync()
    {
        TxtStatusBar.Text = LocalizationService.GetString("Status_Connecting");
        bool connected = await _supervisor.EnsureStartedAndConnectedAsync();

        UpdateActiveBadgeText();

        EmptyStateBorder.Visibility = _allAnimations.Count == 0 ? Visibility.Visible : Visibility.Collapsed;

        if (connected)
        {
            var stats = await _animationService.GetStatsAsync();
            long pid = stats?["pid"]?.GetValue<long>() ?? 0;
            double coreWs = stats?["working_set_mb"]?.GetValue<double>() ?? 0;
            double uiWs = System.Diagnostics.Process.GetCurrentProcess().WorkingSet64 / (1024.0 * 1024.0);
            TxtStatusBar.Text = LocalizationService.Format("Status_CoreRunning_Format", coreWs, pid, uiWs);
            TxtCoreStatus.Text = LocalizationService.Format("Status_CoreActive_Format", pid, coreWs, uiWs);

            var activeList = await _animationService.ListOverlaysAsync();
            if (activeList != null)
            {
                var activeIds = new HashSet<long>();
                foreach (var node in activeList)
                {
                    if (node?["id"]?.GetValue<long>() is long id)
                    {
                        activeIds.Add(id);
                    }
                }
                bool changed = false;
                foreach (var anim in _allAnimations)
                {
                    if (anim.IsPlaying && anim.OverlayId.HasValue && !activeIds.Contains(anim.OverlayId.Value))
                    {
                        anim.IsPlaying = false;
                        anim.OverlayId = null;
                        changed = true;
                    }
                }
                if (changed)
                {
                    UpdateActiveBadgeText();
                    QueueSaveConfig();
                }
            }
        }
        else
        {
            TxtStatusBar.Text = LocalizationService.GetString("Status_CoreError");
            TxtCoreStatus.Text = LocalizationService.GetString("Status_CoreUnavailable");
        }
    }

    private void LoadInitialSampleAnimations()
    {
        string baseDir = AppDomain.CurrentDomain.BaseDirectory;
        string[] searchDirs =
        [
            Path.GetFullPath(Path.Combine(baseDir, @"..\..\..\..\..\_legacy_reference\AnimaEngine\Models")),
            Path.GetFullPath(Path.Combine(baseDir, @"..\..\..\..\_legacy_reference\AnimaEngine\Models")),
            Path.GetFullPath(Path.Combine(baseDir, @"_legacy_reference\AnimaEngine\Models"))
        ];

        foreach (var dir in searchDirs)
        {
            if (Directory.Exists(dir))
            {
                var files = Directory.GetFiles(dir, "*.gif").Take(8);
                foreach (var file in files)
                {
                    AddAnimationFile(file);
                }
                break;
            }
        }
    }

    private void AddAnimationFile(string path)
    {
        if (_allAnimations.Any(a => a.FilePath.Equals(path, StringComparison.OrdinalIgnoreCase)))
            return;

        _allAnimations.Add(new AnimationItem
        {
            FilePath = path,
            Alpha = 1.0,
            Scale = 20.0,
            Smooth = true,
            AlwaysOnTop = true,
            ClickThrough = false
        });
    }

    private void BtnAddAnimation_Click(object sender, RoutedEventArgs e)
    {
        var dialog = new OpenFileDialog
        {
            Title = LocalizationService.GetString("Dialog_SelectAnimation_Title"),
            Filter = LocalizationService.GetString("Dialog_SelectAnimation_Filter"),
            Multiselect = true
        };

        if (dialog.ShowDialog() == true)
        {
            foreach (var file in dialog.FileNames)
            {
                AddAnimationFile(file);
            }
            _ = SaveConfigAsync();
        }
    }

    private void BtnLoadSamples_Click(object sender, RoutedEventArgs e)
    {
        LoadInitialSampleAnimations();
        _ = SaveConfigAsync();
    }

    private async Task StartAnimationItemAsync(AnimationItem item)
    {
        long? id = await _animationService.PlayAsync(
            item.FilePath,
            x: item.X,
            y: item.Y,
            fps: 20,
            alpha: item.Alpha,
            scale: item.Scale / 100.0,
            smooth: item.Smooth,
            alwaysOnTop: item.AlwaysOnTop,
            clickThrough: item.ClickThrough,
            physics: item.Physics,
            bouncy: item.Bouncy,
            chromaKey: item.ChromaKey,
            chromaColor: item.ChromaColor,
            chromaTolerance: item.ChromaTolerance / 100.0,
            chromaContiguous: item.ChromaContiguous
        );

        if (id.HasValue)
        {
            item.OverlayId = id.Value;
            item.IsPlaying = true;
        }

        await RefreshCoreStatusAsync();
    }

    private async void BtnTogglePlay_Click(object sender, RoutedEventArgs e)
    {
        if (sender is not FrameworkElement fe || fe.Tag is not AnimationItem item)
            return;

        if (_isProcessingPlay.Contains(item.FilePath))
            return; // Ignore rapid spam clicks

        _isProcessingPlay.Add(item.FilePath);
        fe.IsEnabled = false;

        try
        {
            if (item.IsPlaying && item.OverlayId.HasValue)
            {
                await _animationService.StopAsync(item.OverlayId.Value);
                item.IsPlaying = false;
                item.OverlayId = null;
            }
            else
            {
                await StartAnimationItemAsync(item);
            }

            await RefreshCoreStatusAsync();
            QueueSaveConfig();
        }
        catch (Exception ex)
        {
            App.Log($"BtnTogglePlay_Click error: {ex}");
        }
        finally
        {
            _isProcessingPlay.Remove(item.FilePath);
            fe.IsEnabled = true;
        }
    }

    private async void BtnStopAll_Click(object sender, RoutedEventArgs e)
    {
        await _animationService.StopAllAsync();
        foreach (var item in _allAnimations)
        {
            item.IsPlaying = false;
            item.OverlayId = null;
        }

        await RefreshCoreStatusAsync();
        QueueSaveConfig();
    }

    private void SliderAlpha_ValueChanged(object sender, RoutedPropertyChangedEventArgs<double> e)
    {
        if (sender is FrameworkElement { Tag: AnimationItem item } && item.IsPlaying && item.OverlayId.HasValue)
        {
            _pendingSliderUpdates.AddOrUpdate(
                item.OverlayId.Value,
                id => new PendingSliderUpdate { OverlayId = id, Alpha = e.NewValue },
                (id, p) => { p.Alpha = e.NewValue; return p; }
            );
            QueueSaveConfig();
        }
    }

    private void SliderScale_ValueChanged(object sender, RoutedPropertyChangedEventArgs<double> e)
    {
        if (sender is FrameworkElement { Tag: AnimationItem item } && item.IsPlaying && item.OverlayId.HasValue)
        {
            _pendingSliderUpdates.AddOrUpdate(
                item.OverlayId.Value,
                id => new PendingSliderUpdate { OverlayId = id, Scale = e.NewValue / 100.0 },
                (id, p) => { p.Scale = e.NewValue / 100.0; return p; }
            );
            QueueSaveConfig();
        }
    }

    private async void OnSettingChanged(object sender, RoutedEventArgs e)
    {
        if (sender is FrameworkElement { Tag: AnimationItem item })
        {
            if (item.Physics && item.ClickThrough)
            {
                item.ClickThrough = false;
            }

            if (item.IsPlaying && item.OverlayId.HasValue)
            {
                await _animationService.UpdateAsync(
                    item.OverlayId.Value,
                    scale: item.Scale / 100.0,
                    smooth: item.Smooth,
                    alwaysOnTop: item.AlwaysOnTop,
                    clickThrough: item.ClickThrough,
                    physics: item.Physics,
                    bouncy: item.Bouncy,
                    chromaKey: item.ChromaKey,
                    chromaColor: item.ChromaColor,
                    chromaTolerance: item.ChromaTolerance / 100.0,
                    chromaContiguous: item.ChromaContiguous
                );
            }
            QueueSaveConfig();
        }
    }

    private void SliderChromaTolerance_ValueChanged(object sender, RoutedPropertyChangedEventArgs<double> e)
    {
        if (sender is FrameworkElement { Tag: AnimationItem item } && item.IsPlaying && item.OverlayId.HasValue)
        {
            _pendingSliderUpdates.AddOrUpdate(
                item.OverlayId.Value,
                id => new PendingSliderUpdate { OverlayId = id, ChromaTolerance = e.NewValue / 100.0 },
                (id, p) => { p.ChromaTolerance = e.NewValue / 100.0; return p; }
            );
            QueueSaveConfig();
        }
    }

    private async void ChromaColor_TextChanged(object sender, TextChangedEventArgs e)
    {
        if (sender is FrameworkElement { Tag: AnimationItem item } && item.IsPlaying && item.OverlayId.HasValue)
        {
            string color = item.ChromaColor?.Trim() ?? "";
            if (System.Text.RegularExpressions.Regex.IsMatch(color, "^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$"))
            {
                if (color.Length == 4)
                {
                    color = $"#{color[1]}{color[1]}{color[2]}{color[2]}{color[3]}{color[3]}";
                }
                await _animationService.UpdateAsync(item.OverlayId.Value, chromaColor: color);
                QueueSaveConfig();
            }
        }
    }

    private async void BtnEyedropper_Click(object sender, RoutedEventArgs e)
    {
        if (sender is not FrameworkElement fe || fe.Tag is not AnimationItem item)
            return;

        try
        {
            // Auto-sample top-left corner pixel (0, 0) of the media file
            if (File.Exists(item.FilePath))
            {
                using var stream = File.OpenRead(item.FilePath);
                var decoder = BitmapDecoder.Create(stream, BitmapCreateOptions.None, BitmapCacheOption.OnLoad);
                if (decoder.Frames.Count > 0)
                {
                    var frame = decoder.Frames[0];
                    var converted = new FormatConvertedBitmap(frame, PixelFormats.Bgra32, null, 0);
                    var bgra = new byte[4];
                    converted.CopyPixels(new Int32Rect(0, 0, 1, 1), bgra, 4, 0);
                    string hex = $"#{bgra[2]:X2}{bgra[1]:X2}{bgra[0]:X2}";
                    item.ChromaColor = hex;
                    if (item.IsPlaying && item.OverlayId.HasValue)
                    {
                        await _animationService.UpdateAsync(item.OverlayId.Value, chromaColor: hex);
                    }
                    QueueSaveConfig();
                    TxtStatusBar.Text = LocalizationService.Format("Status_EyedropperCorner", hex);
                }
            }
        }
        catch (Exception ex)
        {
            App.Log($"[BtnEyedropper_Click] Error: {ex.Message}");
        }
    }

    private void Window_DragOver(object sender, DragEventArgs e)
    {
        if (e.Data.GetDataPresent(DataFormats.FileDrop))
        {
            e.Effects = DragDropEffects.Copy;
            e.Handled = true;
        }
        else
        {
            e.Effects = DragDropEffects.None;
        }
    }

    private async void Window_Drop(object sender, DragEventArgs e)
    {
        if (e.Data.GetDataPresent(DataFormats.FileDrop))
        {
            if (e.Data.GetData(DataFormats.FileDrop) is string[] files && files.Length > 0)
            {
                var mp4File = files.FirstOrDefault(f => Path.GetExtension(f).Equals(".mp4", StringComparison.OrdinalIgnoreCase));
                if (mp4File != null)
                {
                    OpenMp4Converter(mp4File);
                    return;
                }

                var supported = new[] { ".gif", ".png", ".apng", ".webp" };
                int added = 0;
                foreach (var file in files)
                {
                    string ext = Path.GetExtension(file).ToLowerInvariant();
                    if (supported.Contains(ext) && !_allAnimations.Any(a => a.FilePath.Equals(file, StringComparison.OrdinalIgnoreCase)))
                    {
                        var item = new AnimationItem
                        {
                            FilePath = file,
                            Scale = 20.0,
                            Alpha = 1.0,
                            Smooth = true,
                            AlwaysOnTop = true
                        };
                        _allAnimations.Add(item);
                        added++;
                    }
                }
                if (added > 0)
                {
                    await SaveConfigAsync();
                    await RefreshCoreStatusAsync();
                    TxtStatusBar.Text = LocalizationService.Format("Status_FilesAdded", added);
                }
            }
        }
    }

    private void BtnOpenMp4Converter_Click(object sender, RoutedEventArgs e)
    {
        OpenMp4Converter();
    }

    private void OpenMp4Converter(string? initialPath = null)
    {
        var dlg = new OpenAnima.UI.Views.Mp4ConverterDialog(_animationService, _supervisor, initialPath)
        {
            Owner = this
        };
        if (dlg.ShowDialog() == true && !string.IsNullOrEmpty(dlg.ConvertedApngPath) && File.Exists(dlg.ConvertedApngPath))
        {
            AddAnimationFile(dlg.ConvertedApngPath);
            _ = SaveConfigAsync();
            TxtStatusBar.Text = LocalizationService.Format("Convert_CompletedFormat", Path.GetFileName(dlg.ConvertedApngPath));
        }
    }

    private void BtnRevealInFolder_Click(object sender, RoutedEventArgs e)
    {
        if (sender is FrameworkElement { Tag: AnimationItem item } && File.Exists(item.FilePath))
        {
            try
            {
                System.Diagnostics.Process.Start("explorer.exe", $"/select,\"{item.FilePath}\"");
            }
            catch (Exception ex)
            {
                App.Log($"[BtnRevealInFolder] Error: {ex.Message}");
            }
        }
    }

    private async void BtnDeleteAnimation_Click(object sender, RoutedEventArgs e)
    {
        if (sender is FrameworkElement { Tag: AnimationItem item })
        {
            if (item.IsPlaying && item.OverlayId.HasValue)
            {
                await _animationService.StopAsync(item.OverlayId.Value);
                item.IsPlaying = false;
            }
            _allAnimations.Remove(item);
            await SaveConfigAsync();
            await RefreshCoreStatusAsync();
            TxtStatusBar.Text = LocalizationService.Format("Status_AnimationRemoved", item.Name);
        }
    }

    private void BtnOpenGitHub_Click(object sender, RoutedEventArgs e)
    {
        try
        {
            System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("https://github.com/fobakic/OpenAnima")
            {
                UseShellExecute = true
            });
        }
        catch { }
    }

    private void BtnGoToLibrary_Click(object sender, RoutedEventArgs e)
    {
        MainTabControl.SelectedItem = TabLibrary;
    }

    private void TxtSearch_TextChanged(object sender, TextChangedEventArgs e)
    {
        _filter = TxtSearch.Text.Trim();
        if (string.IsNullOrEmpty(_filter))
        {
            AnimationsItemsControl.ItemsSource = _allAnimations;
            EmptyStateBorder.Visibility = _allAnimations.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        }
        else
        {
            var filtered = _allAnimations
                .Where(a => a.Name.Contains(_filter, StringComparison.OrdinalIgnoreCase))
                .ToList();
            AnimationsItemsControl.ItemsSource = filtered;
            EmptyStateBorder.Visibility = filtered.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        }
    }

    private async void BtnRestartCore_Click(object sender, RoutedEventArgs e)
    {
        TxtStatusBar.Text = LocalizationService.GetString("Status_RestartingCore");
        await _supervisor.RestartCoreAsync();
        await RefreshCoreStatusAsync();
    }

    private async void BtnTrimMemory_Click(object sender, RoutedEventArgs e)
    {
        MemoryOptimizer.TrimWorkingSet();
        await _animationService.TrimMemoryAsync();
        await RefreshCoreStatusAsync();
    }

    #region Configuration & Profile Management
    private async Task SyncOverlayPositionsAsync()
    {
        try
        {
            var list = await _animationService.ListOverlaysAsync().ConfigureAwait(false);
            if (list == null) return;

            void ApplyUpdates()
            {
                foreach (var node in list)
                {
                    if (node is JsonObject obj)
                    {
                        long id = obj["overlay_id"]?.GetValue<long>() ?? 0;
                        int x = obj["x"]?.GetValue<int>() ?? 0;
                        int y = obj["y"]?.GetValue<int>() ?? 0;
                        double? scaleVal = obj["scale"]?.GetValue<double>();
                        bool? physVal = obj["physics"]?.GetValue<bool>();

                        var item = _allAnimations.FirstOrDefault(a => a.OverlayId == id);
                        if (item != null)
                        {
                            item.X = x;
                            item.Y = y;
                            if (scaleVal.HasValue)
                            {
                                item.Scale = Math.Round(scaleVal.Value * 100.0, 1);
                            }
                            if (physVal.HasValue)
                            {
                                item.Physics = physVal.Value;
                            }
                        }
                    }
                }
            }

            if (Dispatcher.CheckAccess())
            {
                ApplyUpdates();
            }
            else
            {
                Dispatcher.Invoke(ApplyUpdates);
            }
        }
        catch (Exception ex)
        {
            App.Log($"[SyncOverlayPositionsAsync] Error: {ex.Message}");
        }
    }

    private void SyncOverlayPositionsSync(int timeoutMs = 1000)
    {
        try
        {
            var listTask = Task.Run(async () => await _animationService.ListOverlaysAsync().ConfigureAwait(false));
            if (!listTask.Wait(timeoutMs))
                return;

            var list = listTask.Result;
            if (list == null) return;

            foreach (var node in list)
            {
                if (node is JsonObject obj)
                {
                    long id = obj["overlay_id"]?.GetValue<long>() ?? 0;
                    int x = obj["x"]?.GetValue<int>() ?? 0;
                    int y = obj["y"]?.GetValue<int>() ?? 0;
                    double? scaleVal = obj["scale"]?.GetValue<double>();
                    bool? physVal = obj["physics"]?.GetValue<bool>();

                    var item = _allAnimations.FirstOrDefault(a => a.OverlayId == id);
                    if (item != null)
                    {
                        item.X = x;
                        item.Y = y;
                        if (scaleVal.HasValue)
                        {
                            item.Scale = Math.Round(scaleVal.Value * 100.0, 1);
                        }
                        if (physVal.HasValue)
                        {
                            item.Physics = physVal.Value;
                        }
                    }
                }
            }
        }
        catch (Exception ex)
        {
            App.Log($"[SyncOverlayPositionsSync] Error: {ex.Message}");
        }
    }

    private async Task SaveConfigAsync()
    {
        await SyncOverlayPositionsAsync();

        _config.MinimizeToTrayOnClose = ChkMinimizeToTray.IsChecked == true;
        _config.StartMinimized = ChkStartMinimized.IsChecked == true;
        _config.AutoStartWithWindows = ChkAutoStart.IsChecked == true;
        _config.RestoreOverlaysOnStartup = ChkRestoreOverlays.IsChecked == true;
        _config.AggressiveMemoryTrimming = ChkAggressiveMemory.IsChecked == true;
        _config.GlobalHotkeysEnabled = ChkGlobalHotkeys.IsChecked == true;
        _config.ActiveProfileId = _profileService?.CurrentProfile?.Id ?? _config.ActiveProfileId;

        _config.Animations = _allAnimations.Select(a => new AnimationConfigItem
        {
            FilePath = a.FilePath,
            X = a.X,
            Y = a.Y,
            Alpha = a.Alpha,
            Scale = a.Scale,
            Smooth = a.Smooth,
            AlwaysOnTop = a.AlwaysOnTop,
            ClickThrough = a.ClickThrough,
            Physics = a.Physics,
            Bouncy = a.Bouncy,
            ChromaKey = a.ChromaKey,
            ChromaColor = a.ChromaColor,
            ChromaTolerance = a.ChromaTolerance,
            ChromaContiguous = a.ChromaContiguous,
            IsPlaying = a.IsPlaying
        }).ToList();

        await _configService.SaveAsync(_config);
    }

    private void SaveConfigSync()
    {
        try
        {
            _config.MinimizeToTrayOnClose = ChkMinimizeToTray.IsChecked == true;
            _config.StartMinimized = ChkStartMinimized.IsChecked == true;
            _config.AutoStartWithWindows = ChkAutoStart.IsChecked == true;
            _config.RestoreOverlaysOnStartup = ChkRestoreOverlays.IsChecked == true;
            _config.AggressiveMemoryTrimming = ChkAggressiveMemory.IsChecked == true;
            _config.GlobalHotkeysEnabled = ChkGlobalHotkeys.IsChecked == true;
            _config.ActiveProfileId = _profileService?.CurrentProfile?.Id ?? _config.ActiveProfileId;

            _config.Animations = _allAnimations.Select(a => new AnimationConfigItem
            {
                FilePath = a.FilePath,
                X = a.X,
                Y = a.Y,
                Alpha = a.Alpha,
                Scale = a.Scale,
                Smooth = a.Smooth,
                AlwaysOnTop = a.AlwaysOnTop,
                ClickThrough = a.ClickThrough,
                Physics = a.Physics,
                Bouncy = a.Bouncy,
                ChromaKey = a.ChromaKey,
                ChromaColor = a.ChromaColor,
                ChromaTolerance = a.ChromaTolerance,
                ChromaContiguous = a.ChromaContiguous,
                IsPlaying = a.IsPlaying
            }).ToList();

            _configService.SaveSync(_config);
        }
        catch (Exception ex)
        {
            App.Log($"[SaveConfigSync] Error: {ex.Message}");
        }
    }

    private async void OnConfigSettingChanged(object sender, RoutedEventArgs e)
    {
        await SaveConfigAsync();
    }

    private async void BtnSaveConfig_Click(object sender, RoutedEventArgs e)
    {
        await SaveConfigAsync();
        TxtSaveStatus.Text = LocalizationService.Format("Status_Saved", DateTime.Now.ToString("HH:mm:ss"));
    }

    #region Profiles & Hotkeys
    private async Task SwitchProfileInternalAsync(string profileId)
    {
        if (_profileService == null) return;
        bool switched = await _profileService.SwitchProfileAsync(profileId, _config, _allAnimations);
        if (switched)
        {
            UpdateProfileSelectorUi(profileId);
            QueueSaveConfig();
            TxtStatusBar.Text = LocalizationService.CurrentLanguage == "ru"
                ? $"Активен профиль: {_profileService.CurrentProfile?.Name}"
                : $"Active profile: {_profileService.CurrentProfile?.Name}";
        }
    }

    private void UpdateProfileSelectorUi(string profileId)
    {
        _isUpdatingProfileCombo = true;
        try
        {
            foreach (ComboBoxItem item in CmbActiveProfile.Items)
            {
                if (item.Tag?.ToString()?.Equals(profileId, StringComparison.OrdinalIgnoreCase) == true)
                {
                    CmbActiveProfile.SelectedItem = item;
                    break;
                }
            }
        }
        finally
        {
            _isUpdatingProfileCombo = false;
        }

        MenuProfileDefault.IsChecked = profileId.Equals("default", StringComparison.OrdinalIgnoreCase);
        MenuProfileGaming.IsChecked = profileId.Equals("gaming", StringComparison.OrdinalIgnoreCase);
        MenuProfileWork.IsChecked = profileId.Equals("work", StringComparison.OrdinalIgnoreCase);
        MenuProfileChill.IsChecked = profileId.Equals("chill", StringComparison.OrdinalIgnoreCase);
    }

    private async void CmbActiveProfile_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_isUpdatingProfileCombo) return;
        if (CmbActiveProfile.SelectedItem is ComboBoxItem item && item.Tag is string profileId)
        {
            await SwitchProfileInternalAsync(profileId);
        }
    }

    private async void ProfileMenuItem_Click(object sender, RoutedEventArgs e)
    {
        if (sender is System.Windows.Controls.MenuItem mi && mi.Tag is string profileId)
        {
            await SwitchProfileInternalAsync(profileId);
        }
    }

    private void ChkGlobalHotkeys_Click(object sender, RoutedEventArgs e)
    {
        bool isEnabled = ChkGlobalHotkeys.IsChecked == true;
        _config.GlobalHotkeysEnabled = isEnabled;
        _hotkeyService.RegisterProfileHotkeys(_config.Profiles, isEnabled);
        UpdateHotkeyConflictWarning();
        QueueSaveConfig();
    }

    private void UpdateHotkeyConflictWarning()
    {
        var conflicting = _config.Profiles.Where(p => p.Hotkey?.HasConflict == true).ToList();
        if (conflicting.Count > 0)
        {
            string hotkeyNames = string.Join(", ", conflicting.Select(p => p.Hotkey?.DisplayText));
            TxtHotkeyConflict.Text = LocalizationService.Format("Settings_Hotkeys_Conflict", hotkeyNames);
            TxtHotkeyConflict.Visibility = Visibility.Visible;
        }
        else
        {
            TxtHotkeyConflict.Visibility = Visibility.Collapsed;
        }
    }
    #endregion
    #endregion

    #region System Tray & Window Lifecycle
    private void ShowTrayContextMenu()
    {
        var helper = new WindowInteropHelper(this);
        TrayService.SetForegroundWindow(helper.Handle);
        TrayContextMenu.IsOpen = true;
    }

    private void ToggleWindowVisibility()
    {
        if (IsVisible && WindowState != WindowState.Minimized)
        {
            Hide();
            if (_config.AggressiveMemoryTrimming)
            {
                MemoryOptimizer.TrimWorkingSet();
            }
        }
        else
        {
            ShowAndActivateWindow();
        }
    }

    private void ShowAndActivateWindow()
    {
        Show();
        if (WindowState == WindowState.Minimized)
        {
            WindowState = WindowState.Normal;
        }
        Activate();
        Focus();
    }

    private void MenuOpen_Click(object sender, RoutedEventArgs e)
    {
        ShowAndActivateWindow();
    }

    private async void MenuToggleOverlays_Click(object sender, RoutedEventArgs e)
    {
        _allOverlaysHidden = !_allOverlaysHidden;
        if (_allOverlaysHidden)
        {
            await _animationService.HideAllAsync();
            MenuToggleOverlays.Header = LocalizationService.GetString("Tray_ShowAll");
            BtnToggleAllOverlays.Content = LocalizationService.GetString("Header_ShowAll");
            BtnToggleAllOverlays.Icon = new SymbolIcon(SymbolRegular.Eye24);
            TxtStatusBar.Text = LocalizationService.GetString("Status_AllHidden");
        }
        else
        {
            await _animationService.ShowAllAsync();
            MenuToggleOverlays.Header = LocalizationService.GetString("Tray_HideAll");
            BtnToggleAllOverlays.Content = LocalizationService.GetString("Header_HideAll");
            BtnToggleAllOverlays.Icon = new SymbolIcon(SymbolRegular.EyeOff24);
            TxtStatusBar.Text = LocalizationService.GetString("Status_AllShown");
        }
    }

    private void MenuExit_Click(object sender, RoutedEventArgs e)
    {
        _isExiting = true;
        Close();
    }

    private void OnClosing(object? sender, System.ComponentModel.CancelEventArgs e)
    {
        LocalizationService.LanguageChanged -= OnLanguageChanged;

        if (!_isExiting && _config.MinimizeToTrayOnClose)
        {
            e.Cancel = true;
            Hide();
            _ = SaveConfigAsync();
            if (_config.AggressiveMemoryTrimming)
            {
                MemoryOptimizer.TrimWorkingSet();
            }
            return;
        }

        _saveDebounceTimer.Stop();
        _sliderThrottleTimer.Stop();
        OnSliderThrottleTick(null, EventArgs.Empty);

        SyncOverlayPositionsSync(1000);
        SaveConfigSync();
        _hotkeyService.Dispose();
        _trayService.Dispose();
        _supervisor.Dispose();
    }
    #endregion
}

using System;
using System.IO;
using System.Text.Json.Nodes;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Controls;
using Microsoft.Win32;
using OpenAnima.UI.Services;

namespace OpenAnima.UI.Views;

public partial class Mp4ConverterDialog : Window
{
    private readonly AnimationService _animationService;
    private readonly CoreSupervisor _supervisor;
    private string? _currentJobId;
    private bool _isConverting = false;

    public string? ConvertedApngPath { get; private set; }

    public Mp4ConverterDialog(AnimationService animationService, CoreSupervisor supervisor, string? initialInputPath = null)
    {
        InitializeComponent();
        _animationService = animationService;
        _supervisor = supervisor;

        if (!string.IsNullOrEmpty(initialInputPath) && File.Exists(initialInputPath))
        {
            TxtInputPath.Text = initialInputPath;
        }

        _supervisor.OnBroadcastEventReceived += OnBroadcastEvent;
        Closing += (_, _) => _supervisor.OnBroadcastEventReceived -= OnBroadcastEvent;
    }

    private void TxtInputPath_TextChanged(object sender, TextChangedEventArgs e)
    {
        string input = TxtInputPath.Text.Trim();
        if (!string.IsNullOrEmpty(input) && string.IsNullOrEmpty(TxtOutputPath.Text))
        {
            try
            {
                TxtOutputPath.Text = Path.ChangeExtension(input, ".apng");
            }
            catch { }
        }
    }

    private void BtnBrowseInput_Click(object sender, RoutedEventArgs e)
    {
        var dlg = new OpenFileDialog
        {
            Filter = "MP4 Videos (*.mp4)|*.mp4|All files (*.*)|*.*",
            Title = "Select MP4 Video"
        };
        if (dlg.ShowDialog() == true)
        {
            TxtInputPath.Text = dlg.FileName;
            TxtOutputPath.Text = Path.ChangeExtension(dlg.FileName, ".apng");
        }
    }

    private void BtnBrowseOutput_Click(object sender, RoutedEventArgs e)
    {
        var dlg = new SaveFileDialog
        {
            Filter = "APNG Animation (*.apng)|*.apng|PNG Image (*.png)|*.png",
            Title = "Save APNG As"
        };
        if (dlg.ShowDialog() == true)
        {
            TxtOutputPath.Text = dlg.FileName;
        }
    }

    private void SliderFps_ValueChanged(object sender, RoutedPropertyChangedEventArgs<double> e)
    {
        if (TxtFpsValue != null)
        {
            TxtFpsValue.Text = $"{(int)e.NewValue} FPS";
        }
    }

    private void SliderTolerance_ValueChanged(object sender, RoutedPropertyChangedEventArgs<double> e)
    {
        if (TxtToleranceValue != null)
        {
            TxtToleranceValue.Text = $"{(int)e.NewValue}%";
        }
    }

    private void ChkChromaKey_Changed(object sender, RoutedEventArgs e)
    {
        if (PnlChromaOptions != null)
        {
            PnlChromaOptions.Visibility = ChkChromaKey.IsChecked == true ? Visibility.Visible : Visibility.Collapsed;
        }
    }

    private async void BtnStart_Click(object sender, RoutedEventArgs e)
    {
        string inPath = TxtInputPath.Text.Trim();
        if (string.IsNullOrEmpty(inPath) || !File.Exists(inPath))
        {
            TxtStatus.Text = LocalizationService.CurrentLanguage == "ru"
                ? "Пожалуйста, укажите существующий файл MP4."
                : "Please select a valid MP4 file.";
            return;
        }

        string outPath = TxtOutputPath.Text.Trim();
        if (string.IsNullOrEmpty(outPath))
        {
            outPath = Path.ChangeExtension(inPath, ".apng");
            TxtOutputPath.Text = outPath;
        }

        int fps = (int)SliderFps.Value;
        int maxDim = 384;
        if (CmbMaxSize.SelectedItem is ComboBoxItem item && int.TryParse(item.Tag?.ToString(), out int parsedDim))
        {
            maxDim = parsedDim;
        }

        bool chromaKey = ChkChromaKey.IsChecked == true;
        string chromaColor = TxtChromaColor.Text.Trim();
        if (string.IsNullOrEmpty(chromaColor)) chromaColor = "#FFFFFF";
        double chromaTolerance = SliderTolerance.Value / 100.0;
        bool chromaContiguous = ChkChromaContiguous.IsChecked == true;

        _currentJobId = "job_" + DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        _isConverting = true;
        App.Log($"[Mp4ConverterDialog] Starting conversion job '{_currentJobId}': '{inPath}' -> '{outPath}' (fps: {fps}, maxDim: {maxDim}, chroma: {chromaKey})");

        BtnStart.IsEnabled = false;
        TxtInputPath.IsEnabled = false;
        TxtOutputPath.IsEnabled = false;
        SliderFps.IsEnabled = false;
        CmbMaxSize.IsEnabled = false;
        ChkChromaKey.IsEnabled = false;
        PnlChromaOptions.IsEnabled = false;

        PrgConversion.Value = 0;
        TxtStatus.Text = LocalizationService.Format("Convert_Converting", 0);

        bool started = await _animationService.ConvertStartAsync(
            inPath,
            outPath,
            fps: fps,
            maxDim: maxDim,
            chromaKey: chromaKey,
            chromaColor: chromaColor,
            chromaTolerance: chromaTolerance,
            chromaContiguous: chromaContiguous,
            jobId: _currentJobId
        );

        if (!started)
        {
            _isConverting = false;
            ResetControlsState();
            TxtStatus.Text = LocalizationService.CurrentLanguage == "ru"
                ? "Не удалось запустить конвертацию ядра."
                : "Failed to initiate conversion in core.";
        }
    }

    private async void BtnCancel_Click(object sender, RoutedEventArgs e)
    {
        if (_isConverting)
        {
            TxtStatus.Text = LocalizationService.GetString("Convert_Cancelling");
            await _animationService.ConvertCancelAsync(_currentJobId);
        }
        else
        {
            Close();
        }
    }

    private void ResetControlsState()
    {
        BtnStart.IsEnabled = true;
        TxtInputPath.IsEnabled = true;
        TxtOutputPath.IsEnabled = true;
        SliderFps.IsEnabled = true;
        CmbMaxSize.IsEnabled = true;
        ChkChromaKey.IsEnabled = true;
        PnlChromaOptions.IsEnabled = true;
    }

    private void OnBroadcastEvent(string eventName, JsonObject? data)
    {
        App.Log($"[Mp4ConverterDialog] Received broadcast event '{eventName}': {data?.ToJsonString()}");
        Dispatcher.InvokeAsync(() =>
        {
            if (data == null) return;
            string? jobId = data["job_id"]?.GetValue<string>();
            if (!string.IsNullOrEmpty(_currentJobId) && jobId != _currentJobId)
                return;

            switch (eventName)
            {
                case "convert.progress":
                    double pct = data["percent"]?.GetValue<double>() ?? 0;
                    PrgConversion.Value = Math.Clamp(pct, 0, 100);
                    TxtStatus.Text = LocalizationService.Format("Convert_Converting", pct);
                    break;

                case "convert.completed":
                    _isConverting = false;
                    PrgConversion.Value = 100;
                    string outPath = data["out_path"]?.GetValue<string>() ?? TxtOutputPath.Text;
                    ConvertedApngPath = outPath;
                    TxtStatus.Text = LocalizationService.GetString("Convert_Success") + " " + LocalizationService.Format("Convert_CompletedFormat", Path.GetFileName(outPath));
                    ResetControlsState();
                    BtnStart.Content = LocalizationService.GetString("Convert_BtnStart");
                    DialogResult = true;
                    break;

                case "convert.cancelled":
                    _isConverting = false;
                    TxtStatus.Text = LocalizationService.GetString("Convert_Cancelled");
                    ResetControlsState();
                    break;

                case "convert.error":
                    _isConverting = false;
                    string error = data["error"]?.GetValue<string>() ?? "Unknown error";
                    TxtStatus.Text = (LocalizationService.CurrentLanguage == "ru" ? "Ошибка: " : "Error: ") + error;
                    ResetControlsState();
                    break;
            }
        });
    }
}

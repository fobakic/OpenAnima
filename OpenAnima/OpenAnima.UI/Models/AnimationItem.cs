using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.Runtime.CompilerServices;
using System.Windows.Media.Imaging;

namespace OpenAnima.UI.Models;

public class AnimationItem : INotifyPropertyChanged
{
    private bool _isPlaying;
    private long? _overlayId;
    private double _alpha = 1.0;
    private int _x = 200;
    private int _y = 200;
    private bool _alwaysOnTop = true;
    private bool _clickThrough = false;
    private bool _physics = false;
    private double _scale = 20.0;
    private bool _smooth = true;
    private BitmapSource? _preview;

    public required string FilePath { get; init; }
    public string Name => Path.GetFileNameWithoutExtension(FilePath);
    public string Extension => Path.GetExtension(FilePath).ToUpperInvariant();

    public BitmapSource? Preview
    {
        get
        {
            if (_preview == null && File.Exists(FilePath))
            {
                try
                {
                    var img = new BitmapImage();
                    img.BeginInit();
                    img.UriSource = new Uri(FilePath);
                    img.DecodePixelWidth = 100;
                    img.CacheOption = BitmapCacheOption.OnLoad;
                    img.EndInit();
                    img.Freeze();
                    _preview = img;
                }
                catch { }
            }
            return _preview;
        }
    }

    public double Scale
    {
        get => _scale;
        set => SetField(ref _scale, value);
    }

    public bool Smooth
    {
        get => _smooth;
        set => SetField(ref _smooth, value);
    }

    public bool IsPlaying
    {
        get => _isPlaying;
        set => SetField(ref _isPlaying, value);
    }

    public long? OverlayId
    {
        get => _overlayId;
        set => SetField(ref _overlayId, value);
    }

    public double Alpha
    {
        get => _alpha;
        set => SetField(ref _alpha, value);
    }

    public int X
    {
        get => _x;
        set => SetField(ref _x, value);
    }

    public int Y
    {
        get => _y;
        set => SetField(ref _y, value);
    }

    public bool AlwaysOnTop
    {
        get => _alwaysOnTop;
        set => SetField(ref _alwaysOnTop, value);
    }

    public bool ClickThrough
    {
        get => _clickThrough;
        set => SetField(ref _clickThrough, value);
    }

    public bool Physics
    {
        get => _physics;
        set
        {
            if (SetField(ref _physics, value) && value && ClickThrough)
            {
                ClickThrough = false;
            }
        }
    }

    private bool _bouncy = true;
    public bool Bouncy
    {
        get => _bouncy;
        set => SetField(ref _bouncy, value);
    }

    private bool _chromaKey = false;
    private string _chromaColor = "#FFFFFF";
    private double _chromaTolerance = 15.0;
    private bool _chromaContiguous = true;

    public bool ChromaKey
    {
        get => _chromaKey;
        set => SetField(ref _chromaKey, value);
    }

    public string ChromaColor
    {
        get => _chromaColor;
        set => SetField(ref _chromaColor, value);
    }

    public double ChromaTolerance
    {
        get => _chromaTolerance;
        set => SetField(ref _chromaTolerance, value);
    }

    public bool ChromaContiguous
    {
        get => _chromaContiguous;
        set => SetField(ref _chromaContiguous, value);
    }

    public event PropertyChangedEventHandler? PropertyChanged;

    protected void OnPropertyChanged([CallerMemberName] string? propertyName = null)
        => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));

    protected bool SetField<T>(ref T field, T value, [CallerMemberName] string? propertyName = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value)) return false;
        field = value;
        OnPropertyChanged(propertyName);
        return true;
    }
}

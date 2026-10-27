using System.Text.Json.Nodes;
using System.Threading.Tasks;

namespace OpenAnima.UI.Services;

public class AnimationService
{
    private readonly CoreSupervisor _supervisor;

    public AnimationService(CoreSupervisor supervisor)
    {
        _supervisor = supervisor;
    }

    public async Task<long?> PlayAsync(
        string path,
        int x = 200,
        int y = 200,
        int fps = 20,
        double alpha = 1.0,
        double scale = 0.20,
        bool smooth = true,
        bool alwaysOnTop = true,
        bool clickThrough = false,
        bool physics = false,
        bool bouncy = true,
        bool chromaKey = false,
        string chromaColor = "#FFFFFF",
        double chromaTolerance = 0.15,
        bool chromaContiguous = true)
    {
        await _supervisor.EnsureStartedAndConnectedAsync().ConfigureAwait(false);

        var param = new JsonObject
        {
            ["path"] = path,
            ["x"] = x,
            ["y"] = y,
            ["fps"] = fps,
            ["alpha"] = alpha,
            ["scale"] = scale,
            ["smooth"] = smooth,
            ["always_on_top"] = alwaysOnTop,
            ["click_through"] = clickThrough,
            ["physics"] = physics,
            ["bouncy"] = bouncy,
            ["chroma_key"] = chromaKey,
            ["chroma_color"] = chromaColor,
            ["chroma_tolerance"] = chromaTolerance,
            ["chroma_contiguous"] = chromaContiguous
        };

        var resp = await _supervisor.SendCommandAsync("overlay.play", param).ConfigureAwait(false);
        if (resp != null && resp["ok"]?.GetValue<bool>() == true)
        {
            return resp["result"]?["overlay_id"]?.GetValue<long>();
        }

        return null;
    }

    public async Task<bool> StopAsync(long overlayId)
    {
        var param = new JsonObject { ["overlay_id"] = overlayId };
        var resp = await _supervisor.SendCommandAsync("overlay.stop", param).ConfigureAwait(false);
        return resp != null && resp["ok"]?.GetValue<bool>() == true;
    }

    public async Task<bool> StopAllAsync()
    {
        var resp = await _supervisor.SendCommandAsync("overlay.stop_all").ConfigureAwait(false);
        return resp != null && resp["ok"]?.GetValue<bool>() == true;
    }

    public async Task<bool> UpdateAsync(
        long overlayId,
        int? x = null,
        int? y = null,
        double? alpha = null,
        double? scale = null,
        bool? smooth = null,
        bool? alwaysOnTop = null,
        bool? clickThrough = null,
        bool? physics = null,
        bool? bouncy = null,
        bool? visible = null,
        bool? chromaKey = null,
        string? chromaColor = null,
        double? chromaTolerance = null,
        bool? chromaContiguous = null)
    {
        var param = new JsonObject { ["overlay_id"] = overlayId };
        if (x.HasValue) param["x"] = x.Value;
        if (y.HasValue) param["y"] = y.Value;
        if (alpha.HasValue) param["alpha"] = alpha.Value;
        if (scale.HasValue) param["scale"] = scale.Value;
        if (smooth.HasValue) param["smooth"] = smooth.Value;
        if (alwaysOnTop.HasValue) param["always_on_top"] = alwaysOnTop.Value;
        if (clickThrough.HasValue) param["click_through"] = clickThrough.Value;
        if (physics.HasValue) param["physics"] = physics.Value;
        if (bouncy.HasValue) param["bouncy"] = bouncy.Value;
        if (visible.HasValue) param["visible"] = visible.Value;
        if (chromaKey.HasValue) param["chroma_key"] = chromaKey.Value;
        if (!string.IsNullOrEmpty(chromaColor)) param["chroma_color"] = chromaColor;
        if (chromaTolerance.HasValue) param["chroma_tolerance"] = chromaTolerance.Value;
        if (chromaContiguous.HasValue) param["chroma_contiguous"] = chromaContiguous.Value;

        var resp = await _supervisor.SendCommandAsync("overlay.set", param).ConfigureAwait(false);
        return resp != null && resp["ok"]?.GetValue<bool>() == true;
    }

    public async Task<bool> HideAllAsync()
    {
        var resp = await _supervisor.SendCommandAsync("overlay.hide_all").ConfigureAwait(false);
        return resp != null && resp["ok"]?.GetValue<bool>() == true;
    }

    public async Task<bool> ShowAllAsync()
    {
        var resp = await _supervisor.SendCommandAsync("overlay.show_all").ConfigureAwait(false);
        return resp != null && resp["ok"]?.GetValue<bool>() == true;
    }

    public async Task<JsonArray?> ListOverlaysAsync()
    {
        var resp = await _supervisor.SendCommandAsync("overlay.list").ConfigureAwait(false);
        if (resp != null && resp["ok"]?.GetValue<bool>() == true)
        {
            return resp["result"]?["overlays"] as JsonArray;
        }
        return null;
    }

    public async Task<JsonObject?> GetStatsAsync()
    {
        var resp = await _supervisor.SendCommandAsync("core.stats").ConfigureAwait(false);
        return resp?["result"] as JsonObject;
    }

    public async Task<JsonObject?> TrimMemoryAsync()
    {
        var resp = await _supervisor.SendCommandAsync("core.trim_memory").ConfigureAwait(false);
        return resp?["result"] as JsonObject;
    }

    public async Task<bool> ConvertStartAsync(
        string inPath,
        string? outPath = null,
        int fps = 20,
        int maxDim = 384,
        bool chromaKey = false,
        string chromaColor = "#FFFFFF",
        double chromaTolerance = 0.15,
        bool chromaContiguous = true,
        string? jobId = null)
    {
        await _supervisor.EnsureStartedAndConnectedAsync().ConfigureAwait(false);

        var param = new JsonObject
        {
            ["in_path"] = inPath,
            ["fps"] = fps,
            ["max_dim"] = maxDim,
            ["chroma_key"] = chromaKey,
            ["chroma_color"] = chromaColor,
            ["chroma_tolerance"] = chromaTolerance,
            ["chroma_contiguous"] = chromaContiguous
        };

        if (!string.IsNullOrEmpty(outPath))
        {
            param["out_path"] = outPath;
        }

        if (!string.IsNullOrEmpty(jobId))
        {
            param["job_id"] = jobId;
        }

        var resp = await _supervisor.SendCommandAsync("media.convert_start", param).ConfigureAwait(false);
        return resp != null && resp["ok"]?.GetValue<bool>() == true;
    }

    public async Task<bool> ConvertCancelAsync(string? jobId = null)
    {
        var param = new JsonObject();
        if (!string.IsNullOrEmpty(jobId))
        {
            param["job_id"] = jobId;
        }

        var resp = await _supervisor.SendCommandAsync("media.convert_cancel", param).ConfigureAwait(false);
        return resp != null && resp["ok"]?.GetValue<bool>() == true;
    }
}

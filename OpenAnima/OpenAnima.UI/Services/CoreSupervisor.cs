using System;
using System.Collections.Concurrent;
using System.Diagnostics;
using System.IO;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Threading;
using System.Threading.Tasks;

namespace OpenAnima.UI.Services;

/// <summary>
/// Manages the native Rust OpenAnima.Core process with Windows Job Object limits
/// and provides named-pipe IPC communication.
/// </summary>
public sealed class CoreSupervisor : IDisposable
{
    private const string PipeName = "openanima-core-v1";
    private const int MaxMemoryMb = 500;

    public event Action<int>? OnCoreExited;
    public event Action<string, JsonObject?>? OnBroadcastEventReceived;

    private Process? _coreProcess;
    private IntPtr _jobHandle = IntPtr.Zero;
    private NamedPipeClientStream? _pipeStream;
    private readonly SemaphoreSlim _lock = new(1, 1);
    private readonly SemaphoreSlim _writeLock = new(1, 1);
    private readonly ConcurrentDictionary<long, TaskCompletionSource<JsonObject?>> _pendingRequests = new();
    private CancellationTokenSource? _readCts;
    private long _nextId = 1;
    private bool _isRestarting;
    private bool _disposed;

    public bool IsConnected => _pipeStream?.IsConnected == true;

    /// <summary>
    /// Ensures openanima-core.exe is running under a safety Job Object and connected via named pipe.
    /// </summary>
    public async Task<bool> EnsureStartedAndConnectedAsync(string? explicitCorePath = null, CancellationToken ct = default)
    {
        bool connected = false;
        await _lock.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            if (IsConnected)
                return true;

            // 1. Try connecting to an already running core
            if (await TryConnectPipeAsync(500, ct).ConfigureAwait(false))
            {
                connected = true;
            }
            else
            {
                // 2. Locate core binary
                string coreExe = explicitCorePath ?? FindCoreExecutable();
                if (!File.Exists(coreExe))
                {
                    Debug.WriteLine($"[CoreSupervisor] Core executable not found at: {coreExe}");
                    return false;
                }

                // 3. Launch process with Job Object limit
                var startInfo = new ProcessStartInfo
                {
                    FileName = coreExe,
                    CreateNoWindow = true,
                    UseShellExecute = false,
                    RedirectStandardOutput = false,
                    RedirectStandardError = false
                };

                _coreProcess = Process.Start(startInfo);
                if (_coreProcess == null)
                    return false;

                _coreProcess.EnableRaisingEvents = true;
                _coreProcess.Exited += (s, ev) =>
                {
                    if (_isRestarting) return;
                    int exitCode = -1;
                    try { exitCode = _coreProcess?.ExitCode ?? -1; } catch { }
                    App.Log($"[CoreSupervisor] Core process exited (code: {exitCode})");
                    OnCoreExited?.Invoke(exitCode);
                };

                // Apply Job Object safety limits
                ApplyJobLimits(_coreProcess.Handle, MaxMemoryMb);

                // 4. Wait for named pipe server to accept connection
                connected = await TryConnectPipeAsync(5000, ct).ConfigureAwait(false);
            }
        }
        finally
        {
            _lock.Release();
        }

        if (connected)
        {
            _ = SendCommandAsync("core.set_language", new JsonObject { ["lang"] = LocalizationService.CurrentLanguage });
        }

        return connected;
    }

    private async Task<bool> TryConnectPipeAsync(int timeoutMs, CancellationToken ct)
    {
        try
        {
            var pipe = new NamedPipeClientStream(".", PipeName, PipeDirection.InOut, PipeOptions.Asynchronous);
            using var timeoutCts = CancellationTokenSource.CreateLinkedTokenSource(ct);
            timeoutCts.CancelAfter(timeoutMs);

            await pipe.ConnectAsync(timeoutCts.Token).ConfigureAwait(false);
            _pipeStream = pipe;
            StartReadLoop();
            return true;
        }
        catch
        {
            _readCts?.Cancel();
            _pipeStream?.Dispose();
            _pipeStream = null;
            return false;
        }
    }

    private void StartReadLoop()
    {
        _readCts?.Cancel();
        _readCts?.Dispose();
        _readCts = new CancellationTokenSource();
        var token = _readCts.Token;
        _ = Task.Run(async () =>
        {
            var pipe = _pipeStream;
            if (pipe == null) return;

            byte[] lenBuf = new byte[4];
            while (!token.IsCancellationRequested && pipe.IsConnected)
            {
                try
                {
                    await pipe.ReadExactlyAsync(lenBuf, 0, 4, token).ConfigureAwait(false);
                    uint length = BitConverter.ToUInt32(lenBuf, 0);
                    if (length == 0 || length > 16 * 1024 * 1024)
                        break;

                    byte[] respBytes = new byte[length];
                    await pipe.ReadExactlyAsync(respBytes, 0, (int)length, token).ConfigureAwait(false);
                    var node = JsonNode.Parse(respBytes) as JsonObject;
                    if (node == null) continue;

                    if (node.ContainsKey("event") && !node.ContainsKey("id"))
                    {
                        string eventName = node["event"]?.ToString() ?? string.Empty;
                        var eventData = node["data"] as JsonObject;
                        try
                        {
                            OnBroadcastEventReceived?.Invoke(eventName, eventData);
                        }
                        catch (Exception ex)
                        {
                            App.Log($"[CoreSupervisor] Broadcast event error: {ex.Message}");
                        }
                    }
                    else if (node.ContainsKey("id"))
                    {
                        long id = node["id"]?.GetValue<long>() ?? 0;
                        if (_pendingRequests.TryRemove(id, out var tcs))
                        {
                            tcs.TrySetResult(node);
                        }
                    }
                }
                catch
                {
                    break;
                }
            }

            foreach (var kvp in _pendingRequests)
            {
                if (_pendingRequests.TryRemove(kvp.Key, out var pending))
                {
                    pending.TrySetResult(null);
                }
            }
        }, token);
    }

    public async Task<JsonObject?> SendCommandAsync(string cmd, JsonObject? parameters = null, CancellationToken ct = default)
    {
        if (_pipeStream == null || !_pipeStream.IsConnected)
            return null;

        long id = Interlocked.Increment(ref _nextId);
        var reqObj = new JsonObject
        {
            ["id"] = id,
            ["cmd"] = cmd,
            ["params"] = parameters ?? new JsonObject()
        };

        var tcs = new TaskCompletionSource<JsonObject?>(TaskCreationOptions.RunContinuationsAsynchronously);
        _pendingRequests[id] = tcs;

        byte[] jsonBytes = System.Text.Encoding.UTF8.GetBytes(reqObj.ToJsonString());
        byte[] lenBytes = BitConverter.GetBytes((uint)jsonBytes.Length);

        await _writeLock.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            if (_pipeStream == null || !_pipeStream.IsConnected)
            {
                _pendingRequests.TryRemove(id, out _);
                return null;
            }
            await _pipeStream.WriteAsync(lenBytes, 0, 4, ct).ConfigureAwait(false);
            await _pipeStream.WriteAsync(jsonBytes, 0, jsonBytes.Length, ct).ConfigureAwait(false);
            await _pipeStream.FlushAsync(ct).ConfigureAwait(false);
        }
        catch (Exception ex)
        {
            App.Log($"[CoreSupervisor] IPC write error on '{cmd}': {ex.Message}");
            _pendingRequests.TryRemove(id, out _);
            return null;
        }
        finally
        {
            _writeLock.Release();
        }

        using var reg = ct.Register(() =>
        {
            if (_pendingRequests.TryRemove(id, out var pending))
            {
                pending.TrySetCanceled(ct);
            }
        });

        try
        {
            return await tcs.Task.ConfigureAwait(false);
        }
        catch
        {
            return null;
        }
    }

    private static string FindCoreExecutable()
    {
        string baseDir = AppDomain.CurrentDomain.BaseDirectory;
        string[] candidates =
        [
            Path.Combine(baseDir, "openanima-core.exe"),
            Path.Combine(baseDir, "..", "..", "..", "..", "OpenAnima.Core", "target", "release", "openanima-core.exe"),
            Path.Combine(baseDir, "..", "..", "..", "..", "OpenAnima.Core", "target", "debug", "openanima-core.exe"),
            Path.GetFullPath(Path.Combine(baseDir, @"..\..\..\..\OpenAnima.Core\target\release\openanima-core.exe"))
        ];

        foreach (var c in candidates)
        {
            if (File.Exists(c))
                return Path.GetFullPath(c);
        }

        // If running as a standalone single-file executable and core is not found on disk,
        // extract the embedded openanima-core.exe to %LOCALAPPDATA%\OpenAnima\bin\
        try
        {
            string appDataBin = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "OpenAnima", "bin");
            Directory.CreateDirectory(appDataBin);
            string extractedPath = Path.Combine(appDataBin, "openanima-core.exe");

            var asm = typeof(CoreSupervisor).Assembly;
            using var stream = asm.GetManifestResourceStream("openanima-core.exe");
            if (stream != null)
            {
                if (!File.Exists(extractedPath) || new FileInfo(extractedPath).Length != stream.Length)
                {
                    using var fs = new FileStream(extractedPath, FileMode.Create, FileAccess.Write, FileShare.None);
                    stream.CopyTo(fs);
                }
                return extractedPath;
            }
        }
        catch (Exception ex)
        {
            Debug.WriteLine($"[CoreSupervisor] Failed to extract embedded core: {ex.Message}");
        }

        return candidates[0];
    }

    #region Windows Job Object P/Invoke
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr CreateJobObject(IntPtr lpJobAttributes, string? lpName);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetInformationJobObject(IntPtr hJob, int jobObjectInfoClass, IntPtr lpJobObjectInfo, uint cbJobObjectInfoLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool AssignProcessToJobObject(IntPtr hJob, IntPtr hProcess);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CloseHandle(IntPtr hObject);

    [StructLayout(LayoutKind.Sequential)]
    private struct IO_COUNTERS
    {
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
        public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_BASIC_LIMIT_INFORMATION
    {
        public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize, MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass, SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryLimit, PeakJobMemoryLimit;
    }

    private void ApplyJobLimits(IntPtr processHandle, int maxMemoryMb)
    {
        try
        {
            _jobHandle = CreateJobObject(IntPtr.Zero, null);
            if (_jobHandle == IntPtr.Zero) return;

            var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
            // 0x2000 = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            // 0x0100 = JOB_OBJECT_LIMIT_PROCESS_MEMORY
            info.BasicLimitInformation.LimitFlags = 0x2000 | 0x0100;
            info.ProcessMemoryLimit = new UIntPtr((ulong)maxMemoryMb * 1024 * 1024);

            int length = Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
            IntPtr ptr = Marshal.AllocHGlobal(length);
            try
            {
                Marshal.StructureToPtr(info, ptr, false);
                if (SetInformationJobObject(_jobHandle, 9 /* JobObjectExtendedLimitInformation */, ptr, (uint)length))
                {
                    AssignProcessToJobObject(_jobHandle, processHandle);
                }
            }
            finally
            {
                Marshal.FreeHGlobal(ptr);
            }
        }
        catch (Exception ex)
        {
            Debug.WriteLine($"[CoreSupervisor] Job limits failed: {ex.Message}");
        }
    }
    #endregion

    public async Task<bool> RestartCoreAsync(CancellationToken ct = default)
    {
        _isRestarting = true;
        await _lock.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            try
            {
                if (IsConnected && _pipeStream != null)
                {
                    var reqObj = new JsonObject
                    {
                        ["id"] = Interlocked.Increment(ref _nextId),
                        ["cmd"] = "core.quit",
                        ["params"] = new JsonObject()
                    };
                    byte[] jsonBytes = System.Text.Encoding.UTF8.GetBytes(reqObj.ToJsonString());
                    byte[] lenBytes = BitConverter.GetBytes((uint)jsonBytes.Length);
                    _pipeStream.Write(lenBytes, 0, 4);
                    _pipeStream.Write(jsonBytes, 0, jsonBytes.Length);
                    _pipeStream.Flush();
                }
            }
            catch { }

            _readCts?.Cancel();
        _readCts?.Dispose();
        _pipeStream?.Dispose();
        _writeLock.Dispose();
            _pipeStream = null;

            if (_jobHandle != IntPtr.Zero)
            {
                CloseHandle(_jobHandle);
                _jobHandle = IntPtr.Zero;
            }

            try
            {
                if (_coreProcess != null && !_coreProcess.HasExited)
                {
                    _coreProcess.Kill();
                    _coreProcess.WaitForExit(1000);
                }
            }
            catch { }
            finally
            {
                _coreProcess?.Dispose();
                _coreProcess = null;
            }
        }
        finally
        {
            _lock.Release();
        }

        bool connected = await EnsureStartedAndConnectedAsync(ct: ct).ConfigureAwait(false);
        _isRestarting = false;
        return connected;
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;

        try
        {
            if (IsConnected)
            {
                _ = SendCommandAsync("core.quit");
            }
        }
        catch { }

        _readCts?.Cancel();
        _readCts?.Dispose();
        _pipeStream?.Dispose();
        _writeLock.Dispose();

        if (_jobHandle != IntPtr.Zero)
        {
            CloseHandle(_jobHandle);
            _jobHandle = IntPtr.Zero;
        }

        try
        {
            if (_coreProcess != null && !_coreProcess.HasExited)
            {
                _coreProcess.Kill();
            }
        }
        catch { }
        finally
        {
            _coreProcess?.Dispose();
        }

        _lock.Dispose();
    }
}

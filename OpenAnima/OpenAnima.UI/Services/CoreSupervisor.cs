using System;
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

    private Process? _coreProcess;
    private IntPtr _jobHandle = IntPtr.Zero;
    private NamedPipeClientStream? _pipeStream;
    private BinaryReader? _reader;
    private BinaryWriter? _writer;
    private readonly SemaphoreSlim _lock = new(1, 1);
    private long _nextId = 1;
    private bool _disposed;

    public bool IsConnected => _pipeStream?.IsConnected == true;

    /// <summary>
    /// Ensures openanima-core.exe is running under a safety Job Object and connected via named pipe.
    /// </summary>
    public async Task<bool> EnsureStartedAndConnectedAsync(string? explicitCorePath = null, CancellationToken ct = default)
    {
        await _lock.WaitAsync(ct);
        try
        {
            if (IsConnected)
                return true;

            // 1. Try connecting to an already running core
            if (await TryConnectPipeAsync(500, ct))
                return true;

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

            // Apply Job Object safety limits
            ApplyJobLimits(_coreProcess.Handle, MaxMemoryMb);

            // 4. Wait for named pipe server to accept connection
            bool connected = await TryConnectPipeAsync(5000, ct);
            return connected;
        }
        finally
        {
            _lock.Release();
        }
    }

    private async Task<bool> TryConnectPipeAsync(int timeoutMs, CancellationToken ct)
    {
        try
        {
            var pipe = new NamedPipeClientStream(".", PipeName, PipeDirection.InOut, PipeOptions.Asynchronous);
            using var timeoutCts = CancellationTokenSource.CreateLinkedTokenSource(ct);
            timeoutCts.CancelAfter(timeoutMs);

            await pipe.ConnectAsync(timeoutCts.Token);
            _pipeStream = pipe;
            _reader = new BinaryReader(pipe);
            _writer = new BinaryWriter(pipe);
            return true;
        }
        catch
        {
            _pipeStream?.Dispose();
            _pipeStream = null;
            _reader = null;
            _writer = null;
            return false;
        }
    }

    public async Task<JsonObject?> SendCommandAsync(string cmd, JsonObject? parameters = null, CancellationToken ct = default)
    {
        await _lock.WaitAsync(ct);
        try
        {
            if (_pipeStream == null || !_pipeStream.IsConnected || _writer == null || _reader == null)
                return null;

            long id = Interlocked.Increment(ref _nextId);
            var reqObj = new JsonObject
            {
                ["id"] = id,
                ["cmd"] = cmd,
                ["params"] = parameters ?? new JsonObject()
            };

            byte[] jsonBytes = System.Text.Encoding.UTF8.GetBytes(reqObj.ToJsonString());
            _writer.Write((uint)jsonBytes.Length);
            _writer.Write(jsonBytes);
            _writer.Flush();

            uint length = _reader.ReadUInt32();
            if (length == 0 || length > 16 * 1024 * 1024)
                return null;

            byte[] respBytes = _reader.ReadBytes((int)length);
            var node = JsonNode.Parse(respBytes) as JsonObject;
            return node;
        }
        catch (Exception ex)
        {
            Debug.WriteLine($"[CoreSupervisor] IPC error on '{cmd}': {ex.Message}");
            return null;
        }
        finally
        {
            _lock.Release();
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
        await _lock.WaitAsync(ct);
        try
        {
            try
            {
                if (IsConnected && _writer != null)
                {
                    var reqObj = new JsonObject
                    {
                        ["id"] = Interlocked.Increment(ref _nextId),
                        ["cmd"] = "core.quit",
                        ["params"] = new JsonObject()
                    };
                    byte[] jsonBytes = System.Text.Encoding.UTF8.GetBytes(reqObj.ToJsonString());
                    _writer.Write((uint)jsonBytes.Length);
                    _writer.Write(jsonBytes);
                    _writer.Flush();
                }
            }
            catch { }

            _reader?.Dispose();
            _reader = null;
            _writer?.Dispose();
            _writer = null;
            _pipeStream?.Dispose();
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

        return await EnsureStartedAndConnectedAsync(ct: ct);
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

        _reader?.Dispose();
        _writer?.Dispose();
        _pipeStream?.Dispose();

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

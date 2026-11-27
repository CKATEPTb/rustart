using System;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class DesktopProbe
{
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)]
    private struct MonitorInfo { public uint Size; public Rect Monitor, Work; public uint Flags; }

    [DllImport("user32", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
    [DllImport("user32")] public static extern IntPtr GetDlgItem(IntPtr hwnd, int id);
    [DllImport("user32")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32")] public static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);
    [DllImport("user32")] public static extern bool GetClientRect(IntPtr hwnd, out Rect rect);
    [DllImport("user32")] private static extern IntPtr MonitorFromWindow(IntPtr hwnd, uint flags);
    [DllImport("user32", EntryPoint = "GetMonitorInfoW")] private static extern bool GetMonitorInfo(IntPtr monitor, ref MonitorInfo info);
    [StructLayout(LayoutKind.Sequential)] public struct Point { public int X, Y; }
    [DllImport("user32")] public static extern bool GetCursorPos(out Point point);
    [DllImport("user32")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32")] private static extern IntPtr WindowFromPoint(Point point);
    [DllImport("user32")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [StructLayout(LayoutKind.Sequential)] private struct GuiThreadInfo {
        public uint Size, Flags;
        public IntPtr Active, Focus, Capture, MenuOwner, MoveSize, Caret;
        public Rect CaretRect;
    }
    [DllImport("user32")] private static extern bool GetGUIThreadInfo(uint thread, ref GuiThreadInfo info);
    [DllImport("user32")] private static extern int GetDlgCtrlID(IntPtr hwnd);
    public static int FocusId(IntPtr parent) {
        uint process;
        var info = new GuiThreadInfo { Size = (uint)Marshal.SizeOf(typeof(GuiThreadInfo)) };
        return GetGUIThreadInfo(GetWindowThreadProcessId(parent, out process), ref info) ? GetDlgCtrlID(info.Focus) : -1;
    }
    [DllImport("user32", EntryPoint = "SendMessageW")] public static extern IntPtr Send(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
    [DllImport("user32", EntryPoint = "SendMessageW", CharSet = CharSet.Unicode)] private static extern IntPtr SendText(IntPtr hwnd, uint msg, IntPtr wp, string text);
    [DllImport("user32", EntryPoint = "SendMessageW", CharSet = CharSet.Unicode)] private static extern IntPtr ReadText(IntPtr hwnd, uint msg, IntPtr wp, StringBuilder text);
    [DllImport("user32", EntryPoint = "PostMessageW")] public static extern bool Post(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
    [DllImport("user32")] private static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr info);
    [DllImport("user32")] public static extern bool RegisterHotKey(IntPtr hwnd, int id, uint modifiers, uint key);
    [DllImport("user32")] public static extern bool UnregisterHotKey(IntPtr hwnd, int id);
    [DllImport("user32")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
    [DllImport("user32")] public static extern bool AllowSetForegroundWindow(uint pid);
    [DllImport("user32")] public static extern uint GetGuiResources(IntPtr process, uint kind);
    [StructLayout(LayoutKind.Sequential)]
    private struct MemoryCountersEx {
        public uint Size, FaultCount;
        public UIntPtr PeakWorkingSet, WorkingSet, PeakPagedPool, PagedPool;
        public UIntPtr PeakNonPagedPool, NonPagedPool, Commit, PeakCommit, PrivateBytes;
    }
    [StructLayout(LayoutKind.Sequential)]
    private struct MemoryCountersEx2 {
        public MemoryCountersEx Basic;
        public UIntPtr PrivateWorkingSet;
        public ulong SharedCommit;
    }
    [DllImport("psapi.dll", SetLastError = true, EntryPoint = "GetProcessMemoryInfo")]
    private static extern bool ReadMemory(IntPtr process, ref MemoryCountersEx2 counters, uint size);
    public sealed class MemorySample {
        public ulong? PrivateWorkingSetBytes;
        public ulong PrivateBytes, WorkingSetBytes;
        public string Source;
    }
    public static MemorySample Memory(IntPtr process) {
        var counters = new MemoryCountersEx2();
        counters.Basic.Size = (uint)Marshal.SizeOf(typeof(MemoryCountersEx2));
        counters.PrivateWorkingSet = (UIntPtr)1;
        // EX2 requires Windows 10/11 22H2 with the September 2023 cumulative
        // update. Older builds still report private commit and the total WS.
        bool withPrivateWorkingSet = ReadMemory(process, ref counters, counters.Basic.Size)
            && counters.PrivateWorkingSet.ToUInt64() != 1;
        if (!withPrivateWorkingSet) {
            counters = new MemoryCountersEx2();
            counters.Basic.Size = (uint)Marshal.SizeOf(typeof(MemoryCountersEx));
            if (!ReadMemory(process, ref counters, counters.Basic.Size))
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        }
        return new MemorySample {
            PrivateWorkingSetBytes = withPrivateWorkingSet ? (ulong?)counters.PrivateWorkingSet.ToUInt64() : null,
            PrivateBytes = counters.Basic.PrivateBytes.ToUInt64(),
            WorkingSetBytes = counters.Basic.WorkingSet.ToUInt64(),
            Source = withPrivateWorkingSet ? "PSAPI EX2" : "PSAPI EX (private WS unavailable)"
        };
    }
    [DllImport("kernel32", SetLastError = true)] private static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
    [DllImport("kernel32")] private static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32", EntryPoint = "QueryFullProcessImageNameW", CharSet = CharSet.Unicode)]
    private static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder path, ref int length);

    public sealed class ProcessGroupSample {
        public int ProcessCount;
        public ulong? PrivateWorkingSetBytes;
        public ulong PrivateBytes;
    }

    public static ProcessGroupSample GroupMemory(string executable) {
        const uint queryLimited = 0x1000, vmRead = 0x0010;
        var path = Path.GetFullPath(executable);
        var processes = Process.GetProcessesByName(Path.GetFileNameWithoutExtension(path));
        var result = new ProcessGroupSample();
        var image = new StringBuilder(Math.Max(512, path.Length + 1));
        ulong privateWorkingSet = 0;
        bool hasPrivateWorkingSet = true;
        try {
            foreach (var process in processes) {
                var handle = OpenProcess(queryLimited | vmRead, false, process.Id);
                if (handle == IntPtr.Zero) continue;
                try {
                    int length = image.Capacity;
                    if (!QueryFullProcessImageName(handle, 0, image, ref length) ||
                            !String.Equals(path, image.ToString(), StringComparison.OrdinalIgnoreCase))
                        continue;
                    MemorySample memory;
                    try {
                        memory = Memory(handle);
                    } catch (System.ComponentModel.Win32Exception) {
                        // A short-lived helper may have exited after enumeration.
                        if (process.HasExited) continue;
                        throw;
                    }
                    result.ProcessCount++;
                    result.PrivateBytes += memory.PrivateBytes;
                    if (memory.PrivateWorkingSetBytes.HasValue)
                        privateWorkingSet += memory.PrivateWorkingSetBytes.Value;
                    else
                        hasPrivateWorkingSet = false;
                } finally {
                    CloseHandle(handle);
                }
            }
        } finally {
            foreach (var process in processes) process.Dispose();
        }
        if (hasPrivateWorkingSet && result.ProcessCount > 0)
            result.PrivateWorkingSetBytes = privateWorkingSet;
        return result;
    }

    public sealed class ProcessGroupPeak {
        public int SamplingPeriodMs, Samples, MaxProcessCount;
        public double MaxSampleGapMs;
        public ulong? PrivateWorkingSetBytes;
        public ulong PrivateBytes;
    }

    public sealed class ProcessGroupSampler : IDisposable {
        private readonly string executable;
        private readonly ManualResetEvent stop = new ManualResetEvent(false);
        private readonly Stopwatch clock = Stopwatch.StartNew();
        private readonly ProcessGroupPeak peak;
        private readonly Thread reader;
        private double lastSampleMs;
        private bool unavailablePrivateWorkingSet, disposed;
        private Exception error;

        public ProcessGroupSampler(string executable, int samplingPeriodMs) {
            if (samplingPeriodMs <= 0) throw new ArgumentOutOfRangeException("samplingPeriodMs");
            this.executable = executable;
            peak = new ProcessGroupPeak { SamplingPeriodMs = samplingPeriodMs };
            reader = new Thread(SampleUntilStopped) { IsBackground = true };
            try {
                Sample();
                reader.Start();
            } catch {
                stop.Dispose();
                throw;
            }
        }

        private void SampleUntilStopped() {
            try {
                while (!stop.WaitOne(peak.SamplingPeriodMs)) Sample();
            } catch (Exception failure) {
                error = failure;
            }
        }

        private void Sample() {
            double now = clock.Elapsed.TotalMilliseconds;
            var sample = GroupMemory(executable);
            if (peak.Samples > 0)
                peak.MaxSampleGapMs = Math.Max(peak.MaxSampleGapMs, now - lastSampleMs);
            lastSampleMs = now;
            peak.Samples++;
            peak.MaxProcessCount = Math.Max(peak.MaxProcessCount, sample.ProcessCount);
            peak.PrivateBytes = Math.Max(peak.PrivateBytes, sample.PrivateBytes);
            if (sample.ProcessCount == 0) return;
            if (!sample.PrivateWorkingSetBytes.HasValue) {
                unavailablePrivateWorkingSet = true;
                peak.PrivateWorkingSetBytes = null;
            } else if (!unavailablePrivateWorkingSet) {
                peak.PrivateWorkingSetBytes = Math.Max(peak.PrivateWorkingSetBytes.GetValueOrDefault(),
                    sample.PrivateWorkingSetBytes.Value);
            }
        }

        public ProcessGroupPeak Finish() {
            Dispose();
            if (error != null) throw new InvalidOperationException("Process-group sampling failed.", error);
            Sample();
            return peak;
        }

        public void Dispose() {
            if (disposed) return;
            stop.Set();
            reader.Join();
            stop.Dispose();
            disposed = true;
        }
    }
    [DllImport("user32", CharSet = CharSet.Unicode)] private static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string cls, string title);
    [DllImport("user32", EntryPoint = "GetWindowTextW", CharSet = CharSet.Unicode)] private static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int count);

    public static bool Visible(IntPtr hwnd) { return hwnd != IntPtr.Zero && IsWindowVisible(hwnd); }
    public static IntPtr FindClass(string cls) { return FindWindow(cls, null); }
    public static Rect WorkArea(IntPtr hwnd) {
        var info = new MonitorInfo { Size = (uint)Marshal.SizeOf(typeof(MonitorInfo)) };
        if (!GetMonitorInfo(MonitorFromWindow(hwnd, 2), ref info)) throw new InvalidOperationException("No monitor work area.");
        return info.Work;
    }
    public static bool CanClick(IntPtr hwnd) {
        Rect r;
        if (!Visible(hwnd) || !GetWindowRect(hwnd, out r)) return false;
        return WindowFromPoint(new Point { X = (r.Left+r.Right)/2, Y = (r.Top+r.Bottom)/2 }) == hwnd;
    }
    public static IntPtr FindButton(IntPtr parent, string prefix) {
        IntPtr button = IntPtr.Zero;
        while ((button = FindWindowEx(parent, button, "Button", null)) != IntPtr.Zero) {
            var text = new StringBuilder(256);
            GetWindowText(button, text, text.Capacity);
            if (text.ToString().StartsWith(prefix, StringComparison.Ordinal)) return button;
        }
        return IntPtr.Zero;
    }
    public static void Query(IntPtr edit, string text) { SendText(edit, 12, IntPtr.Zero, text); }
    public static string WindowLabel(IntPtr hwnd) {
        var text = new StringBuilder(256);
        ReadText(hwnd, 0x000D, (IntPtr)text.Capacity, text);
        return text.ToString();
    }
    public static int Value(IntPtr list, uint msg) { return Send(list, msg, IntPtr.Zero, IntPtr.Zero).ToInt32(); }
    public static string Row(IntPtr list, int row) {
        int count = Send(list, 0x018A, (IntPtr)row, IntPtr.Zero).ToInt32();
        if (count < 0) return null;
        var text = new StringBuilder(count + 1);
        ReadText(list, 0x0189, (IntPtr)row, text);
        return text.ToString();
    }
    public sealed class OpenTiming {
        public double HotkeyToEditFocusMs, QueryToFirstRowMs;
    }
    public static OpenTiming MeasureWarmSearch(string query, string expectedFirst, byte[] modifiers) {
        const byte r = 0x52;
        foreach (byte modifier in modifiers)
            keybd_event(modifier, 0, 0, UIntPtr.Zero);
        var watch = Stopwatch.StartNew();
        try {
            keybd_event(r, 0, 0, UIntPtr.Zero);
            keybd_event(r, 0, 2, UIntPtr.Zero);
        } finally {
            for (int i = modifiers.Length - 1; i >= 0; --i)
                keybd_event(modifiers[i], 0, 2, UIntPtr.Zero);
        }
        IntPtr overlay;
        do {
            overlay = GetForegroundWindow();
            if (Visible(overlay) && overlay == FindClass("Rustart.Overlay") && FocusId(overlay) == 101)
                break;
            Thread.Sleep(1);
        } while (watch.ElapsedMilliseconds < 6000);
        if (watch.ElapsedMilliseconds >= 6000)
            throw new TimeoutException("The timed hotkey did not focus the Rustart edit.");
        double focus = watch.Elapsed.TotalMilliseconds;
        var edit = GetDlgItem(overlay, 101);
        var list = GetDlgItem(overlay, 102);
        return new OpenTiming {
            HotkeyToEditFocusMs = focus,
            QueryToFirstRowMs = MeasureQueryToFirstRow(edit, list, query, expectedFirst)
        };
    }
    public static double MeasureQueryToFirstRow(IntPtr edit, IntPtr list, string query, string expectedFirst) {
        var watch = Stopwatch.StartNew();
        Query(edit, query);
        do {
            if (Visible(list) && Row(list, 0) == expectedFirst)
                return watch.Elapsed.TotalMilliseconds;
            Thread.Sleep(1);
        } while (watch.ElapsedMilliseconds < 6000);
        throw new TimeoutException("The timed query did not expose the expected first list row.");
    }
    public static void Chord(params byte[] keys) {
        foreach (byte key in keys) {
            keybd_event(key, 0, 0, UIntPtr.Zero);
            Thread.Sleep(24);
        }
        for (int i = keys.Length - 1; i >= 0; --i) {
            keybd_event(keys[i], 0, 2, UIntPtr.Zero);
            Thread.Sleep(12);
        }
    }
}

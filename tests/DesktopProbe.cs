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
    [DllImport("user32")] private static extern bool PrintWindow(IntPtr hwnd, IntPtr dc, uint flags);
    [DllImport("user32")] private static extern uint GetDpiForWindow(IntPtr hwnd);
    [DllImport("user32")] private static extern int GetWindowRgn(IntPtr hwnd, IntPtr region);
    [DllImport("dwmapi")] private static extern int DwmGetWindowAttribute(IntPtr hwnd, int attribute, out int value, int size);
    [DllImport("user32", CharSet = CharSet.Unicode)] private static extern int GetClassName(IntPtr hwnd, StringBuilder name, int capacity);
    [DllImport("user32")] private static extern bool SystemParametersInfoW(uint action, uint size, ref HighContrast value, uint flags);
    [DllImport("gdi32")] private static extern IntPtr CreateRectRgn(int left, int top, int right, int bottom);
    [DllImport("gdi32")] private static extern bool PtInRegion(IntPtr region, int x, int y);
    [DllImport("gdi32")] private static extern IntPtr CreateCompatibleDC(IntPtr dc);
    [DllImport("gdi32")] private static extern bool DeleteDC(IntPtr dc);
    [DllImport("gdi32")] private static extern IntPtr SelectObject(IntPtr dc, IntPtr obj);
    [DllImport("gdi32")] private static extern bool DeleteObject(IntPtr obj);
    [DllImport("gdi32")] private static extern bool GdiFlush();
    [DllImport("gdi32")] private static extern IntPtr CreateDIBSection(IntPtr dc, ref BitmapInfo info,
        uint usage, out IntPtr bits, IntPtr section, uint offset);

    [StructLayout(LayoutKind.Sequential)]
    private struct BitmapInfo {
        public uint Size;
        public int Width, Height;
        public ushort Planes, BitsPerPixel;
        public uint Compression, ImageSize;
        public int XPelsPerMeter, YPelsPerMeter;
        public uint ColorsUsed, ImportantColors;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct HighContrast {
        public uint Size, Flags;
        public IntPtr Scheme;
    }

    public sealed class WindowImage {
        public int Width, Height;
        public int[] Pixels;
    }

    public static WindowImage CaptureWindow(IntPtr hwnd) {
        Rect bounds;
        if (!Visible(hwnd) || !GetWindowRect(hwnd, out bounds))
            throw new InvalidOperationException("Could not measure the visible Rustart window.");
        int width = bounds.Right - bounds.Left, height = bounds.Bottom - bounds.Top;
        if (width <= 0 || height <= 0)
            throw new InvalidOperationException("The Rustart window is empty.");
        uint dpi = GetDpiForWindow(hwnd);
        if (dpi == 0) throw new InvalidOperationException("Could not read the Rustart window DPI.");
        var info = new BitmapInfo {
            Size = (uint)Marshal.SizeOf(typeof(BitmapInfo)), Width = width, Height = -height,
            Planes = 1, BitsPerPixel = 32
        };
        var pixels = new int[checked(width * height)];
        const int unpainted = 0x110d0211;
        for (int i = 0; i < pixels.Length; i++) pixels[i] = unpainted;
        var dc = CreateCompatibleDC(IntPtr.Zero);
        if (dc == IntPtr.Zero) throw new InvalidOperationException("Could not create the capture DC.");
        IntPtr bitmap = IntPtr.Zero, previous = IntPtr.Zero;
        try {
            IntPtr bits;
            bitmap = CreateDIBSection(dc, ref info, 0, out bits, IntPtr.Zero, 0);
            if (bitmap == IntPtr.Zero) throw new InvalidOperationException("Could not create the capture bitmap.");
            var selected = SelectObject(dc, bitmap);
            if (selected == IntPtr.Zero || selected == new IntPtr(-1))
                throw new InvalidOperationException("Could not select the capture bitmap.");
            previous = selected;
            Marshal.Copy(pixels, 0, bits, pixels.Length);
            // PW_RENDERFULLCONTENT requests the window composition, including its child controls.
            if (!PrintWindow(hwnd, dc, 2) || !GdiFlush())
                throw new InvalidOperationException("Could not print the Rustart window.");
            Marshal.Copy(bits, pixels, 0, pixels.Length);
        } finally {
            if (previous != IntPtr.Zero) SelectObject(dc, previous);
            if (bitmap != IntPtr.Zero) DeleteObject(bitmap);
            DeleteDC(dc);
        }
        var region = CreateRectRgn(0, 0, 0, 0);
        if (region == IntPtr.Zero) throw new InvalidOperationException("Could not allocate the screenshot region.");
        try {
            bool hasRegion = GetWindowRgn(hwnd, region) != 0;
            double radius = CaptureRadius(hwnd, dpi, width, height, hasRegion);
            ApplyWindowAlpha(pixels, width, height, radius, hasRegion ? region : IntPtr.Zero, unpainted);
        } finally {
            DeleteObject(region);
        }
        return new WindowImage { Width = width, Height = height, Pixels = pixels };
    }

    private static double CaptureRadius(IntPtr hwnd, uint dpi, int width, int height, bool hasRegion) {
        var cls = new StringBuilder(64);
        GetClassName(hwnd, cls, cls.Capacity);
        string name = cls.ToString();
        if (name == "Rustart.Overlay") {
            if (hasRegion) {
                // Match the fallback region diameter in src/overlay/view.rs.
                double diameter = (44 * dpi + 48) / 96;
                return Math.Min(diameter, Math.Min(width, height)) / 2;
            }
            // PrintWindow omits the desktop compositor's rounded clip.
            int preference;
            return DwmGetWindowAttribute(hwnd, 33, out preference, sizeof(int)) >= 0 && preference == 2
                ? Math.Min(8 * dpi / 96.0, Math.Min(width, height) / 2.0) : 0;
        }
        if (name != "Rustart.TrayMenu")
            throw new InvalidOperationException("Unsupported Rustart screenshot window.");
        var contrast = new HighContrast { Size = (uint)Marshal.SizeOf(typeof(HighContrast)) };
        if (!SystemParametersInfoW(0x0042, contrast.Size, ref contrast, 0))
            throw new InvalidOperationException("Could not determine the tray menu corner style.");
        // The tray requests the standard Windows 11 DWM corner except in high contrast.
        return Environment.OSVersion.Version.Build >= 22000 && (contrast.Flags & 1) == 0
            ? Math.Min(8 * dpi / 96.0, Math.Min(width, height) / 2.0) : 0;
    }

    private static void ApplyWindowAlpha(int[] pixels, int width, int height, double radius, IntPtr region, int unpainted) {
        const int samples = 8, rgbMask = 0x00ffffff;
        for (int y = 0; y < height; y++) {
            for (int x = 0; x < width; x++) {
                int index = y * width + x;
                int edgeX = Math.Min(x, width - 1 - x), edgeY = Math.Min(y, height - 1 - y);
                int covered = samples * samples;
                bool corner = edgeX < radius && edgeY < radius;
                if (corner) {
                    covered = 0;
                    for (int sy = 0; sy < samples; sy++) {
                        double dy = Math.Max(0, radius - edgeY - (sy + 0.5) / samples);
                        for (int sx = 0; sx < samples; sx++) {
                            double dx = Math.Max(0, radius - edgeX - (sx + 0.5) / samples);
                            if (dx * dx + dy * dy <= radius * radius) covered++;
                        }
                    }
                }
                if (covered == 0) {
                    pixels[index] = 0;
                    continue;
                }
                int color = pixels[index];
                if (corner && region != IntPtr.Zero && !PtInRegion(region, x, y)) {
                    // PrintWindow fills the area outside the hard region with black.
                    // Extend the edge color into the additional antialias coverage.
                    int stepX = edgeX <= edgeY ? (x < width / 2 ? 1 : -1) : 0;
                    int stepY = edgeX > edgeY ? (y < height / 2 ? 1 : -1) : 0;
                    int sourceX = x + stepX, sourceY = y + stepY;
                    while (sourceX >= 0 && sourceX < width && sourceY >= 0 && sourceY < height &&
                           !PtInRegion(region, sourceX, sourceY)) {
                        sourceX += stepX;
                        sourceY += stepY;
                    }
                    if (sourceX < 0 || sourceX >= width || sourceY < 0 || sourceY >= height)
                        throw new InvalidOperationException("Could not sample the Rustart window edge.");
                    color = pixels[sourceY * width + sourceX];
                }
                if (color == unpainted)
                    throw new InvalidOperationException("Rustart left part of the capture unpainted.");
                int alpha = (covered * 255 + samples * samples / 2) / (samples * samples);
                pixels[index] = (color & rgbMask) | (alpha << 24);
            }
        }
    }
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

using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Principal;

internal static class LaunchProbe
{
    [DllImport("kernel32")] private static extern IntPtr GetCurrentProcess();
    [DllImport("advapi32", SetLastError = true)] private static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32", SetLastError = true)] private static extern bool GetTokenInformation(IntPtr token, int kind, IntPtr output, int size, out int needed);
    [DllImport("advapi32")] private static extern IntPtr GetSidSubAuthorityCount(IntPtr sid);
    [DllImport("advapi32")] private static extern IntPtr GetSidSubAuthority(IntPtr sid, uint index);
    [DllImport("kernel32")] private static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32")] private static extern IntPtr GetStdHandle(int id);
    [DllImport("kernel32")] private static extern bool GetConsoleMode(IntPtr handle, out uint mode);
    [DllImport("kernel32")] private static extern IntPtr GetConsoleWindow();
    [DllImport("kernel32", CharSet = CharSet.Unicode)] private static extern void GetStartupInfo(ref StartupInfo info);

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct StartupInfo
    {
        public uint Size;
        public IntPtr Reserved, Desktop, Title;
        public uint X, Y, XSize, YSize, XChars, YChars, FillAttributes, Flags;
        public ushort Show, ReservedSize;
        public IntPtr Reserved2, Input, Output, Error;
    }

    private static int Main(string[] args)
    {
        if (args.Length == 0 || (args.Length != 1 && args[1] != "--privilege-probe")) return 1;
        var marker = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "ran-" + args[0] + ".marker");
        var temporary = marker + ".tmp";
        if (args.Length == 1) File.WriteAllBytes(temporary, new byte[0]);
        else File.WriteAllLines(temporary, Privileges(args));
        File.Move(temporary, marker);
        return 0;
    }

    private static string[] Privileges(string[] args)
    {
        IntPtr token;
        if (!OpenProcessToken(GetCurrentProcess(), 8, out token))
            throw new System.ComponentModel.Win32Exception();
        int elevation, integrity;
        try
        {
            elevation = ReadToken(token, 20, delegate (IntPtr info) { return Marshal.ReadInt32(info); });
            integrity = ReadToken(token, 25, delegate (IntPtr info) {
                var sid = Marshal.ReadIntPtr(info);
                var count = Marshal.ReadByte(GetSidSubAuthorityCount(sid));
                return Marshal.ReadInt32(GetSidSubAuthority(sid, (uint)(count - 1)));
            });
        }
        finally { CloseHandle(token); }
        uint mode;
        var startup = new StartupInfo { Size = (uint)Marshal.SizeOf(typeof(StartupInfo)) };
        GetStartupInfo(ref startup);
        var tail = new string[args.Length - 2];
        Array.Copy(args, 2, tail, 0, tail.Length);
        return new[] {
            "Elevation=" + elevation,
            "Integrity=" + integrity,
            "Sid=" + WindowsIdentity.GetCurrent().User.Value,
            "WorkingDirectory=" + Environment.CurrentDirectory,
            "Arguments=" + String.Join("|", tail),
            "ConsoleInput=" + GetConsoleMode(GetStdHandle(-10), out mode),
            "ConsoleOutput=" + GetConsoleMode(GetStdHandle(-11), out mode),
            "ConsoleWindow=" + (GetConsoleWindow() != IntPtr.Zero),
            "ShowWindow=" + ((startup.Flags & 1) != 0 ? startup.Show.ToString() : "default")
        };
    }

    private static int ReadToken(IntPtr token, int kind, Func<IntPtr, int> read)
    {
        int size;
        GetTokenInformation(token, kind, IntPtr.Zero, 0, out size);
        if (size <= 0) throw new System.ComponentModel.Win32Exception();
        var buffer = Marshal.AllocHGlobal(size);
        try
        {
            if (!GetTokenInformation(token, kind, buffer, size, out size))
                throw new System.ComponentModel.Win32Exception();
            return read(buffer);
        }
        finally { Marshal.FreeHGlobal(buffer); }
    }
}

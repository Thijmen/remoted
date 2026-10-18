// Win32 input/window helper for the rd Desktop e2e test (loaded by win-input.ps1 with Add-Type).
//
// Windows PowerShell 5.1 compiles this with the .NET Framework C# compiler: C# 5 only (no string
// interpolation, no `?.`, no expression-bodied members). `mcs -langversion:5` checks it elsewhere.
//
//   Canary.Run(outDir)  can this session take synthetic input? (foreground, SendInput, LL hooks)
//   Server.Run(outDir)  JSON-lines RPC on stdin/stdout for tests/desktop-e2e/run.ts

using System;
using System.Collections;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Web.Script.Serialization;
using System.Windows.Forms;

namespace RdE2E
{
    public static class N
    {
        [StructLayout(LayoutKind.Sequential)]
        public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
        [StructLayout(LayoutKind.Sequential)]
        public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
        [StructLayout(LayoutKind.Sequential)]
        public struct HARDWAREINPUT { public uint uMsg; public ushort wParamL; public ushort wParamH; }
        [StructLayout(LayoutKind.Explicit)]
        public struct InputUnion
        {
            [FieldOffset(0)] public MOUSEINPUT mi;
            [FieldOffset(0)] public KEYBDINPUT ki;
            [FieldOffset(0)] public HARDWAREINPUT hi;
        }
        /// 40 bytes on x64; SendInput returns 0 unless cbSize matches.
        [StructLayout(LayoutKind.Sequential)]
        public struct INPUT { public uint type; public InputUnion u; }
        [StructLayout(LayoutKind.Sequential)]
        public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
        [StructLayout(LayoutKind.Sequential)]
        public struct POINT { public int X; public int Y; }
        [StructLayout(LayoutKind.Sequential)]
        public struct KBDLLHOOKSTRUCT { public uint vkCode; public uint scanCode; public uint flags; public uint time; public IntPtr dwExtraInfo; }
        [StructLayout(LayoutKind.Sequential)]
        public struct MSG { public IntPtr hwnd; public uint message; public IntPtr wParam; public IntPtr lParam; public uint time; public POINT pt; }
        [StructLayout(LayoutKind.Sequential)]
        public struct GUITHREADINFO
        {
            public int cbSize; public int flags;
            public IntPtr hwndActive; public IntPtr hwndFocus; public IntPtr hwndCapture;
            public IntPtr hwndMenuOwner; public IntPtr hwndMoveSize; public IntPtr hwndCaret;
            public RECT rcCaret;
        }

        public delegate IntPtr HookProc(int code, IntPtr wParam, IntPtr lParam);
        public delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr lParam);

        [DllImport("user32.dll", SetLastError = true)] public static extern uint SendInput(uint n, INPUT[] inputs, int cb);
        [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
        [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
        [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
        [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
        [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
        [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc f, IntPtr l);
        [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr parent, EnumWindowsProc f, IntPtr l);
        [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
        [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
        [DllImport("user32.dll", SetLastError = true)] public static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
        [DllImport("user32.dll")] public static extern bool CloseDesktop(IntPtr h);
        [DllImport("user32.dll")] public static extern IntPtr GetThreadDesktop(uint tid);
        [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        public static extern bool GetUserObjectInformation(IntPtr h, int index, StringBuilder buf, int len, out int needed);
        [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
        [DllImport("user32.dll", SetLastError = true)] public static extern bool SystemParametersInfo(uint action, uint param, IntPtr pv, uint winIni);
        [DllImport("user32.dll", EntryPoint = "SystemParametersInfo", SetLastError = true)]
        public static extern bool SystemParametersInfoGet(uint action, uint param, out uint pv, uint winIni);
        [DllImport("user32.dll", SetLastError = true)] public static extern IntPtr SetWindowsHookEx(int id, HookProc proc, IntPtr mod, uint tid);
        [DllImport("user32.dll")] public static extern bool UnhookWindowsHookEx(IntPtr h);
        [DllImport("user32.dll")] public static extern IntPtr CallNextHookEx(IntPtr h, int code, IntPtr w, IntPtr l);
        [DllImport("user32.dll")] public static extern int GetMessage(out MSG m, IntPtr h, uint min, uint max);
        [DllImport("user32.dll")] public static extern bool PeekMessage(out MSG m, IntPtr h, uint min, uint max, uint remove);
        [DllImport("user32.dll")] public static extern bool PostThreadMessage(uint tid, uint msg, IntPtr w, IntPtr l);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr GetModuleHandle(string name);
        [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
        [DllImport("user32.dll")] public static extern bool GetGUIThreadInfo(uint tid, ref GUITHREADINFO info);
        [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
        [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    }

    /// Low-level keyboard and mouse hooks on a thread of their own (with its own message loop).
    /// The newest LL hook runs first, so a hook installed by this helper before the app's sees
    /// only the events the app's hook let through (a swallowed key never reaches it).
    public class HookProbe
    {
        private readonly Stopwatch clock;
        private readonly List<Dictionary<string, object>> keys = new List<Dictionary<string, object>>();
        private Thread thread;
        private uint tid;
        // Referenced from fields so the GC never collects a delegate Windows still calls.
        private N.HookProc kbProc;
        private N.HookProc msProc;
        public string Error = "";
        public int MouseButtons;

        public HookProbe(Stopwatch clock) { this.clock = clock; }

        public void Start()
        {
            var ready = new ManualResetEvent(false);
            thread = new Thread(() =>
            {
                N.MSG m;
                N.PeekMessage(out m, IntPtr.Zero, 0, 0, 0); // create the message queue
                tid = N.GetCurrentThreadId();
                kbProc = OnKey;
                msProc = OnMouse;
                IntPtr mod = N.GetModuleHandle(null);
                IntPtr kb = N.SetWindowsHookEx(13 /* WH_KEYBOARD_LL */, kbProc, mod, 0);
                if (kb == IntPtr.Zero) Error += "SetWindowsHookEx(WH_KEYBOARD_LL) failed: " + Marshal.GetLastWin32Error() + "; ";
                IntPtr ms = N.SetWindowsHookEx(14 /* WH_MOUSE_LL */, msProc, mod, 0);
                if (ms == IntPtr.Zero) Error += "SetWindowsHookEx(WH_MOUSE_LL) failed: " + Marshal.GetLastWin32Error() + "; ";
                ready.Set();
                while (N.GetMessage(out m, IntPtr.Zero, 0, 0) > 0) { }
                if (kb != IntPtr.Zero) N.UnhookWindowsHookEx(kb);
                if (ms != IntPtr.Zero) N.UnhookWindowsHookEx(ms);
            });
            thread.IsBackground = true;
            thread.Start();
            ready.WaitOne(5000);
        }

        public void Stop()
        {
            N.PostThreadMessage(tid, 0x0012 /* WM_QUIT */, IntPtr.Zero, IntPtr.Zero);
        }

        private IntPtr OnKey(int code, IntPtr w, IntPtr l)
        {
            if (code >= 0)
            {
                var k = (N.KBDLLHOOKSTRUCT)Marshal.PtrToStructure(l, typeof(N.KBDLLHOOKSTRUCT));
                var e = new Dictionary<string, object>();
                e["t"] = clock.ElapsedMilliseconds;
                e["vk"] = (int)k.vkCode;
                e["sc"] = (int)k.scanCode;
                e["up"] = (k.flags & 0x80) != 0;
                e["injected"] = (k.flags & 0x10) != 0;
                e["ext"] = (k.flags & 0x01) != 0;
                lock (keys) keys.Add(e);
            }
            return N.CallNextHookEx(IntPtr.Zero, code, w, l);
        }

        private IntPtr OnMouse(int code, IntPtr w, IntPtr l)
        {
            if (code >= 0)
            {
                int msg = w.ToInt32();
                if (msg == 0x0201 || msg == 0x0202) Interlocked.Increment(ref MouseButtons); // L down/up
            }
            return N.CallNextHookEx(IntPtr.Zero, code, w, l);
        }

        /// Key events seen since the last call.
        public List<Dictionary<string, object>> Take()
        {
            lock (keys)
            {
                var r = new List<Dictionary<string, object>>(keys);
                keys.Clear();
                return r;
            }
        }
    }

    public static class Input
    {
        public static int Size { get { return Marshal.SizeOf(typeof(N.INPUT)); } }

        private static uint Send(N.INPUT i)
        {
            return N.SendInput(1, new[] { i }, Size);
        }

        public static uint Move(int x, int y)
        {
            int cx = N.GetSystemMetrics(0), cy = N.GetSystemMetrics(1);
            var i = new N.INPUT();
            i.type = 0; // INPUT_MOUSE
            i.u.mi.dx = (int)Math.Round(x * 65535.0 / (cx - 1));
            i.u.mi.dy = (int)Math.Round(y * 65535.0 / (cy - 1));
            i.u.mi.dwFlags = 0x0001 | 0x8000; // MOVE | ABSOLUTE
            return Send(i);
        }

        public static uint Button(bool down)
        {
            var i = new N.INPUT();
            i.type = 0;
            i.u.mi.dwFlags = down ? 0x0002u : 0x0004u; // LEFTDOWN / LEFTUP
            return Send(i);
        }

        public static uint Click(int x, int y)
        {
            uint n = Move(x, y);
            Thread.Sleep(80);
            n += Button(true);
            Thread.Sleep(80);
            n += Button(false);
            return n;
        }

        /// A hardware scan code (KEYEVENTF_SCANCODE; EXTENDEDKEY for E0 keys), like a keyboard.
        public static uint Key(int scan, bool ext, bool up)
        {
            var i = new N.INPUT();
            i.type = 1; // INPUT_KEYBOARD
            i.u.ki.wScan = (ushort)scan;
            i.u.ki.dwFlags = 0x0008u | (ext ? 0x0001u : 0u) | (up ? 0x0002u : 0u);
            return Send(i);
        }

        /// A virtual key only (scan code 0), like some keyboards/drivers and most automation tools.
        public static uint Vk(int vk, bool up)
        {
            var i = new N.INPUT();
            i.type = 1;
            i.u.ki.wVk = (ushort)vk;
            i.u.ki.dwFlags = up ? 0x0002u : 0u;
            return Send(i);
        }
    }

    public static class Win
    {
        public static Dictionary<string, object> Rect(N.RECT r)
        {
            return Xywh(r.Left, r.Top, r.Right - r.Left, r.Bottom - r.Top);
        }

        public static Dictionary<string, object> Xywh(int x, int y, int w, int h)
        {
            var d = new Dictionary<string, object>();
            d["x"] = x; d["y"] = y; d["w"] = w; d["h"] = h;
            return d;
        }

        public static Dictionary<string, object> Describe(IntPtr h)
        {
            var d = new Dictionary<string, object>();
            d["hwnd"] = h.ToInt64();
            if (h == IntPtr.Zero) return d;
            uint pid;
            N.GetWindowThreadProcessId(h, out pid);
            d["pid"] = (long)pid;
            try { d["process"] = Process.GetProcessById((int)pid).ProcessName; } catch (Exception) { }
            var sb = new StringBuilder(512);
            N.GetWindowText(h, sb, 512);
            d["title"] = sb.ToString();
            sb = new StringBuilder(256);
            N.GetClassName(h, sb, 256);
            d["class"] = sb.ToString();
            N.RECT r;
            if (N.GetWindowRect(h, out r)) d["rect"] = Rect(r);
            d["visible"] = N.IsWindowVisible(h);
            return d;
        }

        /// Visible top-level windows (of one process when pid != 0), in z-order.
        public static List<object> TopLevel(long pid)
        {
            var list = new List<object>();
            N.EnumWindows((h, l) =>
            {
                if (!N.IsWindowVisible(h)) return true;
                uint p;
                N.GetWindowThreadProcessId(h, out p);
                if (pid != 0 && p != pid) return true;
                list.Add(Describe(h));
                return true;
            }, IntPtr.Zero);
            return list;
        }

        /// Client area on screen, plus Chromium's render widget windows (the page's area).
        public static Dictionary<string, object> Content(IntPtr top)
        {
            var d = new Dictionary<string, object>();
            N.RECT c;
            N.GetClientRect(top, out c);
            var p = new N.POINT();
            N.ClientToScreen(top, ref p);
            d["client"] = Xywh(p.X, p.Y, c.Right - c.Left, c.Bottom - c.Top);
            var widgets = new List<object>();
            N.EnumChildWindows(top, (h, l) =>
            {
                var sb = new StringBuilder(256);
                N.GetClassName(h, sb, 256);
                if (sb.ToString() == "Chrome_RenderWidgetHostHWND")
                {
                    N.RECT r;
                    N.GetWindowRect(h, out r);
                    var w = Rect(r);
                    w["visible"] = N.IsWindowVisible(h);
                    w["hwnd"] = h.ToInt64();
                    widgets.Add(w);
                }
                return true;
            }, IntPtr.Zero);
            d["widgets"] = widgets;
            return d;
        }

        public static Dictionary<string, object> Foreground()
        {
            var d = Describe(N.GetForegroundWindow());
            var gi = new N.GUITHREADINFO();
            gi.cbSize = Marshal.SizeOf(typeof(N.GUITHREADINFO));
            if (N.GetGUIThreadInfo(0, ref gi))
            {
                d["focus"] = Describe(gi.hwndFocus);
                d["active"] = gi.hwndActive.ToInt64();
            }
            return d;
        }

        public static Dictionary<string, object> At(int x, int y)
        {
            var p = new N.POINT();
            p.X = x; p.Y = y;
            return Describe(N.WindowFromPoint(p));
        }

        public static bool Place(IntPtr h, int x, int y, int w, int hgt)
        {
            N.ShowWindow(h, 9 /* SW_RESTORE */);
            // SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW
            return N.SetWindowPos(h, IntPtr.Zero, x, y, w, hgt, 0x0004 | 0x0010 | 0x0040);
        }

        public static string Screenshot(string path)
        {
            try
            {
                var b = SystemInformation.VirtualScreen;
                Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(path)));
                using (var bmp = new Bitmap(b.Width, b.Height))
                using (var g = Graphics.FromImage(bmp))
                {
                    g.CopyFromScreen(b.Left, b.Top, 0, 0, bmp.Size);
                    bmp.Save(path, ImageFormat.Png);
                }
                return "";
            }
            catch (Exception e)
            {
                return e.GetType().Name + ": " + e.Message;
            }
        }

        public static string DesktopName(IntPtr desk)
        {
            if (desk == IntPtr.Zero) return "(none)";
            var sb = new StringBuilder(256);
            int needed;
            return N.GetUserObjectInformation(desk, 2 /* UOI_NAME */, sb, sb.Capacity * 2, out needed)
                ? sb.ToString()
                : "(GetUserObjectInformation failed: " + Marshal.GetLastWin32Error() + ")";
        }

        public static string InputDesktop()
        {
            IntPtr d = N.OpenInputDesktop(0, false, 0x0100 /* DESKTOP_SWITCHDESKTOP */);
            if (d == IntPtr.Zero) return "(OpenInputDesktop failed: " + Marshal.GetLastWin32Error() + ")";
            string name = DesktopName(d);
            N.CloseDesktop(d);
            return name;
        }

        public static Dictionary<string, object> ForegroundLock(bool setZero)
        {
            var d = new Dictionary<string, object>();
            uint v;
            d["before_ms"] = N.SystemParametersInfoGet(0x2000 /* SPI_GETFOREGROUNDLOCKTIMEOUT */, 0, out v, 0) ? (long)v : -1L;
            if (setZero)
            {
                d["set0"] = N.SystemParametersInfo(0x2001 /* SPI_SETFOREGROUNDLOCKTIMEOUT */, 0, IntPtr.Zero, 0x2 /* SPIF_SENDCHANGE */);
                d["set0_error"] = Marshal.GetLastWin32Error();
                d["after_ms"] = N.SystemParametersInfoGet(0x2000, 0, out v, 0) ? (long)v : -1L;
            }
            return d;
        }
    }

    public static class Procs
    {
        private static readonly Dictionary<int, Process> started = new Dictionary<int, Process>();

        /// Start a process without a console window and drain stdout+stderr into `log`. A debug
        /// rd-desktop.exe is a console app: an undrained pipe would block its logging (and the hook
        /// thread with it, until Windows silently drops the hook).
        public static int Launch(string exe, string args, IEnumerable env, string cwd, string log)
        {
            var si = new ProcessStartInfo(exe, args ?? "");
            si.UseShellExecute = false;
            si.CreateNoWindow = true;
            si.RedirectStandardOutput = true;
            si.RedirectStandardError = true;
            si.RedirectStandardInput = true;
            if (!string.IsNullOrEmpty(cwd)) si.WorkingDirectory = cwd;
            if (env != null)
            {
                foreach (object o in env)
                {
                    string kv = (string)o;
                    int i = kv.IndexOf('=');
                    if (i > 0) si.EnvironmentVariables[kv.Substring(0, i)] = kv.Substring(i + 1);
                }
            }
            Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(log)));
            var w = new StreamWriter(log, false, new UTF8Encoding(false));
            w.AutoFlush = true;
            DataReceivedEventHandler h = (s, e) =>
            {
                if (e.Data == null) return;
                lock (w) { w.WriteLine(e.Data); }
            };
            var p = new Process();
            p.StartInfo = si;
            p.OutputDataReceived += h;
            p.ErrorDataReceived += h;
            p.Start();
            p.StandardInput.Close();
            p.BeginOutputReadLine();
            p.BeginErrorReadLine();
            lock (started) started[p.Id] = p;
            return p.Id;
        }

        public static string Kill(int pid)
        {
            var si = new ProcessStartInfo("taskkill", "/T /F /PID " + pid);
            si.UseShellExecute = false;
            si.CreateNoWindow = true;
            si.RedirectStandardOutput = true;
            si.RedirectStandardError = true;
            using (var p = Process.Start(si))
            {
                string o = p.StandardOutput.ReadToEnd() + p.StandardError.ReadToEnd();
                p.WaitForExit(15000);
                return o.Trim();
            }
        }

        public static bool Alive(int pid)
        {
            try { return !Process.GetProcessById(pid).HasExited; }
            catch (Exception) { return false; }
        }
    }

    public static class Canary
    {
        private static void Pump(int ms)
        {
            var sw = Stopwatch.StartNew();
            while (sw.ElapsedMilliseconds < ms)
            {
                Application.DoEvents();
                Thread.Sleep(10);
            }
        }

        /// A WinForms window must become foreground on a SendInput click, take a SendInput key,
        /// and a low-level hook must see both. Returns JSON with "ok".
        public static string Run(string outDir)
        {
            N.SetProcessDPIAware();
            Directory.CreateDirectory(outDir);
            var r = new Dictionary<string, object>();
            r["session_id"] = Process.GetCurrentProcess().SessionId;
            r["user_interactive"] = Environment.UserInteractive;
            r["screen"] = N.GetSystemMetrics(0) + "x" + N.GetSystemMetrics(1);
            r["input_struct_size"] = Input.Size;
            r["thread_desktop"] = Win.DesktopName(N.GetThreadDesktop(N.GetCurrentThreadId()));
            r["input_desktop"] = Win.InputDesktop();
            r["foreground_before"] = Win.Foreground();
            r["windows_before"] = Win.TopLevel(0);
            r["foreground_lock"] = Win.ForegroundLock(true);

            var probe = new HookProbe(Stopwatch.StartNew());
            probe.Start();
            r["hook_error"] = probe.Error;

            var form = new Form();
            form.Text = "rd-e2e canary";
            form.StartPosition = FormStartPosition.Manual;
            form.Location = new Point(80, 80);
            form.Size = new Size(420, 240);
            var tb = new TextBox();
            tb.Multiline = true;
            tb.Dock = DockStyle.Fill;
            tb.Font = new Font("Segoe UI", 14);
            form.Controls.Add(tb);
            form.Show();
            Pump(800);
            r["fg_after_show"] = N.GetForegroundWindow() == form.Handle;
            r["screenshot_1"] = Win.Screenshot(Path.Combine(outDir, "canary-1-shown.png"));

            var pt = tb.PointToScreen(new Point(tb.Width / 2, tb.Height / 2));
            r["window_at_click_point"] = Win.At(pt.X, pt.Y);
            int mouseBefore = probe.MouseButtons;
            uint clicks = Input.Click(pt.X, pt.Y);
            r["click_sent"] = (long)clicks;
            r["click_error"] = clicks == 3 ? 0 : Marshal.GetLastWin32Error();
            Pump(600);
            r["hook_mouse_buttons"] = probe.MouseButtons - mouseBefore;
            r["fg_after_click"] = N.GetForegroundWindow() == form.Handle;
            r["foreground_after_click"] = Win.Foreground();
            r["textbox_focused"] = tb.Focused;

            probe.Take();
            uint keys = Input.Key(0x1E, false, false);
            Thread.Sleep(50);
            keys += Input.Key(0x1E, false, true);
            r["key_sent"] = (long)keys;
            Pump(600);
            r["text_after_scancode_a"] = tb.Text;
            var events = probe.Take();
            r["hook_key_events"] = events;
            int injectedA = 0;
            foreach (var e in events)
                if ((int)e["sc"] == 0x1E && (bool)e["injected"]) injectedA++;

            // A VK-only key (what many automation tools send): which scan code does the hook see?
            Input.Vk(0x42 /* B */, false);
            Thread.Sleep(50);
            Input.Vk(0x42, true);
            Pump(400);
            r["text_after_vk_b"] = tb.Text;
            r["hook_vk_events"] = probe.Take();
            r["screenshot_2"] = Win.Screenshot(Path.Combine(outDir, "canary-2-typed.png"));

            bool ok = (bool)r["fg_after_click"] && tb.Text.StartsWith("a") && injectedA >= 2
                && probe.Error.Length == 0;
            r["ok"] = ok;
            form.Close();
            probe.Stop();
            var json = new JavaScriptSerializer();
            json.MaxJsonLength = int.MaxValue;
            return json.Serialize(r);
        }
    }

    public static class Server
    {
        private static int I(Dictionary<string, object> c, string k, int def)
        {
            object v;
            return c.TryGetValue(k, out v) && v != null ? Convert.ToInt32(v) : def;
        }

        private static string S(Dictionary<string, object> c, string k)
        {
            object v;
            return c.TryGetValue(k, out v) && v != null ? v.ToString() : null;
        }

        private static bool B(Dictionary<string, object> c, string k)
        {
            object v;
            return c.TryGetValue(k, out v) && v != null && Convert.ToBoolean(v);
        }

        private static IntPtr H(Dictionary<string, object> c)
        {
            return new IntPtr(Convert.ToInt64(c["hwnd"]));
        }

        /// One JSON command per stdin line, one JSON answer per stdout line ({id, ok, result|error}).
        public static void Run(string outDir)
        {
            N.SetProcessDPIAware();
            var clock = Stopwatch.StartNew();
            var probe = new HookProbe(clock);
            probe.Start();
            probes["early"] = probe;
            var json = new JavaScriptSerializer();
            json.MaxJsonLength = int.MaxValue;
            var stdin = new StreamReader(Console.OpenStandardInput(), new UTF8Encoding(false));
            var stdout = new StreamWriter(Console.OpenStandardOutput(), new UTF8Encoding(false));
            stdout.AutoFlush = true;

            var hello = new Dictionary<string, object>();
            hello["ready"] = true;
            hello["hook_error"] = probe.Error;
            hello["session_id"] = Process.GetCurrentProcess().SessionId;
            hello["screen"] = N.GetSystemMetrics(0) + "x" + N.GetSystemMetrics(1);
            hello["input_desktop"] = Win.InputDesktop();
            stdout.WriteLine(json.Serialize(hello));

            string line;
            while ((line = stdin.ReadLine()) != null)
            {
                if (line.Trim().Length == 0) continue;
                var resp = new Dictionary<string, object>();
                object id = null;
                string cmd = null;
                try
                {
                    var c = (Dictionary<string, object>)json.DeserializeObject(line);
                    if (c.ContainsKey("id")) id = c["id"];
                    cmd = S(c, "cmd");
                    resp["result"] = Dispatch(c, probe, clock);
                    resp["ok"] = true;
                }
                catch (Exception e)
                {
                    resp["ok"] = false;
                    resp["error"] = e.GetType().Name + ": " + e.Message;
                }
                resp["id"] = id;
                stdout.WriteLine(json.Serialize(resp));
                if (cmd == "exit") break;
            }
            probe.Stop();
        }

        private static readonly Dictionary<string, HookProbe> probes = new Dictionary<string, HookProbe>();

        private static HookProbe Probe(Dictionary<string, object> c)
        {
            string name = S(c, "name") ?? "early";
            HookProbe p;
            if (!probes.TryGetValue(name, out p)) throw new ArgumentException("no hook probe " + name);
            return p;
        }

        private static object Dispatch(Dictionary<string, object> c, HookProbe probe, Stopwatch clock)
        {
            string cmd = S(c, "cmd");
            switch (cmd)
            {
                case "ping":
                    return clock.ElapsedMilliseconds;
                case "exit":
                    return true;
                case "launch":
                    {
                        object env;
                        c.TryGetValue("env", out env);
                        return Procs.Launch(S(c, "exe"), S(c, "args"), env as IEnumerable, S(c, "cwd"), S(c, "log"));
                    }
                case "kill":
                    return Procs.Kill(I(c, "pid", 0));
                case "alive":
                    return Procs.Alive(I(c, "pid", 0));
                case "windows":
                    return Win.TopLevel(I(c, "pid", 0));
                case "content":
                    return Win.Content(H(c));
                case "place":
                    return Win.Place(H(c), I(c, "x", 0), I(c, "y", 0), I(c, "w", 1000), I(c, "h", 700));
                case "foreground":
                    return Win.Foreground();
                case "setForeground":
                    {
                        var d = new Dictionary<string, object>();
                        d["returned"] = N.SetForegroundWindow(H(c));
                        Thread.Sleep(200);
                        d["foreground"] = Win.Foreground();
                        return d;
                    }
                case "at":
                    return Win.At(I(c, "x", 0), I(c, "y", 0));
                case "click":
                    return (long)Input.Click(I(c, "x", 0), I(c, "y", 0));
                case "move":
                    return (long)Input.Move(I(c, "x", 0), I(c, "y", 0));
                case "keys":
                    {
                        // seq: [{sc, ext?, up?} | {vk, up?}], gapMs between events
                        int gap = I(c, "gapMs", 60);
                        long sent = 0;
                        bool first = true;
                        foreach (object o in (IEnumerable)c["seq"])
                        {
                            var k = (Dictionary<string, object>)o;
                            if (!first) Thread.Sleep(gap);
                            first = false;
                            sent += k.ContainsKey("vk")
                                ? Input.Vk(I(k, "vk", 0), B(k, "up"))
                                : Input.Key(I(k, "sc", 0), B(k, "ext"), B(k, "up"));
                        }
                        return sent;
                    }
                case "hook":
                    {
                        // "early": installed when this helper started (before the app's hook, so it
                        // runs after it); others: hookStart'ed later (they run before it).
                        var p = Probe(c);
                        var d = new Dictionary<string, object>();
                        d["keys"] = p.Take();
                        d["mouse_buttons"] = p.MouseButtons;
                        d["error"] = p.Error;
                        d["now"] = clock.ElapsedMilliseconds;
                        return d;
                    }
                case "hookStart":
                    {
                        string name = S(c, "name");
                        HookProbe old;
                        if (probes.TryGetValue(name, out old)) old.Stop();
                        var p = new HookProbe(clock);
                        p.Start();
                        probes[name] = p;
                        return p.Error;
                    }
                case "hookStop":
                    {
                        string name = S(c, "name");
                        HookProbe old;
                        if (probes.TryGetValue(name, out old)) { old.Stop(); probes.Remove(name); }
                        return true;
                    }
                case "screenshot":
                    return Win.Screenshot(S(c, "path"));
                case "foregroundLock":
                    return Win.ForegroundLock(B(c, "setZero"));
                default:
                    throw new ArgumentException("unknown cmd " + cmd);
            }
        }
    }
}

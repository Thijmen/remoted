//! Draws the tray [`Model`] with `tray-icon`. The icon and its menu live on the main thread,
//! which runs the platform's event loop (macOS: `NSApplication`; Windows: a message loop);
//! [`run`] starts the agent on another thread and returns its result once it is done.

use std::cell::RefCell;

use tokio::sync::mpsc;
use tracing::warn;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use super::icon::{self, Rgba};
use super::{Action, IconState, Item, Model};

/// How long the event loop gets to end after the agent stopped.
const EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The agent's handle on the tray.
pub struct Ui {
    main: MainThread,
    actions: mpsc::UnboundedReceiver<Action>,
}

impl Ui {
    /// Shows `model` (from any thread).
    pub fn show(&self, model: Model) {
        self.main
            .post(move || STATE.with(|state| state.borrow_mut().show(model)));
    }

    /// The next menu click; `None` once the tray is gone.
    pub async fn next_action(&mut self) -> Option<Action> {
        self.actions.recv().await
    }
}

/// Whether a tray icon can be shown: there is a desktop session to show it in.
pub fn available() -> bool {
    platform::available()
}

/// Runs the event loop on this (the main) thread and `work` on a new one; returns what `work`
/// returned once it did. A panic in `work` is passed on.
pub fn run<T: Send + 'static>(work: impl FnOnce(Ui) -> T + Send + 'static) -> T {
    let main = platform::init();
    let (tx, actions) = mpsc::unbounded_channel();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(action) = Action::from_id(event.id.as_ref()) {
            let _ = tx.send(action);
        }
    }));
    let ui = Ui {
        main: main.clone(),
        actions,
    };
    let stopper = main.clone();
    let worker = std::thread::Builder::new()
        .name("rd-agent-main".into())
        .spawn(move || {
            // Ends the event loop when `work` returns or panics.
            struct StopLoop(MainThread);
            impl Drop for StopLoop {
                fn drop(&mut self) {
                    self.0.post(platform::stop);
                    // An open menu runs its own loop until it closes; nobody may be there to
                    // close it (the agent stopped for an update, or remotely).
                    std::thread::spawn(|| {
                        std::thread::sleep(EXIT_WAIT);
                        warn!("the tray did not close in time; exiting");
                        std::process::exit(0);
                    });
                }
            }
            let _stop = StopLoop(stopper);
            work(ui)
        })
        .expect("starting the agent thread");
    platform::run_loop();
    MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
    // Dropped here, on its own thread, before the process exits.
    STATE.with(|state| state.borrow_mut().tray = None);
    match worker.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

thread_local! {
    /// The tray icon, on the main thread.
    static STATE: RefCell<State> = RefCell::new(State::default());
}

#[derive(Default)]
struct State {
    tray: Option<TrayIcon>,
    base: Option<Rgba>,
    icon: Option<IconState>,
    /// Creating the icon failed; the agent runs without it.
    failed: bool,
}

impl State {
    fn show(&mut self, model: Model) {
        if self.failed {
            return;
        }
        if let Err(e) = self.try_show(model) {
            warn!("the tray icon is not available: {e:#}");
            self.failed = true;
            self.tray = None;
        }
    }

    fn try_show(&mut self, model: Model) -> anyhow::Result<()> {
        let base = match &self.base {
            Some(base) => base,
            None => self.base.insert(icon::base()?),
        };
        let picture = icon::for_state(base, model.icon);
        let picture = Icon::from_rgba(picture.pixels, picture.width, picture.height)?;
        let menu = menu(&model.items)?;
        match &self.tray {
            None => {
                let builder = TrayIconBuilder::new()
                    .with_id("rd-agent")
                    .with_tooltip(&model.tooltip)
                    .with_menu(Box::new(menu))
                    .with_menu_on_left_click(true);
                #[cfg(target_os = "macos")]
                let builder = builder.with_icon_templated(picture);
                #[cfg(windows)]
                let builder = builder.with_icon(picture);
                self.tray = Some(builder.build()?);
            }
            Some(tray) => {
                tray.set_menu(Some(Box::new(menu)));
                tray.set_tooltip(Some(&model.tooltip))?;
                if self.icon != Some(model.icon) {
                    #[cfg(target_os = "macos")]
                    tray.set_icon_templated(Some(picture))?;
                    #[cfg(windows)]
                    tray.set_icon(Some(picture))?;
                }
            }
        }
        self.icon = Some(model.icon);
        Ok(())
    }
}

fn menu(items: &[Item]) -> anyhow::Result<Menu> {
    let menu = Menu::new();
    for item in items {
        match item {
            Item::Label(text) => menu.append(&MenuItem::new(text, false, None))?,
            Item::Button {
                action,
                text,
                enabled,
            } => menu.append(&MenuItem::with_id(action.id(), text, *enabled, None))?,
            Item::Check {
                action,
                text,
                enabled,
                checked,
            } => menu.append(&CheckMenuItem::with_id(
                action.id(),
                text,
                *enabled,
                *checked,
                None,
            ))?,
            Item::Separator => menu.append(&PredefinedMenuItem::separator())?,
        }
    }
    Ok(menu)
}

/// Runs closures on the main thread.
#[derive(Clone)]
struct MainThread(platform::Poster);

impl MainThread {
    fn post(&self, f: impl FnOnce() + Send + 'static) {
        self.0.post(Box::new(f));
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
    };
    use objc2_foundation::NSPoint;

    use super::MainThread;

    pub fn available() -> bool {
        // No window server session (e.g. started over ssh): nowhere to show an icon.
        rd_platform_macos::session::lock_state() != rd_agent_core::LockState::Unknown
    }

    #[derive(Clone)]
    pub struct Poster;

    impl Poster {
        pub fn post(&self, f: Box<dyn FnOnce() + Send>) {
            dispatch2::DispatchQueue::main().exec_async(f);
        }
    }

    pub fn init() -> MainThread {
        let mtm = MainThreadMarker::new().expect("the tray runs on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        // No Dock icon or app menu (RD Agent.app also says so with LSUIElement).
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        MainThread(Poster)
    }

    pub fn run_loop() {
        let mtm = MainThreadMarker::new().expect("the tray runs on the main thread");
        NSApplication::sharedApplication(mtm).run();
    }

    /// Ends [`run_loop`] (on the main thread).
    pub fn stop() {
        let mtm = MainThreadMarker::new().expect("stop runs on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.stop(None);
        // `stop` takes effect after the next event: post one.
        let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            0,
            0,
            0,
        );
        if let Some(event) = event {
            app.postEvent_atStart(&event, true);
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::collections::VecDeque;
    use std::sync::{Mutex, OnceLock};

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
        PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WM_APP,
        WNDCLASSW, WS_OVERLAPPED,
    };
    use windows::core::w;

    use super::MainThread;

    /// Asks the main thread's window to run the queued closures.
    const WM_RUN: u32 = WM_APP + 1;

    type Job = Box<dyn FnOnce() + Send>;

    fn queue() -> &'static Mutex<VecDeque<Job>> {
        static QUEUE: OnceLock<Mutex<VecDeque<Job>>> = OnceLock::new();
        QUEUE.get_or_init(Mutex::default)
    }

    pub fn available() -> bool {
        true
    }

    /// Posts to a message-only window rather than to the thread: thread messages are lost
    /// while a modal loop runs (an open menu), window messages are not.
    #[derive(Clone)]
    pub struct Poster(isize);

    impl Poster {
        pub fn post(&self, f: Job) {
            queue()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(f);
            // SAFETY: the window lives until the process exits.
            let _ =
                unsafe { PostMessageW(Some(HWND(self.0 as *mut _)), WM_RUN, WPARAM(0), LPARAM(0)) };
        }
    }

    thread_local! {
        static RUNNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        // Not re-entered from a job that pumps messages: the outer loop runs what is queued.
        if msg == WM_RUN && !RUNNING.replace(true) {
            loop {
                let job = queue()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pop_front();
                match job {
                    Some(job) => job(),
                    None => break,
                }
            }
            RUNNING.set(false);
            return LRESULT(0);
        }
        // SAFETY: the default handling for every other message.
        unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }

    pub fn init() -> MainThread {
        // SAFETY: registering a class and creating a message-only window of it, on this
        // thread, which then runs the message loop.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(None).expect("GetModuleHandleW");
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("rd-agent-tray"),
                ..Default::default()
            };
            RegisterClassW(&class);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("rd-agent-tray"),
                w!("rd-agent"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                None,
            )
            .expect("creating the tray's message window")
        };
        MainThread(Poster(hwnd.0 as isize))
    }

    pub fn run_loop() {
        let mut msg = MSG::default();
        // SAFETY: the standard message loop of this thread.
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Ends [`run_loop`] (on the main thread).
    pub fn stop() {
        // SAFETY: posts WM_QUIT to this thread's queue.
        unsafe { PostQuitMessage(0) };
    }
}

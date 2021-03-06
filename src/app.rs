use crate::startup::Startup;
use crate::switcher::{Switcher, VirtualDesktop};
use crate::trayicon::TrayIcon;
use crate::{log_error, log_info, Config, Win32Error};

use anyhow::{anyhow, bail, Result};
use std::ptr::null_mut;
use wchar::{wchar_t, wchz};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, PWSTR, WPARAM};
use windows::Win32::System::Com::{CoInitialize, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_NOREPEAT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, TranslateMessage, CREATESTRUCTW, CW_USEDEFAULT,
    GWL_USERDATA, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COMMAND, WM_CREATE, WM_HOTKEY,
    WM_LBUTTONUP, WM_RBUTTONUP, WM_USER, WNDCLASSW,
};

pub const WM_USER_TRAYICON: u32 = WM_USER + 1;
pub const MENU_CMD_EXIT: u32 = 1;
pub const MENU_CMD_STARTUP: u32 = 2;

pub const NAME: &[wchar_t] = wchz!("Windows Switcher");

pub fn start_app(config: &Config) {
    if let Err(err) = App::start(config) {
        log_error!(&err.to_string());
    }
}

pub struct App {
    trayicon: Option<TrayIcon>,
    startup: Startup,
    hwnd: HWND,
    msg_cb: Option<u32>,
    has_com: bool,
    switcher: Switcher,
}

impl App {
    pub fn start(config: &Config) -> Result<()> {
        let has_com = match Self::init_com() {
            Ok(_) => true,
            Err(err) => {
                log_error!(err.to_string());
                false
            }
        };
        let virtual_desktop = match VirtualDesktop::create() {
            Ok(v) => Some(v),
            Err(err) => {
                log_error!(err.to_string());
                None
            }
        };
        let instance = Self::get_instance()?;

        let name = PWSTR(NAME.as_ptr());

        Self::register_window_class(instance, name)?;

        let trayicon = match config.trayicon {
            true => Some(TrayIcon::create()),
            false => None,
        };
        let startup = Startup::create()?;
        let switcher = Switcher::new(virtual_desktop);

        let app = App {
            trayicon,
            startup,
            hwnd: HWND::default(),
            msg_cb: None,
            has_com,
            switcher,
        };
        let hwnd = Self::create_window(instance, name, app)?;

        Self::regist_hotkey(hwnd, config)?;
        Self::eventloop()
    }

    fn init_com() -> Result<()> {
        unsafe { CoInitialize(null_mut()).map_err(|e| anyhow!("Fail to init com, {}", e)) }
    }

    fn get_instance() -> Result<HINSTANCE> {
        let instance = unsafe { GetModuleHandleW(None) }
            .ok()
            .map_err(|e| anyhow!("Fail to get instance, {}", e))?;

        debug_assert!(instance.0 != 0);
        Ok(instance)
    }

    fn register_window_class(instance: HINSTANCE, name: PWSTR) -> Result<()> {
        let class = WNDCLASSW {
            hInstance: instance,
            lpszClassName: name,
            lpfnWndProc: Some(App::winproc),
            ..Default::default()
        };
        let atom = unsafe { RegisterClassW(&class) };
        if atom == 0 {
            bail!("Fail to register class, {}", Win32Error::from_win32());
        }
        Ok(())
    }

    fn create_window(instance: HINSTANCE, name: PWSTR, app: App) -> Result<HWND> {
        let ptr = Box::into_raw(Box::new(app));
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                name,
                name,
                WINDOW_STYLE(0),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                instance,
                ptr as *mut _,
            )
        }
        .ok()
        .map_err(|e| anyhow!("Fail to create window, {}", e))
    }

    fn regist_hotkey(hwnd: HWND, config: &Config) -> Result<()> {
        unsafe { RegisterHotKey(hwnd, 1, config.hotkey.0 | MOD_NOREPEAT, config.hotkey.1) }
            .ok()
            .map_err(|e| anyhow!("Fail to register hotkey, {}", e))
    }

    fn eventloop() -> Result<()> {
        let mut message = MSG::default();
        loop {
            let ret = unsafe { GetMessageW(&mut message, HWND(0), 0, 0) };
            match ret.0 {
                0 => break,
                -1 => {
                    log_error!("Fail to get message, {}", Win32Error::from_win32());
                }
                _ => unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                },
            }
        }

        Ok(())
    }

    unsafe extern "system" fn winproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match Self::handle_wm(hwnd, msg, wparam, lparam) {
            Ok(ret) => ret,
            Err(err) => {
                log_error!(&err.to_string());
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }

    fn handle_wm(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Result<LRESULT> {
        match msg {
            WM_CREATE => unsafe {
                log_info!("Handle msg=WM_CREATE");
                let create_struct: &mut CREATESTRUCTW = &mut *(lparam.0 as *mut _);
                let app: &mut App = &mut *(create_struct.lpCreateParams as *mut _);
                set_window_ptr(hwnd, app);
                app.hwnd = hwnd;
                if let Some(trayicon) = app.trayicon.as_mut() {
                    trayicon.add(hwnd)?;
                }
                app.msg_cb = {
                    Some(RegisterWindowMessageW(PWSTR(
                        wchz!("TaskbarCreated").as_ptr(),
                    )))
                };
            },
            WM_HOTKEY => {
                log_info!("Handle msg=WM_NOTIFY");
                let app = retrive_app(hwnd)?;
                app.switcher.switch_window()?;
            }
            WM_USER_TRAYICON => {
                let app = retrive_app(hwnd)?;
                if let Some(trayicon) = app.trayicon.as_mut() {
                    let keycode = lparam.0 as u32;
                    if keycode == WM_LBUTTONUP || keycode == WM_RBUTTONUP {
                        log_info!("Handle msg=WM_TAYICON");
                        trayicon.popup(app.startup.is_enable)?;
                    }
                }
                return Ok(LRESULT(0));
            }
            WM_COMMAND => {
                let value = wparam.0 as u32;
                let kind = ((value >> 16) & 0xffff) as u16;
                let id = (value & 0xffff) as u32;
                if kind == 0 {
                    match id {
                        MENU_CMD_EXIT => {
                            log_info!("Handle msg=MENU_CMD_EXIT");
                            unsafe { PostQuitMessage(0) };
                        }
                        MENU_CMD_STARTUP => {
                            log_info!("Handle msg=MENU_CMD_STARTUP");
                            let app = retrive_app(hwnd)?;
                            app.startup.toggle()?;
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                if let Ok(app) = retrive_app(hwnd) {
                    if let Some(msg_id) = app.msg_cb {
                        if msg == msg_id {
                            if let Some(trayicon) = app.trayicon.as_mut() {
                                trayicon.add(hwnd)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if self.has_com {
            unsafe { CoUninitialize() }
        }
    }
}

fn retrive_app(hwnd: HWND) -> Result<&'static mut App> {
    unsafe {
        let ptr = get_window_ptr(hwnd);
        if ptr == 0 {
            bail!("Fail to get app from win ptr");
        }
        let tx: &mut App = &mut *(ptr as *mut _);
        Ok(tx)
    }
}

#[cfg(target_arch = "x86")]
fn get_window_ptr(hwnd: HWND) -> i32 {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(hwnd, GWL_USERDATA) }
}
#[cfg(target_arch = "x86_64")]
fn get_window_ptr(hwnd: HWND) -> isize {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWL_USERDATA) }
}

#[cfg(target_arch = "x86")]
fn set_window_ptr(hwnd: HWND, app: &mut App) {
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongW(
            hwnd,
            GWL_USERDATA,
            app as *mut _ as _,
        )
    };
}

#[cfg(target_arch = "x86_64")]
fn set_window_ptr(hwnd: HWND, app: &mut App) {
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            hwnd,
            GWL_USERDATA,
            app as *mut _ as _,
        )
    };
}

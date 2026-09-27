//! Tray icon (Windows) / menu bar item (macOS). Runs the native event loop on
//! the main thread; the async runtime lives on worker threads.

use crate::state::{AppRef, TrayStatus};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

enum UserEvent {
    Status(TrayStatus),
    Menu(MenuEvent),
    Tray(TrayIconEvent),
}

/// Two circular arrows; blue while transferring, green when idle.
/// On macOS it is drawn black and used as a template image.
fn make_icon(busy: bool) -> Icon {
    const N: u32 = 32;
    let (r, g, b) = if cfg!(target_os = "macos") {
        (0, 0, 0)
    } else if busy {
        (0x0b, 0x5c, 0xd5)
    } else {
        (0x1e, 0x7a, 0x3c)
    };
    let mut px = vec![0u8; (N * N * 4) as usize];
    let c = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let d = (dx * dx + dy * dy).sqrt();
            let ang = dy.atan2(dx).to_degrees();
            // Ring with two gaps (arrow tails) and a solid dot while busy.
            let ring = (9.5..=14.5).contains(&d) && !((-20.0..=10.0).contains(&ang) || (160.0..=180.0).contains(&ang) || (-180.0..=-170.0).contains(&ang));
            let head1 = dx > 8.0 && dx < 16.0 && dy > -2.0 && dy < 6.0 && (dy + 2.0) < (16.0 - dx);
            let head2 = dx < -8.0 && dx > -16.0 && dy < 2.0 && dy > -6.0 && (2.0 - dy) < (16.0 + dx);
            let dot = busy && d <= 4.5;
            if ring || head1 || head2 || dot {
                let i = ((y * N + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
    }
    Icon::from_rgba(px, N, N).expect("icon")
}

fn tooltip(st: &TrayStatus) -> String {
    let mut t = format!("SyncMe: {}", st.status);
    if !st.last.is_empty() {
        t.push_str(". ");
        t.push_str(&st.last);
    }
    // Windows limits tray tooltips to 127 characters.
    if t.chars().count() > 127 {
        t = t.chars().take(124).collect::<String>() + "...";
    }
    t
}

fn menu_text(s: &str, max: usize) -> String {
    let s = s.replace('&', "&&");
    if s.chars().count() > max { s.chars().take(max - 3).collect::<String>() + "..." } else { s }
}

pub fn run(app: AppRef, rt: tokio::runtime::Runtime) -> ! {
    let mut builder = EventLoopBuilder::<UserEvent>::with_user_event();
    let event_loop = builder.build();
    #[cfg(target_os = "macos")]
    let mut event_loop = event_loop;
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    let proxy = event_loop.create_proxy();
    {
        let p = proxy.clone();
        *app.tray.lock() = Some(Box::new(move |st| {
            let _ = p.send_event(UserEvent::Status(st));
        }));
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Menu(e));
        }));
        let p = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Tray(e));
        }));
    }

    let status_item = MenuItem::new("SyncMe: starting", true, None);
    let last_item = MenuItem::new("No activity yet", true, None);
    let open_item = MenuItem::new("Open SyncMe", true, None);
    let announce_item = CheckMenuItem::new("Announce changes with screen reader", true, app.cfg.read().settings.announce, None);
    let quit_item = MenuItem::new("Quit SyncMe", true, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[&open_item, &PredefinedMenuItem::separator(), &status_item, &last_item, &PredefinedMenuItem::separator(), &announce_item, &quit_item]);

    let idle_icon = make_icon(false);
    let busy_icon = make_icon(true);
    let mut tray: Option<TrayIcon> = None;
    let mut last_busy = false;
    let mut menu_slot = Some(menu);
    let _rt = rt;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // macOS requires creating the status item once the loop is running.
            Event::NewEvents(StartCause::Init) => {
                let st = app.tray_status();
                let built = TrayIconBuilder::new()
                    .with_menu(Box::new(menu_slot.take().unwrap()))
                    .with_tooltip(tooltip(&st))
                    .with_icon(idle_icon.clone())
                    .with_icon_as_template(true)
                    .with_menu_on_left_click(true)
                    .build();
                match built {
                    Ok(t) => tray = Some(t),
                    Err(e) => tracing::error!("tray icon: {e}"),
                }
                app.update_tray();
            }
            Event::UserEvent(UserEvent::Status(st)) => {
                status_item.set_text(menu_text(&st.status, 70));
                last_item.set_text(if st.last.is_empty() { "No activity yet".into() } else { menu_text(&format!("Last: {}", st.last), 90) });
                announce_item.set_checked(app.cfg.read().settings.announce);
                if let Some(t) = &tray {
                    let _ = t.set_tooltip(Some(tooltip(&st)));
                    if st.busy != last_busy {
                        last_busy = st.busy;
                        let _ = t.set_icon(Some(if st.busy { busy_icon.clone() } else { idle_icon.clone() }));
                        #[cfg(target_os = "macos")]
                        t.set_icon_as_template(true);
                    }
                    #[cfg(target_os = "macos")]
                    t.set_title(if st.busy { Some("Syncing") } else { None::<&str> });
                }
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if e.id == *open_item.id() || e.id == *status_item.id() || e.id == *last_item.id() {
                    let _ = open::that_detached(app.ui_url());
                } else if e.id == *announce_item.id() {
                    let on = announce_item.is_checked();
                    app.cfg.write().settings.announce = on;
                    app.save();
                    app.changed();
                } else if e.id == *quit_item.id() {
                    app.shutdown.cancel();
                    app.save();
                    tray.take();
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::DoubleClick { .. })) => {
                let _ = open::that_detached(app.ui_url());
            }
            _ => {}
        }
    })
}

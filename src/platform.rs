use gpui::Window;

#[cfg(target_os = "macos")]
pub fn show_about_panel() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    NSApplication::sharedApplication(main_thread).orderFrontStandardAboutPanel(None);
}

#[cfg(not(target_os = "macos"))]
pub fn show_about_panel() {}

/// Register embedded font data with CoreText at process scope. GPUI's
/// `add_fonts` only feeds its private font-kit source, which CoreText cascade
/// matching cannot see — and it refuses symbols-only faces outright (fonts
/// with no 'm' glyph). Fonts referenced through `FontFallbacks` therefore
/// must be registered here instead.
#[cfg(target_os = "macos")]
pub fn register_fonts_with_coretext(fonts: &[&'static [u8]]) -> anyhow::Result<()> {
    use std::ffi::c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGDataProviderCreateWithData(
            info: *mut c_void,
            data: *const u8,
            size: usize,
            release_callback: *const c_void,
        ) -> *mut c_void;
        fn CGFontCreateWithDataProvider(provider: *mut c_void) -> *mut c_void;
        fn CGDataProviderRelease(provider: *mut c_void);
        fn CGFontRelease(font: *mut c_void);
    }
    #[link(name = "CoreText", kind = "framework")]
    unsafe extern "C" {
        fn CTFontManagerRegisterGraphicsFont(font: *mut c_void, error: *mut *mut c_void) -> bool;
    }

    for (index, data) in fonts.iter().enumerate() {
        unsafe {
            let provider = CGDataProviderCreateWithData(
                std::ptr::null_mut(),
                data.as_ptr(),
                data.len(),
                std::ptr::null(),
            );
            anyhow::ensure!(!provider.is_null(), "font {index}: not a readable buffer");
            let font = CGFontCreateWithDataProvider(provider);
            CGDataProviderRelease(provider);
            anyhow::ensure!(!font.is_null(), "font {index}: not a valid font");
            let registered = CTFontManagerRegisterGraphicsFont(font, std::ptr::null_mut());
            CGFontRelease(font);
            anyhow::ensure!(registered, "font {index}: CoreText registration failed");
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn register_fonts_with_coretext(_: &[&'static [u8]]) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn init_reduce_motion(cx: &mut gpui::App) {
    use objc2_app_kit::NSWorkspace;

    cx.set_reduce_motion(NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion());
}

#[cfg(target_os = "linux")]
pub fn init_reduce_motion(cx: &mut gpui::App) {
    if let Ok(value) = std::env::var("RAKU_REDUCE_MOTION")
        && let Some(enabled) = parse_boolean_setting(&value)
    {
        cx.set_reduce_motion(enabled);
        return;
    }

    // GNOME exposes its animation preference through GSettings. Resolve it
    // once off the UI thread; frames only read GPUI's in-memory flag.
    cx.spawn(async move |cx| {
        let enabled = cx
            .background_executor()
            .spawn(async move { linux_reduce_motion_enabled() })
            .await;
        cx.update(|cx| cx.set_reduce_motion(enabled));
    })
    .detach();
}

#[cfg(target_os = "linux")]
fn linux_reduce_motion_enabled() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| parse_boolean_setting(&value))
        .is_some_and(|animations_enabled| !animations_enabled)
}

/// Ease of Access → "Show animations in Windows" clears
/// `SPI_GETCLIENTAREAANIMATION`. GPUI has no Windows implementation of its
/// own, and the call only reads a cached user setting, so startup can ask
/// directly.
#[cfg(target_os = "windows")]
pub fn init_reduce_motion(cx: &mut gpui::App) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW,
    };

    let mut animations_enabled: i32 = 1;
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            std::ptr::from_mut(&mut animations_enabled).cast(),
            0,
        )
    };
    if read != 0 {
        cx.set_reduce_motion(animations_enabled == 0);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn init_reduce_motion(_: &mut gpui::App) {}

#[cfg(target_os = "linux")]
fn parse_boolean_setting(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Deliver an audible macOS notification. GPUI owns the notification-center
/// delegate (and therefore click responses); Raku only supplies content here
/// because GPUI's generic payload does not currently expose a sound field.
#[cfg(target_os = "macos")]
pub fn show_task_notification(tag: &str, title: &str, body: &str, _: &gpui::App) {
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSBundle, NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNNotificationSound, UNUserNotificationCenter,
    };

    // UserNotifications raises an Objective-C exception for an executable
    // outside an application bundle, including unit tests and `cargo run`.
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        return;
    }

    let tag = tag.to_owned();
    let title = title.to_owned();
    let body = body.to_owned();
    let authorization = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
        if !granted.as_bool() {
            return;
        }

        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&title));
        content.setBody(&NSString::from_str(&body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));

        // A nil trigger delivers immediately. The stable task tag replaces an
        // older completion banner for the same task and comes back on click.
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&tag),
            &content,
            None,
        );
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, None);
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &authorization,
        );
}

#[cfg(not(target_os = "macos"))]
pub fn show_task_notification(tag: &str, title: &str, body: &str, cx: &gpui::App) {
    cx.show_system_notification(gpui::SystemNotification {
        tag: tag.to_owned().into(),
        title: title.to_owned().into(),
        body: body.to_owned().into(),
        actions: Vec::new(),
    });
}

/// Select `path` in the platform file manager. GPUI dispatches Linux portal
/// and subprocess work away from the UI thread.
pub fn reveal_in_file_manager(path: &std::path::Path, cx: &gpui::App) {
    cx.reveal_path(path);
}

/// Open `path` with its default application — a document in its editor.
pub fn open_with_default_app(path: &std::path::Path, cx: &gpui::App) {
    cx.open_with_system(path);
}

/// Decode the embedded desktop icon once. X11 consumes the RGBA pixels from
/// `WindowOptions`; Wayland associates the window through `app_id` and its
/// installed desktop entry.
#[cfg(target_os = "linux")]
pub fn linux_app_icon() -> Option<std::sync::Arc<image::RgbaImage>> {
    static ICON: std::sync::LazyLock<Option<std::sync::Arc<image::RgbaImage>>> =
        std::sync::LazyLock::new(|| {
            image::load_from_memory(include_bytes!("../website/public/app-icon.png"))
                .ok()
                .map(|image| std::sync::Arc::new(image.into_rgba8()))
        });
    ICON.clone()
}

/// A compact shortcut label for the platform's primary GUI modifier.
pub const fn primary_shortcut<'a>(macos: &'a str, other: &'a str) -> &'a str {
    if cfg!(target_os = "macos") {
        macos
    } else {
        other
    }
}

/// Keep Raku's single main window alive when the user closes it. This preserves
/// the current session and lets a Dock activation reveal the same GPUI window.
#[cfg(target_os = "macos")]
pub fn configure_main_window_close_behavior(window: &Window, cx: &gpui::App) {
    window.on_window_should_close(cx, |window, _| {
        hide_window(window);
        false
    });
}

#[cfg(not(target_os = "macos"))]
pub fn configure_main_window_close_behavior(_: &Window, _: &gpui::App) {}

/// GPUI enables mouse-moved delivery for normal AppKit windows even when
/// they are inactive. Keep hover tracking confined to the key window.
#[cfg(target_os = "macos")]
pub fn configure_window_mouse_tracking<T: 'static>(window: &mut Window, cx: &mut gpui::Context<T>) {
    sync_window_mouse_tracking(window, cx);
    // GPUI emits this on activation changes too, after re-reading the native
    // pointer position. Clear that position after the update has completed.
    cx.observe_window_bounds(window, |_, window, cx| {
        sync_window_mouse_tracking(window, cx);
    })
    .detach();
}

#[cfg(not(target_os = "macos"))]
pub fn configure_window_mouse_tracking<T: 'static>(_: &mut Window, _: &mut gpui::Context<T>) {}

#[cfg(target_os = "macos")]
fn sync_window_mouse_tracking(window: &Window, cx: &mut gpui::App) {
    // GPUI's test windows have no native handle.
    #[cfg(not(test))]
    {
        use objc2::MainThreadMarker;
        use objc2_app_kit::NSView;
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        if let Ok(handle) = HasWindowHandle::window_handle(window)
            && let RawWindowHandle::AppKit(handle) = handle.as_raw()
            && let Some(_main_thread) = MainThreadMarker::new()
        {
            // The view and window belong to GPUI; access is confined to AppKit's
            // main thread. This does not affect clicks, scrolling, or dragging.
            unsafe {
                let view = handle.ns_view.cast::<NSView>().as_ref();
                if let Some(native_window) = view.window() {
                    native_window.setAcceptsMouseMovedEvents(window.is_window_active());
                }
            }
        }
    }

    if !window.is_window_active() {
        // Defer dispatch so hover listeners can update their entities without
        // re-entering the entity observing this window. A move outside clears
        // hover styles, on_hover state, and pending/visible tooltips together.
        // Keyboard modality already suppresses hover; preserve it so returning
        // via the keyboard keeps focus-visible styling.
        window.defer(cx, |window, cx| {
            if !window.is_window_active() {
                let event = if window.last_input_was_keyboard() {
                    gpui::PlatformInput::MouseExited(gpui::MouseExitEvent {
                        position: window.mouse_position(),
                        modifiers: window.modifiers(),
                        ..Default::default()
                    })
                } else {
                    gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                        position: gpui::point(gpui::px(-1.0), gpui::px(-1.0)),
                        modifiers: window.modifiers(),
                        ..Default::default()
                    })
                };
                window.dispatch_event(event, cx);
            }
        });
    }
}

#[cfg(target_os = "macos")]
pub fn hide_window(window: &mut Window) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSView;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    let Some(_main_thread) = MainThreadMarker::new() else {
        return;
    };

    // GPUI owns this view and its NSWindow. AppKit access stays on the main
    // thread, and orderOut hides without triggering GPUI's close callback.
    unsafe {
        let view = handle.ns_view.cast::<NSView>().as_ref();
        if let Some(native_window) = view.window() {
            native_window.orderOut(None);
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn hide_window(window: &mut Window) {
    window.remove_window();
}

pub fn start_window_move(window: &Window) {
    window.start_window_move();
}

/// Perform the platform's titlebar double-click action. GPUI delegates this
/// to the user's system preference on macOS, while Linux client decorations
/// must toggle maximize explicitly.
pub fn titlebar_double_click(window: &Window) {
    #[cfg(target_os = "macos")]
    window.titlebar_double_click();

    // Windows performs the user's configured caption double-click action in
    // `DefWindowProc`, which sees the click because the drag region reports
    // itself as caption to the hit test.
    #[cfg(target_os = "windows")]
    let _ = window;

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    if window.window_controls().maximize && window.is_resizable() {
        window.zoom_window();
    }
}

/// Follow macOS when `dark` is `None`, otherwise force the native titlebar,
/// traffic lights, menus, and vibrancy to the selected appearance.
#[cfg(target_os = "macos")]
pub fn set_window_appearance(window: &Window, dark: Option<bool>) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
        NSView,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    let Some(_main_thread) = MainThreadMarker::new() else {
        return;
    };

    unsafe {
        let view = handle.ns_view.cast::<NSView>().as_ref();
        let Some(native_window) = view.window() else {
            return;
        };
        let appearance = dark.and_then(|dark| {
            NSAppearance::appearanceNamed(if dark {
                NSAppearanceNameDarkAqua
            } else {
                NSAppearanceNameAqua
            })
        });
        native_window.setAppearance(appearance.as_deref());
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_window_appearance(_: &Window, _: Option<bool>) {}

#[cfg(all(test, target_os = "macos"))]
mod mouse_tracking_tests {
    use super::configure_window_mouse_tracking;
    use gpui::{
        Context, IntoElement, Modifiers, Render, TestAppContext, div, point, prelude::*, px,
    };
    use std::{cell::Cell, rc::Rc, time::Duration};

    struct HoverTarget {
        hovered: Rc<Cell<bool>>,
        tooltips_built: Rc<Cell<usize>>,
    }

    struct EmptyTooltip;

    impl Render for EmptyTooltip {
        fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size(px(16.0))
        }
    }

    impl Render for HoverTarget {
        fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
            let hovered = self.hovered.clone();
            let tooltips_built = self.tooltips_built.clone();
            div().size_full().child(
                div()
                    .id("hover-target")
                    .size(px(100.0))
                    .on_hover(move |value, _, _| hovered.set(*value))
                    .tooltip(move |_, cx| {
                        tooltips_built.set(tooltips_built.get() + 1);
                        cx.new(|_| EmptyTooltip).into()
                    }),
            )
        }
    }

    #[gpui::test]
    fn deactivation_clears_hover_and_cancels_pending_tooltips(cx: &mut TestAppContext) {
        let hovered = Rc::new(Cell::new(false));
        let tooltips_built = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view(|window, cx| {
            configure_window_mouse_tracking(window, cx);
            HoverTarget {
                hovered: hovered.clone(),
                tooltips_built: tooltips_built.clone(),
            }
        });
        cx.update(|window, _| window.activate_window());
        cx.simulate_mouse_move(point(px(10.0), px(10.0)), None, Modifiers::none());
        assert!(hovered.get());

        cx.deactivate_window();
        assert!(!hovered.get(), "losing activation must end the hover");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(tooltips_built.get(), 0);

        cx.update(|window, _| window.activate_window());
        cx.simulate_mouse_move(point(px(10.0), px(10.0)), None, Modifiers::none());
        assert!(hovered.get(), "hover must work again after activation");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(tooltips_built.get(), 1);
    }

    #[gpui::test]
    fn deactivation_preserves_keyboard_input_modality(cx: &mut TestAppContext) {
        let hovered = Rc::new(Cell::new(false));
        let (_, cx) = cx.add_window_view(|window, cx| {
            configure_window_mouse_tracking(window, cx);
            HoverTarget {
                hovered: hovered.clone(),
                tooltips_built: Rc::new(Cell::new(0)),
            }
        });
        cx.update(|window, _| window.activate_window());
        cx.simulate_mouse_move(point(px(10.0), px(10.0)), None, Modifiers::none());
        cx.simulate_keystrokes("tab");
        assert!(cx.update(|window, _| window.last_input_was_keyboard()));

        cx.deactivate_window();
        assert!(!hovered.get());
        assert!(cx.update(|window, _| window.last_input_was_keyboard()));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::parse_boolean_setting;

    #[test]
    fn boolean_desktop_settings_are_parsed_case_insensitively() {
        assert_eq!(parse_boolean_setting(" true\n"), Some(true));
        assert_eq!(parse_boolean_setting("OFF"), Some(false));
        assert_eq!(parse_boolean_setting("default"), None);
    }

    #[test]
    fn embedded_linux_icon_decodes_at_desktop_size() {
        let icon = super::linux_app_icon().expect("embedded PNG should decode");

        assert_eq!(icon.dimensions(), (256, 256));
    }
}

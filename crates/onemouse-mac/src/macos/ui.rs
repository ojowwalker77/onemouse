//! Menu-bar item and the Arrange Displays window: the Mac's displays and the
//! PC's, drawn to scale; drag the PC to where it sits on the desk.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezierPath, NSColor,
    NSEvent, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMenu, NSMenuDelegate,
    NSMenuItem, NSStatusBar, NSStringDrawing, NSVariableStatusItemLength, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString, NSTimer};
use onemouse_protocol::Display;

use super::{Tap, displays};
use crate::layout::{self, Point, Rect};
use crate::log;

/// Starts the menu-bar item and runs the app (and with it the event tap's
/// run loop) until Quit.
pub(super) fn run(tap: &'static Tap) {
    let mtm = MainThreadMarker::new().expect("onemouse must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let target = Target::new(mtm, tap);
    let status = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = status.button(mtm) {
        button.setTitle(&NSString::from_str("⇄"));
    }

    let menu = NSMenu::new(mtm);
    menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    let status_line = item(mtm, "Waiting for the PC…", None);
    status_line.setEnabled(false);
    menu.addItem(&status_line);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let arrange = item(mtm, "Arrange Displays…", Some(sel!(openArrange:)));
    // SAFETY: `target` implements `openArrange:` and outlives the menu.
    unsafe { arrange.setTarget(Some(&target)) };
    menu.addItem(&arrange);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item(mtm, "Quit onemouse", Some(sel!(terminate:))));
    status.setMenu(Some(&menu));
    *target.ivars().status_line.borrow_mut() = Some(status_line);

    // SAFETY: `target` implements `tick:`; the timer retains it.
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            1.0,
            &target,
            sel!(tick:),
            None,
            true,
        );
    }
    if tap.arrange_at_start {
        target.show_window();
    }
    // Lives as long as the app.
    std::mem::forget((status, menu, target));
    app.run();
}

fn item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<objc2::runtime::Sel>,
) -> Retained<NSMenuItem> {
    // SAFETY: plain menu item; actions are resolved through the responder
    // chain or an explicit target.
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            &NSString::from_str(""),
        )
    }
}

/// The secondary's displays and name: live if connected, else remembered.
fn secondary(tap: &Tap) -> Option<(String, Vec<Display>, bool)> {
    tap.link
        .with_peer(|p| p.map(|p| (p.name.clone(), p.displays.clone(), true)))
        .or_else(|| {
            let config = tap.config.borrow();
            (!config.displays.is_empty()).then(|| ("PC".into(), config.displays.clone(), false))
        })
}

pub(super) struct TargetIvars {
    tap: &'static Tap,
    status_line: RefCell<Option<Retained<NSMenuItem>>>,
    window: RefCell<Option<Retained<NSWindow>>>,
    view: RefCell<Option<Retained<ArrangeView>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OneMouseTarget"]
    #[ivars = TargetIvars]
    pub(super) struct Target;

    impl Target {
        #[unsafe(method(openArrange:))]
        fn open_arrange(&self, _sender: Option<&AnyObject>) {
            self.show_window();
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            self.remember_displays();
            if let Some(window) = &*self.ivars().window.borrow()
                && window.isVisible()
                && let Some(view) = &*self.ivars().view.borrow()
            {
                view.setNeedsDisplay(true);
            }
        }
    }

    unsafe impl NSObjectProtocol for Target {}

    unsafe impl NSMenuDelegate for Target {
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            let title = match self.ivars().tap.link.with_peer(|p| p.map(|p| p.name.clone())) {
                Some(name) => format!("Connected to {name}"),
                None => "Waiting for the PC…".into(),
            };
            if let Some(line) = &*self.ivars().status_line.borrow() {
                line.setTitle(&NSString::from_str(&title));
            }
        }
    }
);

impl Target {
    fn new(mtm: MainThreadMarker, tap: &'static Tap) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars {
            tap,
            status_line: RefCell::new(None),
            window: RefCell::new(None),
            view: RefCell::new(None),
        });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn show_window(&self) {
        let mtm = self.mtm();
        let ivars = self.ivars();
        if ivars.window.borrow().is_none() {
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(680.0, 440.0));
            // SAFETY: standard window creation; it's kept in our ivars and not
            // released on close.
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    frame,
                    NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            // SAFETY: we own the window through `ivars.window`.
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str("onemouse: Arrange Displays"));
            let view = ArrangeView::new(mtm, frame, ivars.tap);
            window.setContentView(Some(&view));
            window.center();
            *ivars.view.borrow_mut() = Some(view);
            *ivars.window.borrow_mut() = Some(window);
        }
        if let Some(window) = &*ivars.window.borrow() {
            window.makeKeyAndOrderFront(None);
        }
        NSApplication::sharedApplication(mtm).activate();
    }

    /// Keeps the last seen PC layout so it can be arranged while offline.
    fn remember_displays(&self) {
        let tap = self.ivars().tap;
        let Some(live) = tap.link.with_peer(|p| p.map(|p| p.displays.clone())) else {
            return;
        };
        if tap.config.borrow().displays != live {
            tap.config.borrow_mut().displays = live;
            save(tap);
        }
    }
}

fn save(tap: &Tap) {
    if let Some(path) = &tap.config_path
        && let Err(e) = tap.config.borrow().save(path)
    {
        log!("couldn't save the arrangement to {}: {e}", path.display());
    }
}

/// Mac points ↔ view coordinates (the view is flipped, so y grows down
/// in both).
#[derive(Debug, Clone, Copy)]
struct Transform {
    scale: f64,
    dx: f64,
    dy: f64,
}

impl Transform {
    /// Fits `content` centered in `bounds` with a margin.
    fn fit(content: Rect, bounds: NSRect) -> Self {
        let margin = 48.0;
        let (w, h) = (
            bounds.size.width - 2.0 * margin,
            bounds.size.height - 2.0 * margin - 24.0,
        );
        let scale = (w / content.width).min(h / content.height).min(0.25);
        Self {
            scale,
            dx: (bounds.size.width - content.width * scale) / 2.0 - content.x * scale,
            dy: margin + (h - content.height * scale) / 2.0 - content.y * scale,
        }
    }

    fn to_view(self, r: Rect) -> NSRect {
        NSRect::new(
            NSPoint::new(r.x * self.scale + self.dx, r.y * self.scale + self.dy),
            NSSize::new(r.width * self.scale, r.height * self.scale),
        )
    }

    fn to_points(self, p: NSPoint) -> Point {
        Point::new((p.x - self.dx) / self.scale, (p.y - self.dy) / self.scale)
    }
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    /// Mouse position relative to the block's origin, in points.
    grab: Point,
    /// Where the block is being dragged, unsnapped.
    origin: Point,
    /// Frozen for the whole drag so the picture doesn't rescale under it.
    transform: Transform,
}

pub(super) struct ArrangeIvars {
    tap: &'static Tap,
    drag: Cell<Option<Drag>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OneMouseArrangeView"]
    #[ivars = ArrangeIvars]
    pub(super) struct ArrangeView;

    impl ArrangeView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let Some(scene) = self.scene() else { return };
            let Some(block) = scene.block else { return };
            let at = scene.transform.to_points(self.location(event));
            if block.contains(at) {
                self.ivars().drag.set(Some(Drag {
                    grab: Point::new(at.x - block.x, at.y - block.y),
                    origin: Point::new(block.x, block.y),
                    transform: scene.transform,
                }));
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let Some(mut drag) = self.ivars().drag.get() else { return };
            let at = drag.transform.to_points(self.location(event));
            drag.origin = Point::new(at.x - drag.grab.x, at.y - drag.grab.y);
            self.ivars().drag.set(Some(drag));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            let Some(drag) = self.ivars().drag.take() else { return };
            let tap = self.ivars().tap;
            if let Some((_, pc, _)) = secondary(tap) {
                let snapped = layout::snap(&displays(), layout::block_size(&pc), drag.origin);
                tap.controller.borrow_mut().set_arrangement(snapped);
                let mut config = tap.config.borrow_mut();
                config.origin = snapped;
                config.displays = pc;
                drop(config);
                save(tap);
            }
            self.setNeedsDisplay(true);
        }
    }
);

/// What's on screen right now.
struct Scene {
    mac: Vec<Rect>,
    name: String,
    online: bool,
    placed: Vec<Rect>,
    block: Option<Rect>,
    transform: Transform,
}

impl ArrangeView {
    fn new(mtm: MainThreadMarker, frame: NSRect, tap: &'static Tap) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ArrangeIvars {
            tap,
            drag: Cell::new(None),
        });
        // SAFETY: NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn location(&self, event: &NSEvent) -> NSPoint {
        self.convertPoint_fromView(event.locationInWindow(), None)
    }

    fn scene(&self) -> Option<Scene> {
        let tap = self.ivars().tap;
        let mac = displays();
        let content_mac = Rect::union(mac.iter().copied())?;
        let drag = self.ivars().drag.get();
        let (name, online, placed) = match secondary(tap) {
            Some((name, displays, online)) => {
                let placed = match drag {
                    Some(d) => layout::place(&displays, d.origin),
                    None => tap.controller.borrow().placed(&mac, &displays),
                };
                (name, online, placed.into_iter().map(|p| p.rect).collect())
            }
            None => (String::new(), false, Vec::new()),
        };
        let block = Rect::union(placed.iter().copied());
        let transform = match drag {
            Some(d) => d.transform,
            None => Transform::fit(
                Rect::union([content_mac].into_iter().chain(block)).unwrap_or(content_mac),
                self.bounds(),
            ),
        };
        Some(Scene {
            mac,
            name,
            online,
            placed,
            block,
            transform,
        })
    }

    fn draw(&self) {
        let bounds = self.bounds();
        NSColor::windowBackgroundColor().setFill();
        NSBezierPath::fillRect(bounds);
        let Some(scene) = self.scene() else { return };

        for (i, r) in scene.mac.iter().enumerate() {
            let v = scene.transform.to_view(*r);
            screen(v, rgb(0.42, 0.45, 0.50), 1.0);
            if r.contains(Point::new(0.0, 0.0)) {
                // The main display has the menu bar.
                rgb(1.0, 1.0, 1.0).setFill();
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(v.origin.x + 3.0, v.origin.y + 3.0),
                    NSSize::new(v.size.width - 6.0, 4.0),
                ));
            }
            let label = if i == 0 || r.contains(Point::new(0.0, 0.0)) {
                "Mac"
            } else {
                "Mac display"
            };
            text(label, v, 12.0);
        }
        let dragging = self.ivars().drag.get().is_some();
        for v in scene.placed.iter().map(|r| scene.transform.to_view(*r)) {
            let alpha = if scene.online { 1.0 } else { 0.55 };
            screen(v, rgb(0.20, 0.47, 0.96), if dragging { 0.8 } else { alpha });
            let name = if scene.online {
                scene.name.clone()
            } else {
                format!("{} (offline)", scene.name)
            };
            text(&name, v, 12.0);
        }

        let hint = if scene.placed.is_empty() {
            "Connect the PC to arrange it."
        } else {
            "Drag the PC to where it sits next to your Mac. The cursor crosses where they touch."
        };
        draw_string(
            hint,
            NSPoint::new(20.0, bounds.size.height - 32.0),
            11.0,
            &NSColor::secondaryLabelColor(),
        );
    }
}

fn rgb(r: f64, g: f64, b: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0)
}

/// A display: rounded, filled, with a light border.
fn screen(v: NSRect, fill: Retained<NSColor>, alpha: f64) {
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(v, 5.0, 5.0);
    fill.colorWithAlphaComponent(alpha).setFill();
    path.fill();
    NSColor::colorWithWhite_alpha(1.0, 0.7).setStroke();
    path.setLineWidth(1.5);
    path.stroke();
}

/// Centered label inside `v`.
fn text(s: &str, v: NSRect, size: f64) {
    let approx_width = s.chars().count() as f64 * size * 0.55;
    draw_string(
        s,
        NSPoint::new(
            v.origin.x + (v.size.width - approx_width) / 2.0,
            v.origin.y + (v.size.height - size) / 2.0 - 2.0,
        ),
        size,
        &NSColor::whiteColor(),
    );
}

fn draw_string(s: &str, at: NSPoint, size: f64, color: &NSColor) {
    let font = NSFont::systemFontOfSize(size);
    // SAFETY: the attribute statics are valid NSString keys.
    let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let values: [&AnyObject; 2] = [&font, color];
    let attrs = NSDictionary::from_slices(&keys, &values);
    // SAFETY: the values have the types those attribute keys expect.
    unsafe { NSString::from_str(s).drawAtPoint_withAttributes(at, Some(&attrs)) };
}

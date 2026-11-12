//! Context menus and dropdown menus.
//!
//! Both share one card and one dismissal model; they differ only in where they
//! anchor — a context menu at the pointer, a dropdown under its trigger.
//!
//! A menu is built lazily: the item list is only constructed once the menu
//! actually opens, and while closed the wrapper contributes one `Rc<Cell>` read
//! and no children. The open menu renders through a deferred floating surface
//! so it escapes its row's clipping and paints above every sibling.
//!
//! Dismissal follows Zed's own context menus:
//!
//! - **Click outside** uses `on_mouse_down_out`, which tests the card's own
//!   hitbox during the capture phase. An occluding full-window backdrop would
//!   also work but has to guess the window size and swallows hover elsewhere.
//!   A left click on the trigger is exempt — capture runs before the trigger's
//!   bubble-phase toggle, so closing here would make the toggle see a closed
//!   menu and reopen it.
//! - **Escape** is an action bound in the menu's own key context, so it beats
//!   the transcript's `escape` binding instead of also cancelling the turn.
//! - **Focus** is taken two frames after opening. Deferred elements are not
//!   linked into the dispatch tree until after the deferred draw runs, so
//!   focusing any earlier silently does nothing — and then no key reaches the
//!   menu at all.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Display, Element, ElementId, FocusHandle, FontWeight, GlobalElementId,
    InspectorElementId, InteractiveElement, IntoElement, KeyDownEvent, LayoutId, ListAlignment,
    ListState, MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Position, RenderOnce,
    SharedString, Size, StatefulInteractiveElement, Style, Styled, Window, actions, canvas,
    deferred, div, list, prelude::FluentBuilder, px,
};

actions!(
    raku_menu,
    [
        DismissMenu,
        SelectNextEntry,
        SelectPreviousEntry,
        SelectNextTab,
        SelectPreviousTab,
        ConfirmEntry
    ]
);

/// Key context the open menu declares, and the scope its bindings live in.
const MENU_CONTEXT: &str = "RakuMenu";
const FONT_MENU_CONTEXT: &str = "RakuFontMenu";

/// Vertical gap between a trigger and its anchored card.
const TRIGGER_GAP: f32 = 4.0;
const SUBMENU_OVERLAP: f32 = 4.0;

/// A text field inside an open panel, such as a picker's filter box.
///
/// The field holds real focus the whole time — the list's selection is drawn,
/// never focused, which is how Zed's picker works. So the list's keys have to
/// be claimed from under the focused field, and only a binding can do that:
/// `enter`, `tab`, and the arrows reach the field as *actions*, and an action
/// consumes the keystroke before any `on_key_down` listener above it ever runs.
const PANEL_FIELD_CONTEXT: &str = "RakuMenu > TextInput";

/// Bind the menu's own keys. Called once at startup.
///
/// Must run after [`crate::input::init`]: these share a context depth with the
/// field's own bindings, and the tie goes to whichever was registered last.
/// That is what lets `enter` here beat the field's submit.
pub fn init(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("escape", DismissMenu, Some(MENU_CONTEXT)),
        KeyBinding::new("escape", DismissMenu, Some(FONT_MENU_CONTEXT)),
        KeyBinding::new("tab", DismissMenu, Some(FONT_MENU_CONTEXT)),
        KeyBinding::new("shift-tab", DismissMenu, Some(FONT_MENU_CONTEXT)),
        KeyBinding::new("down", SelectNextEntry, Some(PANEL_FIELD_CONTEXT)),
        KeyBinding::new("up", SelectPreviousEntry, Some(PANEL_FIELD_CONTEXT)),
        KeyBinding::new("tab", SelectNextTab, Some(PANEL_FIELD_CONTEXT)),
        KeyBinding::new("shift-tab", SelectPreviousTab, Some(PANEL_FIELD_CONTEXT)),
        KeyBinding::new("enter", ConfirmEntry, Some(PANEL_FIELD_CONTEXT)),
    ]);
}

use crate::theme::{DEFAULT_FONT_WEIGHT, SMALL_UI_FONT_SIZE, Theme, sp};
use crate::ui::icon;

/// One row of a menu.
#[derive(Clone)]
pub enum MenuItem {
    Entry {
        label: SharedString,
        icon: Option<&'static str>,
        /// Draws a trailing check, for menus that present a current choice.
        selected: bool,
        /// Shown greyed and inert. Preferred over omitting the row when the
        /// action is temporarily unavailable, so the menu keeps a stable shape.
        disabled: bool,
        /// Drawn in the danger color, for actions that destroy data.
        destructive: bool,
        #[allow(clippy::type_complexity)]
        on_click: Rc<dyn Fn(&mut Window, &mut App)>,
    },
    /// Opens a one-level flyout beside the parent card.
    Submenu {
        label: SharedString,
        #[allow(clippy::type_complexity)]
        items: Rc<dyn Fn(&mut App) -> Vec<MenuItem>>,
    },
    /// A caller-drawn row, for choices that need more than a label — a badge, a
    /// secondary line, an inline swatch. Clickable when `on_click` is set.
    Custom {
        #[allow(clippy::type_complexity)]
        render: Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>,
        #[allow(clippy::type_complexity)]
        on_click: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
    },
    /// A non-interactive caption grouping the rows beneath it.
    Header(SharedString),
    Separator,
}

impl MenuItem {
    pub fn new(
        label: impl Into<SharedString>,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self::Entry {
            label: label.into(),
            icon: None,
            selected: false,
            disabled: false,
            destructive: false,
            on_click: Rc::new(on_click),
        }
    }

    /// A caller-drawn row. `render` runs on every frame the menu is open.
    pub fn custom(render: impl Fn(&mut Window, &mut App) -> AnyElement + 'static) -> Self {
        Self::Custom {
            render: Rc::new(render),
            on_click: None,
        }
    }

    pub fn submenu(
        label: impl Into<SharedString>,
        items: impl Fn(&mut App) -> Vec<MenuItem> + 'static,
    ) -> Self {
        Self::Submenu {
            label: label.into(),
            items: Rc::new(items),
        }
    }

    pub fn on_click(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        if let Self::Custom { on_click, .. } = &mut self {
            *on_click = Some(Rc::new(handler));
        }
        self
    }

    pub fn selected(mut self, value: bool) -> Self {
        if let Self::Entry { selected, .. } = &mut self {
            *selected = value;
        }
        self
    }

    pub fn disabled(mut self, value: bool) -> Self {
        if let Self::Entry { disabled, .. } = &mut self {
            *disabled = value;
        }
        self
    }

    pub fn destructive(mut self) -> Self {
        if let Self::Entry { destructive, .. } = &mut self {
            *destructive = true;
        }
        self
    }

    pub fn icon(mut self, path: &'static str) -> Self {
        if let Self::Entry { icon, .. } = &mut self {
            *icon = Some(path);
        }
        self
    }

    fn is_focusable(&self) -> bool {
        match self {
            Self::Entry { disabled, .. } => !disabled,
            Self::Submenu { .. } => true,
            Self::Custom { on_click, .. } => on_click.is_some(),
            Self::Header(_) | Self::Separator => false,
        }
    }

    fn click_handler(self) -> Option<Rc<dyn Fn(&mut Window, &mut App)>> {
        match self {
            Self::Entry {
                disabled: false,
                on_click,
                ..
            } => Some(on_click),
            Self::Entry { disabled: true, .. } => None,
            Self::Custom { on_click, .. } => on_click,
            Self::Submenu { .. } | Self::Header(_) | Self::Separator => None,
        }
    }
}

/// Where an open menu is anchored, in window coordinates.
#[derive(Default)]
struct MenuState {
    open: Option<Point<Pixels>>,
    /// A nested hit target can contribute actions to its ancestor's menu.
    /// Snapshot them on opening so streaming/reflow cannot retarget an action.
    pending_context_items: Vec<MenuItem>,
    context_items: Vec<MenuItem>,
    /// Keyboard cursor over focusable entries.
    highlighted: Option<usize>,
    /// Lazily allocated for a scrollable dropdown and retained until it closes.
    list: Option<ListState>,
    /// Parent item whose flyout is visible.
    active_submenu: Option<usize>,
    /// Keyboard cursor inside the visible flyout.
    submenu_highlighted: Option<usize>,
    /// Whether arrow-key navigation currently belongs to the flyout.
    submenu_focused: bool,
    /// A dropdown/popover trigger toggles its own surface on left click. The
    /// outside-click capture must leave that click alone so the later trigger
    /// handler can close it; a context-menu row has no such handler.
    trigger_click_toggles: bool,
}

/// Cross-frame state for one context menu. The owner keeps one per menu site.
#[derive(Clone)]
pub struct ContextMenuHandle {
    state: Rc<RefCell<MenuState>>,
    /// Stable focus identity shared by dropdown and keyboard context triggers.
    trigger_focus: FocusHandle,
    focus: FocusHandle,
    /// The trigger's bounds as of the last frame, so a dropdown can align under
    /// it. Recorded by a zero-cost canvas inside the trigger.
    trigger_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// A flyout sits outside the main card's hitbox but belongs to its menu.
    submenu_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// A left press landed on the dropdown trigger and has not been released
    /// yet; the toggle fires on the matching release, like a native button.
    trigger_pressed: Rc<Cell<bool>>,
    /// Notified with the new open state whenever the menu toggles, in order.
    /// The composer's caret preservation is one of these; a site can add its
    /// own on top.
    #[allow(clippy::type_complexity)]
    on_toggle: Rc<Vec<Rc<dyn Fn(bool, &mut Window, &mut App)>>>,
}

impl ContextMenuHandle {
    pub fn new(cx: &mut App) -> Self {
        Self {
            state: Rc::new(RefCell::new(MenuState::default())),
            trigger_focus: cx.focus_handle(),
            focus: cx.focus_handle(),
            trigger_bounds: Rc::new(Cell::new(None)),
            submenu_bounds: Rc::new(Cell::new(None)),
            trigger_pressed: Rc::new(Cell::new(false)),
            on_toggle: Rc::new(Vec::new()),
        }
    }

    /// Observe open/close transitions. Called only on an actual change, in the
    /// order the observers were added.
    pub fn on_toggle(mut self, handler: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        let mut handlers = (*self.on_toggle).clone();
        handlers.push(Rc::new(handler));
        self.on_toggle = Rc::new(handlers);
        self
    }

    fn notify_toggle(&self, open: bool, window: &mut Window, cx: &mut App) {
        for handler in self.on_toggle.iter() {
            handler(open, window, cx);
        }
    }

    pub fn is_open(&self) -> bool {
        self.state.borrow().open.is_some()
    }

    pub fn set_context_items(&self, items: Vec<MenuItem>) {
        self.state.borrow_mut().pending_context_items = items;
    }

    /// The card's focus handle, for content-focusing surfaces whose panel has
    /// no input of its own: focusing the card puts the menu key context on
    /// the dispatch path, which is what lets `escape` dismiss it.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The trigger's bounds as of its last paint, for a panel sized to it.
    pub fn trigger_bounds(&self) -> Option<Bounds<Pixels>> {
        self.trigger_bounds.get()
    }

    /// Stable focus identity for a keyboard-operable menu trigger.
    pub fn trigger_focus_handle(&self) -> &FocusHandle {
        &self.trigger_focus
    }

    /// Opens a context menu from its trigger instead of a pointer event. The
    /// card begins just under the row, avoiding an overlap with its focus ring.
    pub fn open_context_menu(&self, window: &mut Window, cx: &mut App) {
        let position = self
            .trigger_bounds
            .get()
            .map(|bounds| Point::new(bounds.left() + px(8.0), bounds.bottom()))
            .unwrap_or_else(|| window.mouse_position());
        open_menu(self, position, SurfaceFocus::Card, false, window, cx);
    }

    pub fn close(&self, window: &mut Window, cx: &mut App) {
        self.submenu_bounds.set(None);
        let was_open = {
            let mut state = self.state.borrow_mut();
            let was_open = state.open.is_some();
            state.open = None;
            state.pending_context_items.clear();
            state.context_items.clear();
            state.highlighted = None;
            state.list = None;
            state.active_submenu = None;
            state.submenu_highlighted = None;
            state.submenu_focused = false;
            state.trigger_click_toggles = false;
            was_open
        };
        if was_open {
            self.notify_toggle(false, window, cx);
        }
    }

    /// Dismiss for a mouse down outside the card, except a left click on a
    /// dropdown/popover trigger: its own bubble-phase handler is the toggle,
    /// and it runs after this capture-phase listener. Closing here first would
    /// make that handler see a closed menu and reopen it.
    fn dismiss_on_down_out(&self, event: &MouseDownEvent, window: &mut Window, cx: &mut App) {
        if self.state.borrow().active_submenu.is_some()
            && self
                .submenu_bounds
                .get()
                .is_some_and(|bounds| bounds.contains(&event.position))
        {
            return;
        }
        let on_toggling_trigger = self.state.borrow().trigger_click_toggles
            && event.button == MouseButton::Left
            && self
                .trigger_bounds
                .get()
                .is_some_and(|bounds| bounds.contains(&event.position));
        if on_toggling_trigger {
            return;
        }
        self.close(window, cx);
        window.refresh();
    }

    fn open_at(
        &self,
        position: Point<Pixels>,
        trigger_click_toggles: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.submenu_bounds.set(None);
        let was_open = {
            let mut state = self.state.borrow_mut();
            let was_open = state.open.is_some();
            state.open = Some(position);
            state.context_items = std::mem::take(&mut state.pending_context_items);
            state.highlighted = None;
            state.list = None;
            state.active_submenu = None;
            state.submenu_highlighted = None;
            state.submenu_focused = false;
            state.trigger_click_toggles = trigger_click_toggles;
            was_open
        };
        if !was_open {
            self.notify_toggle(true, window, cx);
        }
    }
}

/// Whether the opened surface takes focus itself.
///
/// A [`MenuCard`] tracks the handle's focus and needs it to see arrow keys. A
/// [`PopoverCard`] does not track it, so focusing the handle would detach focus
/// from the window's dispatch tree — and blur whatever the panel's content
/// focused for itself, such as a search field.
#[derive(Clone, Copy, Eq, PartialEq)]
enum SurfaceFocus {
    Card,
    Content,
}

/// Open at `position`, handing focus to the card when it owns focus.
///
/// The card is deferred, so its focus handle joins the dispatch tree only after
/// the deferred draw. Focusing before then is a silent no-op that leaves the
/// menu unable to see a keystroke — hence the two-frame wait, matching Zed.
fn open_menu(
    handle: &ContextMenuHandle,
    position: Point<Pixels>,
    focus_target: SurfaceFocus,
    trigger_click_toggles: bool,
    window: &mut Window,
    cx: &mut App,
) {
    // Runs the toggle observers, which is where a content-focusing surface
    // schedules its own focus. Ours is scheduled after, so it would win — only
    // request it when the card is what should end up focused.
    handle.open_at(position, trigger_click_toggles, window, cx);
    if focus_target == SurfaceFocus::Card {
        let focus = handle.focus.clone();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        });
    }
    window.refresh();
}

/// A zero-cost canvas that records its parent's bounds into the handle.
///
/// `inset_0` rather than `size_full`: an absolutely positioned child sizes
/// against its containing block, so `size_full` inside a padded trigger reports
/// the *content* box and the menu ends up indented by the trigger's padding.
fn trigger_bounds_probe(handle: &ContextMenuHandle) -> impl IntoElement {
    bounds_probe(handle.trigger_bounds.clone())
}

fn bounds_probe(bounds: Rc<Cell<Option<Bounds<Pixels>>>>) -> impl IntoElement {
    canvas(
        move |probe: Bounds<Pixels>, _, _| bounds.set(Some(probe)),
        |_, _, _, _| (),
    )
    .absolute()
    .inset_0()
}

/// Where a dropdown's card sits relative to its trigger.
///
/// Side matters as much as alignment here: the composer's controls live at the
/// bottom of the window, so their menus have to grow upward or they open off
/// screen and get snapped back over the trigger.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MenuAlign {
    /// Below the trigger, left edges aligned.
    #[default]
    BelowLeft,
    /// Below the trigger, right edges aligned.
    BelowRight,
    /// Above the trigger, left edges aligned.
    AboveLeft,
    /// Above the trigger, right edges aligned.
    AboveRight,
}

impl MenuAlign {
    fn above(self) -> bool {
        matches!(self, Self::AboveLeft | Self::AboveRight)
    }

    fn right_aligned(self) -> bool {
        matches!(self, Self::BelowRight | Self::AboveRight)
    }

    fn from_sides(above: bool, right_aligned: bool) -> Self {
        match (above, right_aligned) {
            (false, false) => Self::BelowLeft,
            (false, true) => Self::BelowRight,
            (true, false) => Self::AboveLeft,
            (true, true) => Self::AboveRight,
        }
    }

    /// The point on the trigger the card's corner attaches to.
    fn anchor_point(self, bounds: gpui::Bounds<Pixels>, gap: Pixels) -> Point<Pixels> {
        let x = if self.right_aligned() {
            bounds.right()
        } else {
            bounds.left()
        };
        let y = if self.above() {
            bounds.top() - gap
        } else {
            bounds.bottom() + gap
        };
        Point::new(x, y)
    }
}

/// Final placement for an anchored surface after `flip` and `shift`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FloatingPlacement {
    bounds: Bounds<Pixels>,
    align: MenuAlign,
}

/// Resolve a trigger-aware placement using Floating UI's core policy:
///
/// 1. Keep the requested vertical side while it fits.
/// 2. Flip to the opposite side when it fits better.
/// 3. Try the opposite horizontal alignment, then shift inside the viewport.
///
/// Unlike GPUI's point-based anchor switching, a vertical flip uses the
/// trigger's opposite edge, so the card never lands across the trigger merely
/// because the preferred side ran out of room.
fn resolve_floating_placement(
    trigger: Bounds<Pixels>,
    surface_size: Size<Pixels>,
    viewport: Bounds<Pixels>,
    preferred: MenuAlign,
    gap: Pixels,
    margin: Pixels,
) -> FloatingPlacement {
    let viewport_left = f32::from(viewport.left() + margin);
    let viewport_right = f32::from(viewport.right() - margin);
    let viewport_top = f32::from(viewport.top() + margin);
    let viewport_bottom = f32::from(viewport.bottom() - margin);
    let trigger_left = f32::from(trigger.left());
    let trigger_right = f32::from(trigger.right());
    let trigger_top = f32::from(trigger.top());
    let trigger_bottom = f32::from(trigger.bottom());
    let width = f32::from(surface_size.width);
    let height = f32::from(surface_size.height);
    let gap = f32::from(gap);

    let above_space = (trigger_top - gap - viewport_top).max(0.0);
    let below_space = (viewport_bottom - trigger_bottom - gap).max(0.0);
    let preferred_above = preferred.above();
    let preferred_space = if preferred_above {
        above_space
    } else {
        below_space
    };
    let opposite_space = if preferred_above {
        below_space
    } else {
        above_space
    };
    let above = if height <= preferred_space || preferred_space >= opposite_space {
        preferred_above
    } else {
        !preferred_above
    };

    let left_aligned_x = trigger_left;
    let right_aligned_x = trigger_right - width;
    let overflow = |x: f32| (viewport_left - x).max(0.0) + (x + width - viewport_right).max(0.0);
    let preferred_right = preferred.right_aligned();
    let preferred_x = if preferred_right {
        right_aligned_x
    } else {
        left_aligned_x
    };
    let opposite_x = if preferred_right {
        left_aligned_x
    } else {
        right_aligned_x
    };
    let right_aligned = if overflow(preferred_x) <= overflow(opposite_x) {
        preferred_right
    } else {
        !preferred_right
    };
    let mut x = if right_aligned {
        right_aligned_x
    } else {
        left_aligned_x
    };
    let mut y = if above {
        trigger_top - gap - height
    } else {
        trigger_bottom + gap
    };

    // `shift`: keep the chosen side and alignment, moving only enough to stay
    // inside the viewport. If a card is larger than the usable viewport, pin
    // it to the leading edge; a caller can then constrain its own contents.
    let usable_width = (viewport_right - viewport_left).max(0.0);
    if width <= usable_width {
        x = x.clamp(viewport_left, viewport_right - width);
    } else {
        x = viewport_left;
    }
    let usable_height = (viewport_bottom - viewport_top).max(0.0);
    if height <= usable_height {
        y = y.clamp(viewport_top, viewport_bottom - height);
    } else {
        y = viewport_top;
    }

    FloatingPlacement {
        bounds: Bounds::new(Point::new(px(x), px(y)), surface_size),
        align: MenuAlign::from_sides(above, right_aligned),
    }
}

/// Place the flyout beside the already positioned main card. Its size must
/// never participate in the main card's viewport fitting.
fn resolve_submenu_bounds(
    parent: Bounds<Pixels>,
    surface_size: Size<Pixels>,
    offset: Pixels,
    viewport: Bounds<Pixels>,
    margin: Pixels,
) -> Bounds<Pixels> {
    let left = viewport.left() + margin;
    let right = viewport.right() - margin;
    let top = viewport.top() + margin;
    let bottom = viewport.bottom() - margin;
    let right_x = parent.right() - px(SUBMENU_OVERLAP);
    let left_x = parent.left() + px(SUBMENU_OVERLAP) - surface_size.width;
    let right_space = right - right_x;
    let left_space = parent.left() + px(SUBMENU_OVERLAP) - left;
    let x = if surface_size.width <= right_space || right_space >= left_space {
        right_x
    } else {
        left_x
    };
    Bounds::new(
        Point::new(
            x.clamp(left, (right - surface_size.width).max(left)),
            (parent.top() + offset).clamp(top, (bottom - surface_size.height).max(top)),
        ),
        surface_size,
    )
}

enum FloatingAnchor {
    Trigger {
        bounds: Bounds<Pixels>,
        preferred: MenuAlign,
        gap: Pixels,
    },
    Submenu {
        parent_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
        offset: Pixels,
    },
}

/// A measured, trigger-aware deferred surface. This mirrors GPUI's
/// `Anchored` element lifecycle, but resolves placement from the trigger's
/// rectangle instead of a single point so vertical flips remain attached to
/// the correct edge.
struct FloatingSurface {
    child: AnyElement,
    anchor: FloatingAnchor,
    margin: Pixels,
}

struct FloatingSurfaceState {
    child_layout_id: LayoutId,
}

impl FloatingSurface {
    fn new(
        child: AnyElement,
        trigger: Bounds<Pixels>,
        preferred: MenuAlign,
        gap: Pixels,
        margin: Pixels,
    ) -> Self {
        Self {
            child,
            anchor: FloatingAnchor::Trigger {
                bounds: trigger,
                preferred,
                gap,
            },
            margin,
        }
    }

    fn submenu(
        child: AnyElement,
        parent_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
        offset: Pixels,
        margin: Pixels,
    ) -> Self {
        Self {
            child,
            anchor: FloatingAnchor::Submenu {
                parent_bounds,
                offset,
            },
            margin,
        }
    }
}

impl Element for FloatingSurface {
    type RequestLayoutState = FloatingSurfaceState;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let child_layout_id = self.child.request_layout(window, cx);
        let layout_id = window.request_layout(
            Style {
                position: Position::Absolute,
                display: Display::Flex,
                ..Style::default()
            },
            [child_layout_id],
            cx,
        );
        (layout_id, FloatingSurfaceState { child_layout_id })
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let surface_size = window.layout_bounds(request_layout.child_layout_id).size;
        let viewport = Bounds::new(Point::default(), window.viewport_size());
        let margin = self.margin + window.client_inset().unwrap_or(px(0.0));
        let placement = match &self.anchor {
            FloatingAnchor::Trigger {
                bounds,
                preferred,
                gap,
            } => {
                resolve_floating_placement(
                    *bounds,
                    surface_size,
                    viewport,
                    *preferred,
                    *gap,
                    margin,
                )
                .bounds
            }
            FloatingAnchor::Submenu {
                parent_bounds,
                offset,
            } => resolve_submenu_bounds(
                // The main card's probe runs before this sibling prepaints.
                parent_bounds.get().unwrap_or(bounds),
                surface_size,
                *offset,
                viewport,
                margin,
            ),
        };
        let offset = placement.origin - bounds.origin;
        let offset = Point::new(offset.x.round(), offset.y.round());
        window.with_element_offset(offset, |window| self.child.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

impl IntoElement for FloatingSurface {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// A dropdown menu anchored under its trigger, toggled by a left click.
///
/// Unlike a context menu it aligns to the trigger rather than the pointer, so
/// the handle carries the trigger's last-known bounds. Those are a frame old,
/// which is invisible in practice: a trigger does not move between the click
/// and the menu appearing.
pub fn dropdown_menu<E>(
    trigger: E,
    id: impl Into<ElementId>,
    handle: &ContextMenuHandle,
    align: MenuAlign,
    items: impl Fn(&mut App) -> Vec<MenuItem> + 'static,
) -> AnyElement
where
    E: ParentElement + Styled + InteractiveElement + IntoElement + 'static,
{
    let id: ElementId = id.into();
    let items = Rc::new(items);
    anchored_surface(trigger, handle, align, SurfaceFocus::Card, move |handle| {
        MenuCard {
            id: id.clone(),
            handle: handle.clone(),
            items: items.clone(),
            scrollable: false,
        }
        .into_any_element()
    })
}

/// A dropdown for large, flat option catalogs, with a bounded, virtualized list.
pub fn scrollable_dropdown_menu<E>(
    trigger: E,
    id: impl Into<ElementId>,
    handle: &ContextMenuHandle,
    align: MenuAlign,
    items: impl Fn(&mut App) -> Vec<MenuItem> + 'static,
) -> AnyElement
where
    E: ParentElement + Styled + InteractiveElement + IntoElement + 'static,
{
    let id: ElementId = id.into();
    let items = Rc::new(items);
    anchored_surface(trigger, handle, align, SurfaceFocus::Card, move |handle| {
        MenuCard {
            id: id.clone(),
            handle: handle.clone(),
            items: items.clone(),
            scrollable: true,
        }
        .into_any_element()
    })
}

/// A font menu can contain hundreds of families. Keep its rows virtualized and
/// use the same anchoring, focus, checks, and dismissal as other dropdowns.
pub fn font_dropdown_menu<E>(
    trigger: E,
    handle: &ContextMenuHandle,
    families: Rc<Vec<SharedString>>,
    special_label: SharedString,
    selected: Option<usize>,
    list_state: ListState,
    on_select: impl Fn(Option<String>, &mut Window, &mut App) + 'static,
) -> AnyElement
where
    E: ParentElement + Styled + InteractiveElement + IntoElement + 'static,
{
    let trigger_focus = handle.trigger_focus.clone();
    let on_select: Rc<dyn Fn(Option<String>, &mut Window, &mut App)> =
        Rc::new(move |family, window, cx| {
            window.focus(&trigger_focus, cx);
            on_select(family, window, cx);
        });
    anchored_surface(
        trigger,
        handle,
        MenuAlign::BelowRight,
        SurfaceFocus::Card,
        move |handle| {
            FontMenuCard {
                handle: handle.clone(),
                families: families.clone(),
                special_label: special_label.clone(),
                selected,
                list_state: list_state.clone(),
                on_select: on_select.clone(),
            }
            .into_any_element()
        },
    )
}

#[derive(IntoElement)]
struct FontMenuCard {
    handle: ContextMenuHandle,
    families: Rc<Vec<SharedString>>,
    /// Label for the fixed first row: System, or Auto for the CJK fallback.
    special_label: SharedString,
    selected: Option<usize>,
    list_state: ListState,
    #[allow(clippy::type_complexity)]
    on_select: Rc<dyn Fn(Option<String>, &mut Window, &mut App)>,
}

impl RenderOnce for FontMenuCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::current(cx);
        let count = self.families.len() + 1;
        let selected = self.selected;
        let highlighted = {
            let mut state = self.handle.state.borrow_mut();
            *state.highlighted.get_or_insert_with(|| {
                let index = selected.unwrap_or(0);
                self.list_state.scroll_to_reveal_item(index);
                index
            })
        };
        let row_handle = self.handle.clone();
        let row_families = self.families.clone();
        let row_select = self.on_select.clone();
        let rows = list(self.list_state.clone(), move |index, _, _| {
            let family = index
                .checked_sub(1)
                .and_then(|i| row_families.get(i))
                .cloned();
            let label = family.clone().unwrap_or_else(|| self.special_label.clone());
            let on_select = row_select.clone();
            let entry = row(
                index,
                highlighted == index,
                &theme,
                row_handle.clone(),
                Some(Rc::new(move |window, cx| {
                    on_select(family.as_ref().map(ToString::to_string), window, cx);
                })),
            )
            .h(px(ROW_HEIGHT))
            .text_color(theme.text)
            .child(div().flex_1().min_w_0().truncate().child(label))
            .when(selected == Some(index), |entry| {
                entry.child(icon("icons/check.svg", 11.0, theme.text_tertiary))
            });
            let hover_handle = row_handle.clone();
            let entry = entry.on_hover(move |hovered, window, _| {
                if *hovered {
                    hover_handle.state.borrow_mut().highlighted = Some(index);
                    window.refresh();
                }
            });
            div()
                .child(entry)
                .when(index == 0, |element| {
                    element.child(div().my(px(4.0)).mx(px(15.0)).h(px(1.0)).bg(theme.border))
                })
                .into_any_element()
        })
        .h(px((count as f32 * ROW_HEIGHT + SEPARATOR_HEIGHT)
            .min(320.0)
            .min((f32::from(window.viewport_size().height) - 32.0).max(0.0))));

        div()
            .id("font-menu")
            .occlude()
            .track_focus(&self.handle.focus)
            .tab_index(0)
            .key_context(FONT_MENU_CONTEXT)
            .w(px(300.0))
            .py(px(CARD_PADDING))
            .rounded(px(CARD_RADIUS))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.menu)
            .shadow_lg()
            .focus_visible(|style| style.border_color(theme.accent))
            .on_action({
                let handle = self.handle.clone();
                move |_: &DismissMenu, window, cx| {
                    handle.close(window, cx);
                    window.focus(&handle.trigger_focus, cx);
                    window.refresh();
                }
            })
            .on_mouse_down_out({
                let handle = self.handle.clone();
                move |event, window, cx| handle.dismiss_on_down_out(event, window, cx)
            })
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                let current = self
                    .handle
                    .state
                    .borrow()
                    .highlighted
                    .unwrap_or(highlighted);
                let next = match event.keystroke.key.as_str() {
                    "down" => (current + 1) % count,
                    "up" => (current + count - 1) % count,
                    "home" => 0,
                    "end" => count - 1,
                    "enter" | "space" => {
                        let family = current
                            .checked_sub(1)
                            .and_then(|index| self.families.get(index))
                            .map(ToString::to_string);
                        self.handle.close(window, cx);
                        (self.on_select)(family, window, cx);
                        window.refresh();
                        cx.stop_propagation();
                        return;
                    }
                    _ => return,
                };
                self.handle.state.borrow_mut().highlighted = Some(next);
                self.list_state.scroll_to_reveal_item(next);
                window.refresh();
                cx.stop_propagation();
            })
            .child(rows)
    }
}

/// A dropdown-anchored panel holding arbitrary content.
///
/// Same trigger, anchoring and dismissal as [`dropdown_menu`], but the card
/// draws no chrome — the content owns its own surface — and it does not take
/// focus, so a search field inside can. Escape still works: the card declares
/// the menu key context, and key dispatch walks up to it from the focused
/// descendant.
pub fn popover<E>(
    trigger: E,
    handle: &ContextMenuHandle,
    align: MenuAlign,
    content: impl Fn(&ContextMenuHandle, &mut Window, &mut App) -> AnyElement + 'static,
) -> AnyElement
where
    E: ParentElement + Styled + InteractiveElement + IntoElement + 'static,
{
    let content = Rc::new(content);
    anchored_surface(
        trigger,
        handle,
        align,
        SurfaceFocus::Content,
        move |handle| {
            PopoverCard {
                handle: handle.clone(),
                content: content.clone(),
            }
            .into_any_element()
        },
    )
}

/// Toggle a [`popover`] as if its trigger were clicked, for keyboard shortcuts.
///
/// Anchors to the trigger's last recorded bounds, so it no-ops until the
/// trigger has drawn at least once. The handle's toggle observers may update
/// the owning entity, so a caller holding that entity's lease must defer this.
pub fn toggle_popover(
    handle: &ContextMenuHandle,
    align: MenuAlign,
    window: &mut Window,
    cx: &mut App,
) {
    if handle.is_open() {
        handle.close(window, cx);
        window.refresh();
        return;
    }
    let Some(anchor) = handle
        .trigger_bounds
        .get()
        .map(|bounds| align.anchor_point(bounds, px(TRIGGER_GAP)))
    else {
        return;
    };
    open_menu(handle, anchor, SurfaceFocus::Content, true, window, cx);
}

/// The shared half of both dropdown surfaces: a trigger that records its bounds
/// and toggles the handle, plus the open card deferred and anchored to it.
fn anchored_surface<E>(
    trigger: E,
    handle: &ContextMenuHandle,
    align: MenuAlign,
    focus_target: SurfaceFocus,
    card: impl Fn(&ContextMenuHandle) -> AnyElement + 'static,
) -> AnyElement
where
    E: ParentElement + Styled + InteractiveElement + IntoElement + 'static,
{
    let open_at = handle.state.borrow().open;
    let press_handle = handle.clone();
    let toggle_handle = handle.clone();
    let cancel_handle = handle.clone();
    let key_handle = handle.clone();

    // Toggle on release, not press: the press only arms the trigger (and
    // keeps titlebar dragging and outer handlers from seeing it), and the
    // release must land back on the trigger to count as a click.
    let trigger = trigger
        .relative()
        .track_focus(&handle.trigger_focus)
        .tab_index(0)
        .child(trigger_bounds_probe(handle))
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            press_handle.trigger_pressed.set(true);
            cx.stop_propagation();
        })
        .on_mouse_up(MouseButton::Left, move |_, window, cx| {
            if toggle_handle.trigger_pressed.replace(false) {
                toggle_anchored_surface(&toggle_handle, align, focus_target, window, cx);
                cx.stop_propagation();
            }
        })
        .on_mouse_up_out(MouseButton::Left, move |_, _, _| {
            cancel_handle.trigger_pressed.set(false);
        })
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if key_handle.trigger_focus.is_focused(window)
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                toggle_anchored_surface(&key_handle, align, focus_target, window, cx);
                cx.stop_propagation();
            }
        });

    let Some(position) = open_at else {
        return trigger.into_any_element();
    };
    let trigger_bounds = handle
        .trigger_bounds
        .get()
        .unwrap_or_else(|| Bounds::new(position, Size::default()));

    trigger
        .child(
            deferred(FloatingSurface::new(
                card(handle),
                trigger_bounds,
                align,
                px(TRIGGER_GAP),
                px(8.0),
            ))
            .with_priority(1),
        )
        .into_any_element()
}

fn toggle_anchored_surface(
    handle: &ContextMenuHandle,
    align: MenuAlign,
    focus_target: SurfaceFocus,
    window: &mut Window,
    cx: &mut App,
) {
    if handle.is_open() {
        handle.close(window, cx);
        window.refresh();
        return;
    }
    let anchor = handle
        .trigger_bounds
        .get()
        .map(|bounds| align.anchor_point(bounds, px(TRIGGER_GAP)))
        .unwrap_or_else(|| window.mouse_position());
    open_menu(handle, anchor, focus_target, true, window, cx);
}

/// A chrome-less card: dismissal and the menu key context, nothing else.
#[derive(IntoElement)]
struct PopoverCard {
    handle: ContextMenuHandle,
    #[allow(clippy::type_complexity)]
    content: Rc<dyn Fn(&ContextMenuHandle, &mut Window, &mut App) -> AnyElement>,
}

impl RenderOnce for PopoverCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let body = (self.content)(&self.handle, window, cx);
        div()
            .occlude()
            .key_context(MENU_CONTEXT)
            .on_action({
                let handle = self.handle.clone();
                move |_: &DismissMenu, window, cx| {
                    handle.close(window, cx);
                    window.refresh();
                }
            })
            .on_mouse_down_out({
                let handle = self.handle.clone();
                move |event, window, cx| handle.dismiss_on_down_out(event, window, cx)
            })
            .child(body)
    }
}

/// Attach a context menu to `element`.
///
/// `items` is called only when the menu opens, so building the item list — which
/// may capture message content or run availability checks — never costs
/// anything on an ordinary frame.
pub fn context_menu<E>(
    element: E,
    id: impl Into<ElementId>,
    handle: &ContextMenuHandle,
    items: impl Fn(&mut App) -> Vec<MenuItem> + 'static,
) -> AnyElement
where
    E: ParentElement + Styled + InteractiveElement + IntoElement + 'static,
{
    context_menu_with_priority(element, id, handle, 1, items)
}

/// Place a context menu above a deferred surface such as an image preview.
pub fn context_menu_with_priority<E>(
    element: E,
    id: impl Into<ElementId>,
    handle: &ContextMenuHandle,
    priority: usize,
    items: impl Fn(&mut App) -> Vec<MenuItem> + 'static,
) -> AnyElement
where
    E: ParentElement + Styled + InteractiveElement + IntoElement + 'static,
{
    let id: ElementId = id.into();
    let open_at = handle.state.borrow().open;
    let handle_for_down = handle.clone();
    let items = Rc::new(items);
    let items_for_down = items.clone();

    let element = element
        .relative()
        .child(trigger_bounds_probe(handle))
        .on_mouse_down(
            MouseButton::Right,
            move |event: &MouseDownEvent, window, cx| {
                if handle_for_down
                    .state
                    .borrow()
                    .pending_context_items
                    .is_empty()
                    && items_for_down(cx).is_empty()
                {
                    return;
                }
                open_menu(
                    &handle_for_down,
                    event.position,
                    SurfaceFocus::Card,
                    false,
                    window,
                    cx,
                );
                cx.stop_propagation();
                window.prevent_default();
            },
        );

    let Some(position) = open_at else {
        return element.into_any_element();
    };

    element
        .child(
            // A point-sized trigger lets either corner stay at the cursor
            // when the menu flips, before shifting for the window margin.
            deferred(FloatingSurface::new(
                MenuCard {
                    id,
                    handle: handle.clone(),
                    items,
                    scrollable: false,
                }
                .into_any_element(),
                Bounds::new(position, Size::default()),
                MenuAlign::BelowLeft,
                px(0.0),
                px(8.0),
            ))
            .with_priority(priority),
        )
        .into_any_element()
}

#[derive(IntoElement)]
struct MenuCard {
    id: ElementId,
    handle: ContextMenuHandle,
    scrollable: bool,
    #[allow(clippy::type_complexity)]
    items: Rc<dyn Fn(&mut App) -> Vec<MenuItem>>,
}

impl RenderOnce for MenuCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::current(cx);
        let parent_bounds = Rc::new(Cell::new(None));
        self.handle.submenu_bounds.set(None);
        let item_builder: Rc<dyn Fn(&mut App) -> Vec<MenuItem>> = {
            let handle = self.handle.clone();
            let base_items = self.items.clone();
            Rc::new(move |cx| {
                let mut items = handle.state.borrow().context_items.clone();
                items.extend(base_items(cx));
                items
            })
        };
        let items = item_builder(cx);
        let focusable = focusable_indexes(&items);
        let list_state = self.scrollable.then(|| {
            let mut state = self.handle.state.borrow_mut();
            if state
                .list
                .as_ref()
                .is_none_or(|list| list.item_count() != items.len())
            {
                let list = ListState::new(items.len(), ListAlignment::Top, px(ROW_HEIGHT))
                    .with_uniform_item_height(px(ROW_HEIGHT));
                state.highlighted = items
                    .iter()
                    .position(|item| {
                        matches!(
                            item,
                            MenuItem::Entry {
                                selected: true,
                                disabled: false,
                                ..
                            }
                        )
                    })
                    .or_else(|| focusable.first().copied());
                if let Some(index) = state.highlighted {
                    // The list has no viewport before its first layout, so
                    // reveal-item would align past the selected row's bottom.
                    list.scroll_to(gpui::ListOffset {
                        item_ix: index,
                        offset_in_item: px(0.0),
                    });
                }
                state.list = Some(list);
            }
            state.list.as_ref().unwrap().clone()
        });
        let (highlighted, active_submenu, submenu_highlighted) = {
            let state = self.handle.state.borrow();
            (
                state.highlighted,
                state.active_submenu,
                state.submenu_highlighted,
            )
        };
        let submenu = active_submenu.and_then(|index| {
            let MenuItem::Submenu { items, .. } = items.get(index)? else {
                return None;
            };
            Some((index, items(cx)))
        });

        let submenu_offset = submenu.as_ref().map(|(parent_index, _)| {
            items
                .iter()
                .take(*parent_index)
                .map(|item| match item {
                    MenuItem::Separator => SEPARATOR_HEIGHT,
                    MenuItem::Header(_) => 22.0,
                    MenuItem::Entry { .. } | MenuItem::Submenu { .. } | MenuItem::Custom { .. } => {
                        ROW_HEIGHT
                    }
                })
                .sum::<f32>()
        });

        let mut root_card = div()
            .font_weight(DEFAULT_FONT_WEIGHT)
            .id(self.id)
            .min_w(px(176.0))
            .max_w(px(320.0))
            .when(self.scrollable, |card| {
                card.w(px(320.0))
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            })
            .py(px(CARD_PADDING))
            .rounded(px(CARD_RADIUS))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.menu)
            .shadow_lg()
            .flex()
            .flex_col();
        #[cfg(test)]
        {
            root_card = root_card.debug_selector(|| "MENU_ROOT".into());
        }

        if let Some(list_state) = list_state {
            let height = (items.len() as f32 * ROW_HEIGHT)
                .min(320.0)
                .min((f32::from(window.viewport_size().height) - 32.0).max(0.0));
            let handle = self.handle.clone();
            root_card = root_card.child(
                list(list_state, move |index, window, cx| {
                    render_menu_item(
                        items[index].clone(),
                        index,
                        highlighted == Some(index),
                        false,
                        &theme,
                        handle.clone(),
                        window,
                        cx,
                    )
                })
                .h(px(height)),
            );
        } else {
            for (index, item) in items.into_iter().enumerate() {
                root_card = root_card.child(render_menu_item(
                    item,
                    index,
                    highlighted == Some(index) || active_submenu == Some(index),
                    false,
                    &theme,
                    self.handle.clone(),
                    window,
                    cx,
                ));
            }
        }

        let mut surface = div()
            .relative()
            .occlude()
            .track_focus(&self.handle.focus)
            .key_context(MENU_CONTEXT)
            .flex()
            .items_start()
            .on_action({
                let handle = self.handle.clone();
                move |_: &DismissMenu, window, cx| {
                    handle.close(window, cx);
                    window.refresh();
                }
            })
            .on_mouse_down_out({
                let handle = self.handle.clone();
                move |event, window, cx| handle.dismiss_on_down_out(event, window, cx)
            })
            .on_key_down({
                let handle = self.handle.clone();
                let focusable = focusable.clone();
                let items = item_builder.clone();
                move |event: &KeyDownEvent, window, cx| {
                    on_menu_key(&handle, &focusable, &items, event, window, cx);
                }
            })
            .child(root_card)
            .child(bounds_probe(parent_bounds.clone()));

        if let Some((parent_index, submenu_items)) = submenu {
            let offset = submenu_offset.unwrap_or(0.0);
            let mut submenu_card = div()
                .id(SharedString::from(format!("submenu-{parent_index}")))
                .font_weight(DEFAULT_FONT_WEIGHT)
                .occlude()
                .min_w(px(176.0))
                .max_w(px(320.0))
                .py(px(CARD_PADDING))
                .rounded(px(CARD_RADIUS))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.menu)
                .shadow_lg()
                .flex()
                .flex_col();
            #[cfg(test)]
            {
                submenu_card = submenu_card.debug_selector(|| "MENU_SUBMENU".into());
            }
            for (index, item) in submenu_items.into_iter().enumerate() {
                submenu_card = submenu_card.child(render_menu_item(
                    item,
                    index,
                    submenu_highlighted == Some(index),
                    true,
                    &theme,
                    self.handle.clone(),
                    window,
                    cx,
                ));
            }
            surface = surface.child(FloatingSurface::submenu(
                div()
                    .relative()
                    .child(submenu_card)
                    .child(bounds_probe(self.handle.submenu_bounds.clone()))
                    .into_any_element(),
                parent_bounds,
                px(offset),
                px(8.0),
            ));
        }
        surface
    }
}

/// Card and row geometry, modeled on the native macOS menu: a generously
/// rounded card, highlights inset from its edge, and dividers that start at
/// the label column rather than spanning the card.
const CARD_PADDING: f32 = 5.0;
const CARD_RADIUS: f32 = 12.0;
const ROW_PADDING: f32 = 10.0;
const ROW_HEIGHT: f32 = 28.0;
const ROW_RADIUS: f32 = 7.0;
const SEPARATOR_HEIGHT: f32 = 9.0;

fn render_menu_item(
    item: MenuItem,
    index: usize,
    highlighted: bool,
    in_submenu: bool,
    theme: &Theme,
    handle: ContextMenuHandle,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    match item {
        // Inset to the label column, like a native macOS menu.
        MenuItem::Separator => div()
            .my(px((SEPARATOR_HEIGHT - 1.0) / 2.0))
            .mx(px(CARD_PADDING + ROW_PADDING))
            .h(px(1.0))
            .bg(theme.border)
            .into_any_element(),
        MenuItem::Header(label) => div()
            .px(px(CARD_PADDING + ROW_PADDING))
            .pt(px(6.0))
            .pb(px(2.0))
            .text_size(sp(SMALL_UI_FONT_SIZE))
            .line_height(sp(14.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_tertiary)
            .child(label)
            .into_any_element(),
        MenuItem::Entry {
            label,
            icon: item_icon,
            selected,
            disabled,
            destructive,
            on_click,
        } => {
            let color = match (disabled, destructive) {
                (true, _) => theme.text_ghost,
                (false, true) => theme.danger,
                (false, false) => theme.text,
            };
            let entry = row(
                index,
                highlighted,
                theme,
                handle.clone(),
                (!disabled).then_some(on_click),
            )
            .text_color(color)
            .when(selected, |element| element.font_weight(FontWeight::MEDIUM))
            .when_some(item_icon, |element, path| {
                element.child(icon(path, 12.0, color))
            })
            .child(div().flex_1().min_w_0().truncate().child(label))
            .when(selected, |element| {
                element.child(icon("icons/check.svg", 11.0, theme.text_tertiary))
            });
            track_pointer_highlight(entry, index, in_submenu, disabled, handle).into_any_element()
        }
        MenuItem::Submenu { label, items: _ } => {
            // Raku currently exposes one flyout level. Keeping a nested
            // submenu row inert prevents a child builder from accidentally
            // stealing the parent flyout's keyboard state.
            if in_submenu {
                return row(index, highlighted, theme, handle, None)
                    .text_color(theme.text_ghost)
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(icon("icons/chevron-right.svg", 10.0, theme.text_ghost))
                    .into_any_element();
            }
            let hover = theme.overlay;
            let hover_handle = handle.clone();
            let click_handle = handle.clone();
            row(index, highlighted, theme, handle, None)
                .cursor_default()
                .hover(move |element| element.bg(hover))
                .text_color(theme.text)
                .on_hover(move |hovered, window, _| {
                    if *hovered {
                        open_submenu(&hover_handle, index, false);
                        window.refresh();
                    }
                })
                .on_click(move |_, window, cx| {
                    open_submenu(&click_handle, index, false);
                    window.refresh();
                    cx.stop_propagation();
                })
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(icon("icons/chevron-right.svg", 10.0, theme.text_tertiary))
                .into_any_element()
        }
        MenuItem::Custom { render, on_click } => {
            let body = render(window, cx);
            match on_click {
                Some(on_click) => {
                    let entry =
                        row(index, highlighted, theme, handle.clone(), Some(on_click)).child(body);
                    track_pointer_highlight(entry, index, in_submenu, false, handle)
                        .into_any_element()
                }
                // Non-interactive rows still need the row's insets so they
                // line up with the entries around them.
                None => div()
                    .mx(px(CARD_PADDING))
                    .px(px(ROW_PADDING))
                    .child(body)
                    .into_any_element(),
            }
        }
    }
}

fn open_submenu(handle: &ContextMenuHandle, index: usize, keyboard: bool) {
    let mut state = handle.state.borrow_mut();
    if state.active_submenu != Some(index) {
        state.submenu_highlighted = None;
    }
    state.highlighted = Some(index);
    state.active_submenu = Some(index);
    state.submenu_focused = keyboard;
}

fn track_pointer_highlight(
    row: gpui::Stateful<gpui::Div>,
    index: usize,
    in_submenu: bool,
    disabled: bool,
    handle: ContextMenuHandle,
) -> gpui::Stateful<gpui::Div> {
    row.when(!disabled, |row| {
        row.on_hover(move |hovered, window, _| {
            let mut state = handle.state.borrow_mut();
            if !*hovered {
                // Leaving a row clears the highlight it took, like a native
                // menu. Only clear our own index so a hover-in on the next row
                // that lands first is not undone.
                let slot = if in_submenu {
                    &mut state.submenu_highlighted
                } else {
                    &mut state.highlighted
                };
                if *slot == Some(index) {
                    *slot = None;
                    window.refresh();
                }
                return;
            }
            if in_submenu {
                state.submenu_highlighted = Some(index);
            } else {
                state.highlighted = Some(index);
                state.active_submenu = None;
                state.submenu_highlighted = None;
                state.submenu_focused = false;
            }
            window.refresh();
        })
    })
}

/// The shared row: consistent insets, plus hover, keyboard highlight and
/// close-then-act when it has a handler. A `None` handler renders the same
/// geometry inert, which is how a disabled entry keeps the menu's shape.
fn row(
    index: usize,
    highlighted: bool,
    theme: &Theme,
    handle: ContextMenuHandle,
    on_click: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
) -> gpui::Stateful<gpui::Div> {
    let hover = theme.overlay;
    let highlight = theme.overlay_strong;
    div()
        .id(index)
        .mx(px(CARD_PADDING))
        .px(px(ROW_PADDING))
        .min_h(px(ROW_HEIGHT))
        .rounded(px(ROW_RADIUS))
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(sp(13.0))
        .line_height(sp(16.0))
        .when(highlighted, |element| element.bg(highlight))
        .when_some(on_click, |element, on_click| {
            element
                .cursor_default()
                .hover(move |element| element.bg(hover))
                .on_click(move |_, window, cx| {
                    handle.close(window, cx);
                    on_click(window, cx);
                    window.refresh();
                })
        })
}

fn focusable_indexes(items: &[MenuItem]) -> Rc<Vec<usize>> {
    Rc::new(
        items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.is_focusable())
            .map(|(index, _)| index)
            .collect(),
    )
}

/// The next highlighted item index for a navigation key, wrapping at both ends.
/// `current` and the result are indexes into the *item list*, not into
/// `focusable`. `None` means the key does not navigate.
fn next_highlight(focusable: &[usize], current: Option<usize>, key: &str) -> Option<usize> {
    if focusable.is_empty() {
        return None;
    }
    let position =
        current.and_then(|item| focusable.iter().position(|candidate| *candidate == item));
    let next = match key {
        "down" => position.map_or(0, |index| (index + 1) % focusable.len()),
        "up" => position.map_or(focusable.len() - 1, |index| {
            (index + focusable.len() - 1) % focusable.len()
        }),
        "home" => 0,
        "end" => focusable.len() - 1,
        _ => return None,
    };
    Some(focusable[next])
}

fn on_menu_key(
    handle: &ContextMenuHandle,
    focusable: &[usize],
    items: &Rc<dyn Fn(&mut App) -> Vec<MenuItem>>,
    event: &KeyDownEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let key = event.keystroke.key.as_str();
    if key == "escape" {
        handle.close(window, cx);
        window.refresh();
        cx.stop_propagation();
        return;
    }

    let (submenu_focused, active_submenu, submenu_current) = {
        let state = handle.state.borrow();
        (
            state.submenu_focused,
            state.active_submenu,
            state.submenu_highlighted,
        )
    };
    if submenu_focused {
        let submenu_items = active_submenu
            .and_then(|index| items(cx).into_iter().nth(index))
            .and_then(|item| match item {
                MenuItem::Submenu { items, .. } => Some(items(cx)),
                _ => None,
            });
        let Some(submenu_items) = submenu_items else {
            let mut state = handle.state.borrow_mut();
            state.active_submenu = None;
            state.submenu_highlighted = None;
            state.submenu_focused = false;
            window.refresh();
            return;
        };

        if key == "left" {
            let mut state = handle.state.borrow_mut();
            state.submenu_focused = false;
            state.submenu_highlighted = None;
            window.refresh();
            cx.stop_propagation();
            return;
        }

        let submenu_focusable = focusable_indexes(&submenu_items);
        if let Some(next) = next_highlight(&submenu_focusable, submenu_current, key) {
            handle.state.borrow_mut().submenu_highlighted = Some(next);
            window.refresh();
            cx.stop_propagation();
            return;
        }

        if matches!(key, "enter" | "space") {
            cx.stop_propagation();
            let Some(highlighted) = handle.state.borrow().submenu_highlighted else {
                return;
            };
            let activated = submenu_items
                .into_iter()
                .nth(highlighted)
                .and_then(MenuItem::click_handler);
            if let Some(on_click) = activated {
                handle.close(window, cx);
                on_click(window, cx);
                window.refresh();
            }
        }
        return;
    }

    if key == "left" && active_submenu.is_some() {
        let mut state = handle.state.borrow_mut();
        state.active_submenu = None;
        state.submenu_highlighted = None;
        state.submenu_focused = false;
        window.refresh();
        cx.stop_propagation();
        return;
    }
    if focusable.is_empty() {
        return;
    }

    let current = handle.state.borrow().highlighted;
    if let Some(next) = next_highlight(focusable, current, key) {
        let mut state = handle.state.borrow_mut();
        state.highlighted = Some(next);
        if let Some(list) = &state.list {
            list.scroll_to_reveal_item(next);
        }
        state.active_submenu = None;
        state.submenu_highlighted = None;
        state.submenu_focused = false;
        window.refresh();
        cx.stop_propagation();
        return;
    }

    if matches!(key, "right" | "enter" | "space") {
        cx.stop_propagation();
        let Some(highlighted) = handle.state.borrow().highlighted else {
            return;
        };
        // Rebuild to reach the entry's closure: the item list is intentionally
        // not retained between frames.
        let Some(item) = items(cx).into_iter().nth(highlighted) else {
            return;
        };
        match item {
            MenuItem::Submenu { items, .. } => {
                let submenu_items = items(cx);
                let first = focusable_indexes(&submenu_items).first().copied();
                let mut state = handle.state.borrow_mut();
                state.active_submenu = Some(highlighted);
                state.submenu_highlighted = first;
                state.submenu_focused = true;
                window.refresh();
            }
            item if matches!(key, "enter" | "space") => {
                if let Some(on_click) = item.click_handler() {
                    handle.close(window, cx);
                    on_click(window, cx);
                    window.refresh();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui::{Context, Modifiers, Render, TestAppContext, point, size};

    use super::*;

    /// Which anchored surface the harness mounts; both share
    /// `anchored_surface` but dismiss through different cards.
    #[derive(Clone, Copy)]
    enum Surface {
        Popover,
        Dropdown,
        ScrollableDropdown,
        Context,
        DeferredContext,
    }

    struct Harness {
        handle: ContextMenuHandle,
        surface: Surface,
    }

    struct FocusedPopoverHarness {
        handle: ContextMenuHandle,
        descendant_focus: FocusHandle,
    }

    struct SubmenuHarness {
        handle: ContextMenuHandle,
        activated: Rc<Cell<bool>>,
    }

    struct ContextSubmenuHarness {
        handle: ContextMenuHandle,
        activated: Rc<Cell<bool>>,
    }

    #[test]
    fn floating_surface_keeps_a_preferred_side_that_fits() {
        let placement = resolve_floating_placement(
            Bounds::new(point(px(600.0), px(200.0)), size(px(100.0), px(20.0))),
            size(px(240.0), px(180.0)),
            Bounds::new(Point::default(), size(px(800.0), px(600.0))),
            MenuAlign::BelowRight,
            px(4.0),
            px(8.0),
        );

        assert_eq!(placement.align, MenuAlign::BelowRight);
        assert_eq!(placement.bounds.origin, point(px(460.0), px(224.0)));
    }

    #[test]
    fn floating_surface_flips_across_the_trigger_when_below_does_not_fit() {
        let trigger = Bounds::new(point(px(600.0), px(500.0)), size(px(100.0), px(20.0)));
        let placement = resolve_floating_placement(
            trigger,
            size(px(240.0), px(180.0)),
            Bounds::new(Point::default(), size(px(800.0), px(600.0))),
            MenuAlign::BelowRight,
            px(4.0),
            px(8.0),
        );

        assert_eq!(placement.align, MenuAlign::AboveRight);
        assert_eq!(placement.bounds.origin, point(px(460.0), px(316.0)));
        assert_eq!(placement.bounds.bottom() + px(4.0), trigger.top());
    }

    #[test]
    fn floating_surface_flips_alignment_before_shifting() {
        let placement = resolve_floating_placement(
            Bounds::new(point(px(10.0), px(200.0)), size(px(40.0), px(20.0))),
            size(px(240.0), px(180.0)),
            Bounds::new(Point::default(), size(px(800.0), px(600.0))),
            MenuAlign::BelowRight,
            px(4.0),
            px(8.0),
        );

        assert_eq!(placement.align, MenuAlign::BelowLeft);
        assert_eq!(placement.bounds.origin, point(px(10.0), px(224.0)));
    }

    #[test]
    fn floating_surface_shifts_oversized_content_to_the_viewport_margin() {
        let placement = resolve_floating_placement(
            Bounds::new(point(px(100.0), px(200.0)), size(px(40.0), px(20.0))),
            size(px(900.0), px(180.0)),
            Bounds::new(Point::default(), size(px(800.0), px(600.0))),
            MenuAlign::BelowLeft,
            px(4.0),
            px(8.0),
        );

        assert_eq!(placement.bounds.origin.x, px(8.0));
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let trigger = div().w(px(120.0)).h(px(32.0));
            div()
                .size_full()
                .tab_index(0)
                .tab_group()
                .tab_stop(false)
                .child(match self.surface {
                    Surface::Popover => {
                        popover(trigger, &self.handle, MenuAlign::BelowLeft, |_, _, _| {
                            div().w(px(200.0)).h(px(100.0)).into_any_element()
                        })
                    }
                    Surface::Dropdown => dropdown_menu(
                        trigger,
                        "dropdown",
                        &self.handle,
                        MenuAlign::BelowLeft,
                        |_| vec![MenuItem::new("Entry", |_, _| {})],
                    ),
                    Surface::ScrollableDropdown => scrollable_dropdown_menu(
                        trigger,
                        "catalog-dropdown",
                        &self.handle,
                        MenuAlign::BelowLeft,
                        |_| {
                            (0..1_000)
                                .map(|index| {
                                    MenuItem::new(format!("Model {index}"), |_, _| {})
                                        .selected(index == 400)
                                })
                                .collect()
                        },
                    ),
                    Surface::Context => context_menu(trigger, "context", &self.handle, |_| {
                        vec![MenuItem::new("Entry", |_, _| {})]
                    }),
                    Surface::DeferredContext => deferred(context_menu_with_priority(
                        trigger
                            .track_focus(self.handle.trigger_focus_handle())
                            .tab_index(0),
                        "preview-context",
                        &self.handle,
                        6,
                        |_| vec![MenuItem::new("Entry", |_, _| {})],
                    ))
                    .with_priority(5)
                    .into_any_element(),
                })
        }
    }

    impl Render for FocusedPopoverHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let descendant_focus = self.descendant_focus.clone();
            popover(
                div().w(px(120.0)).h(px(32.0)),
                &self.handle,
                MenuAlign::BelowLeft,
                move |_, _, _| {
                    div()
                        .track_focus(&descendant_focus)
                        .w(px(200.0))
                        .h(px(100.0))
                        .into_any_element()
                },
            )
        }
    }

    impl Render for SubmenuHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let activated = self.activated.clone();
            dropdown_menu(
                div().w(px(120.0)).h(px(32.0)),
                "submenu-dropdown",
                &self.handle,
                MenuAlign::BelowLeft,
                move |_| {
                    let activated = activated.clone();
                    vec![MenuItem::submenu("Grouping", move |_| {
                        let activated = activated.clone();
                        vec![MenuItem::new("Project", move |_, _| {
                            activated.set(true);
                        })]
                    })]
                },
            )
        }
    }

    impl Render for ContextSubmenuHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let activated = self.activated.clone();
            context_menu(div().size_full(), "file-menu", &self.handle, move |_| {
                let activated = activated.clone();
                vec![
                    MenuItem::new("Attach as context", |_, _| {}),
                    MenuItem::submenu("Copy", move |_| {
                        let activated = activated.clone();
                        vec![
                            MenuItem::new("Absolute path", |_, _| {}).disabled(true),
                            MenuItem::new("Relative path", move |_, _| activated.set(true)),
                            MenuItem::new("Filename", |_, _| {}),
                        ]
                    }),
                    MenuItem::Separator,
                    MenuItem::new("Open in Default App", |_, _| {}),
                    MenuItem::new("Show in File Manager", |_, _| {}),
                ]
            })
        }
    }

    /// The trigger sits at the window origin, 120×32; the card hangs below it,
    /// so a point inside the trigger is outside the card and vice versa.
    fn assert_trigger_toggles(surface: Surface, cx: &mut TestAppContext) {
        let handle = cx.update(ContextMenuHandle::new);
        let harness = Harness {
            handle: handle.clone(),
            surface,
        };
        let (_view, cx) = cx.add_window_view(|_, _| harness);
        let on_trigger = point(px(10.0), px(10.0));
        let outside = point(px(500.0), px(400.0));

        cx.simulate_mouse_down(on_trigger, MouseButton::Left, Modifiers::none());
        assert!(
            !handle.is_open(),
            "a press alone must not open; release does"
        );
        cx.simulate_mouse_up(on_trigger, MouseButton::Left, Modifiers::none());
        assert!(handle.is_open(), "first trigger click should open");

        // The card's capture-phase `on_mouse_down_out` sees this click first;
        // without the trigger exemption it closes the menu and the trigger's
        // own handler reopens it.
        cx.simulate_click(on_trigger, Modifiers::none());
        assert!(!handle.is_open(), "second trigger click should close");

        cx.simulate_click(on_trigger, Modifiers::none());
        assert!(handle.is_open(), "trigger click after close should reopen");

        cx.simulate_mouse_down(outside, MouseButton::Left, Modifiers::none());
        assert!(!handle.is_open(), "click outside should dismiss");
    }

    #[gpui::test]
    fn popover_trigger_toggles(cx: &mut TestAppContext) {
        assert_trigger_toggles(Surface::Popover, cx);
    }

    #[gpui::test]
    fn dropdown_trigger_toggles(cx: &mut TestAppContext) {
        assert_trigger_toggles(Surface::Dropdown, cx);
    }

    #[gpui::test]
    fn scrollable_dropdown_keeps_large_catalog_reachable(cx: &mut TestAppContext) {
        let handle = cx.update(ContextMenuHandle::new);
        let (_, window) = cx.add_window_view(|_, _| Harness {
            handle: handle.clone(),
            surface: Surface::ScrollableDropdown,
        });
        window.simulate_resize(size(px(800.0), px(240.0)));
        window.simulate_click(point(px(10.0), px(10.0)), Modifiers::none());
        window.run_until_parked();

        let card = window.debug_bounds("MENU_ROOT").expect("catalog menu");
        assert!(card.top() >= px(8.0));
        assert!(card.bottom() <= px(232.0));
        let list = handle.state.borrow().list.as_ref().unwrap().clone();
        let viewport = list.viewport_bounds();
        let selected = list
            .bounds_for_item(400)
            .expect("selected model is visible");
        assert!(
            viewport.contains(&selected.center()),
            "selected {selected:?} outside {viewport:?}; scroll {:?}",
            list.logical_scroll_top(),
        );
        let before = list.logical_scroll_top().item_ix;

        window.simulate_event(gpui::ScrollWheelEvent {
            position: viewport.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(-560.0))),
            ..Default::default()
        });
        window.run_until_parked();
        assert!(list.logical_scroll_top().item_ix > before);

        window.update(|window, cx| window.focus(handle.focus_handle(), cx));
        window.simulate_keystrokes("home");
        assert_eq!(handle.state.borrow().highlighted, Some(0));
        assert_eq!(list.logical_scroll_top().item_ix, 0);
        window.simulate_keystrokes("end");
        assert_eq!(handle.state.borrow().highlighted, Some(999));
        let last = list.bounds_for_item(999).expect("last model is visible");
        assert!(list.viewport_bounds().contains(&last.center()));
        window.simulate_keystrokes("enter");
        assert!(!handle.is_open(), "the last model can be activated");

        window.simulate_click(point(px(10.0), px(10.0)), Modifiers::none());
        window.run_until_parked();
        let reopened = handle.state.borrow().list.as_ref().unwrap().clone();
        let selected = reopened
            .bounds_for_item(400)
            .expect("selected model on reopen");
        assert!(reopened.viewport_bounds().contains(&selected.center()));
    }

    #[gpui::test]
    fn context_menu_trigger_click_dismisses(cx: &mut TestAppContext) {
        let handle = cx.update(ContextMenuHandle::new);
        let harness = Harness {
            handle: handle.clone(),
            surface: Surface::Context,
        };
        let (_view, cx) = cx.add_window_view(|_, _| harness);
        let on_trigger = point(px(10.0), px(10.0));

        cx.update(|window, cx| handle.open_context_menu(window, cx));
        assert!(handle.is_open());
        cx.run_until_parked();
        cx.simulate_mouse_down(on_trigger, MouseButton::Left, Modifiers::none());
        assert!(!handle.is_open(), "a context-menu row click should dismiss");
    }

    fn assert_trigger_opens_from_keyboard(surface: Surface, cx: &mut TestAppContext) {
        let handle = cx.update(ContextMenuHandle::new);
        let harness = Harness {
            handle: handle.clone(),
            surface,
        };
        let (_view, cx) = cx.add_window_view(|_, _| harness);

        cx.update(|window, cx| window.focus(&handle.trigger_focus, cx));
        cx.simulate_keystrokes("enter");
        assert!(handle.is_open(), "enter on the tab stop should open");
    }

    #[gpui::test]
    fn popover_trigger_is_keyboard_operable(cx: &mut TestAppContext) {
        assert_trigger_opens_from_keyboard(Surface::Popover, cx);
    }

    #[gpui::test]
    fn context_menu_above_a_deferred_preview_accepts_pointer_and_keyboard_input(
        cx: &mut TestAppContext,
    ) {
        let handle = cx.update(ContextMenuHandle::new);
        let restore_focus = handle.trigger_focus_handle().clone();
        let handle = handle.on_toggle(move |open, window, cx| {
            if !open {
                window.focus(&restore_focus, cx);
            }
        });
        let (_, window) = cx.add_window_view(|_, _| Harness {
            handle: handle.clone(),
            surface: Surface::DeferredContext,
        });
        window.simulate_mouse_down(
            point(px(10.0), px(10.0)),
            MouseButton::Right,
            Modifiers::none(),
        );
        assert!(handle.is_open());
        window.run_until_parked();
        window.update(|window, cx| window.focus(handle.focus_handle(), cx));
        window.simulate_keystrokes("down enter");
        assert!(
            !handle.is_open(),
            "the menu should receive and activate the selected action"
        );
        assert!(
            window.update(|window, _| handle.trigger_focus_handle().is_focused(window)),
            "closing the image menu restores keyboard focus to the preview"
        );
    }

    #[gpui::test]
    fn popover_descendant_space_does_not_toggle_trigger(cx: &mut TestAppContext) {
        let handle = cx.update(ContextMenuHandle::new);
        let descendant_focus = cx.update(|cx| cx.focus_handle());
        let harness = FocusedPopoverHarness {
            handle: handle.clone(),
            descendant_focus: descendant_focus.clone(),
        };
        let (_view, cx) = cx.add_window_view(|_, _| harness);

        cx.simulate_click(point(px(10.0), px(10.0)), Modifiers::none());
        assert!(handle.is_open());
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&descendant_focus, cx));
        cx.simulate_keystrokes("space");

        assert!(
            handle.is_open(),
            "space from focused popover content must not toggle the trigger"
        );
    }

    #[gpui::test]
    fn dropdown_trigger_is_keyboard_operable(cx: &mut TestAppContext) {
        assert_trigger_opens_from_keyboard(Surface::Dropdown, cx);
    }

    #[gpui::test]
    fn submenu_is_keyboard_operable(cx: &mut TestAppContext) {
        let handle = cx.update(ContextMenuHandle::new);
        let activated = Rc::new(Cell::new(false));
        let harness = SubmenuHarness {
            handle: handle.clone(),
            activated: activated.clone(),
        };
        let (_view, cx) = cx.add_window_view(|_, _| harness);

        cx.simulate_click(point(px(10.0), px(10.0)), Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&handle.focus, cx));
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("right");
        assert!(handle.state.borrow().submenu_focused);
        cx.simulate_keystrokes("enter");

        assert!(activated.get());
        assert!(!handle.is_open());
    }

    #[gpui::test]
    fn hovering_a_submenu_keeps_the_parent_menu_in_place(cx: &mut TestAppContext) {
        let handle = cx.update(ContextMenuHandle::new);
        let activated = Rc::new(Cell::new(false));
        let (_, window) = cx.add_window_view(|_, _| ContextSubmenuHarness {
            handle: handle.clone(),
            activated: activated.clone(),
        });
        window.simulate_resize(size(px(800.0), px(600.0)));

        for position in [
            point(px(40.0), px(160.0)),
            point(px(740.0), px(160.0)),
            point(px(740.0), px(570.0)),
        ] {
            activated.set(false);
            window.simulate_mouse_down(position, MouseButton::Right, Modifiers::none());
            window.run_until_parked();
            let before = window.debug_bounds("MENU_ROOT").expect("parent menu");
            window.simulate_mouse_move(
                point(before.left() + px(20.0), before.top() + px(44.0)),
                None,
                Modifiers::none(),
            );
            window.run_until_parked();

            assert_eq!(handle.state.borrow().active_submenu, Some(1));
            let parent = window.debug_bounds("MENU_ROOT").expect("parent menu");
            let submenu = window.debug_bounds("MENU_SUBMENU").expect("copy submenu");
            assert_eq!(parent, before, "opening a flyout must not move its parent");
            assert!(submenu.left() >= px(8.0));
            assert!(submenu.right() <= px(792.0));
            assert!(submenu.top() >= px(8.0));
            assert!(submenu.bottom() <= px(592.0));
            if position.x > px(400.0) {
                assert!(submenu.right() <= parent.left() + px(4.0));
            } else {
                assert!(submenu.left() >= parent.right() - px(4.0));
            }

            window.simulate_click(
                point(submenu.left() + px(20.0), submenu.top() + px(18.0)),
                Modifiers::none(),
            );
            assert!(handle.is_open(), "a disabled submenu item must not dismiss");
            window.simulate_click(
                point(submenu.left() + px(20.0), submenu.top() + px(44.0)),
                Modifiers::none(),
            );
            assert!(activated.get(), "submenu entries remain clickable");
            assert!(!handle.is_open());
        }
    }

    fn items() -> Vec<MenuItem> {
        vec![
            MenuItem::new("Copy", |_, _| {}),
            MenuItem::Separator,
            MenuItem::Separator,
            MenuItem::new("Revert", |_, _| {}),
        ]
    }

    #[test]
    fn separators_are_not_focusable() {
        assert_eq!(*focusable_indexes(&items()), vec![0, 3]);
    }

    #[test]
    fn submenus_are_focusable() {
        let items = vec![
            MenuItem::submenu("Export", |_| Vec::new()),
            MenuItem::submenu("Grouping", |_| Vec::new()),
        ];
        assert_eq!(*focusable_indexes(&items), vec![0, 1]);
    }

    #[test]
    fn keyboard_navigation_wraps_at_both_ends() {
        let focusable = focusable_indexes(&items());
        // Two focusable entries at indexes 0 and 3: down from the last wraps to
        // the first, and up from the first wraps to the last.
        assert_eq!(next_highlight(&focusable, None, "down"), Some(0));
        assert_eq!(next_highlight(&focusable, Some(0), "down"), Some(3));
        assert_eq!(next_highlight(&focusable, Some(3), "down"), Some(0));
        assert_eq!(next_highlight(&focusable, None, "up"), Some(3));
        assert_eq!(next_highlight(&focusable, Some(0), "up"), Some(3));
        assert_eq!(next_highlight(&focusable, Some(0), "home"), Some(0));
        assert_eq!(next_highlight(&focusable, Some(0), "end"), Some(3));
        assert_eq!(next_highlight(&focusable, Some(0), "tab"), None);
        assert_eq!(next_highlight(&[], None, "down"), None);
    }
}

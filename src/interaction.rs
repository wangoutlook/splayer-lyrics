use crate::render::PressTarget;

/// A click is resolved on release, never on press. `PlayPause` is resolved
/// against the live playback state at release time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickTarget {
    Prev,
    PlayPause,
    Next,
    Hide,
}

impl ClickTarget {
    fn from_target(target: PressTarget) -> Option<Self> {
        match target {
            PressTarget::Prev => Some(Self::Prev),
            PressTarget::PlayPause => Some(Self::PlayPause),
            PressTarget::Next => Some(Self::Next),
            PressTarget::Hide => Some(Self::Hide),
            PressTarget::Handle | PressTarget::Pad => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DragState {
    origin: PressTarget,
    start_x: i32,
    start_y: i32,
    active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragUpdate {
    Ignored,
    /// Hide-button drags are swallowed so they can still cancel the click
    /// without moving a hidden card.
    Blocked,
    Move {
        x: i32,
        y: i32,
    },
}

/// GTK's click and drag gestures do not arbitrate against each other, so the
/// application must. A press records the intended click; crossing GTK's drag
/// threshold cancels it and turns the gesture into a card move.
#[derive(Debug)]
pub struct PointerState {
    pending: Option<ClickTarget>,
    drag: Option<DragState>,
    threshold: f64,
}

impl Default for PointerState {
    fn default() -> Self {
        Self::new(8.0)
    }
}

impl PointerState {
    pub fn new(threshold: f64) -> Self {
        Self {
            pending: None,
            drag: None,
            threshold: threshold.max(1.0),
        }
    }

    /// GestureClick::pressed. Record only; do not act.
    pub fn pressed(&mut self, target: Option<PressTarget>) {
        self.pending = target.and_then(ClickTarget::from_target);
    }

    /// GestureDrag::drag_begin fires at press time. Arm the candidate without
    /// cancelling a potential click yet.
    pub fn drag_begin(&mut self, target: Option<PressTarget>, win_x: i32, win_y: i32) {
        self.drag = target.map(|origin| DragState {
            origin,
            start_x: win_x,
            start_y: win_y,
            active: false,
        });
    }

    /// GestureDrag::drag_update is the arbitration point. Match GTK's
    /// per-axis threshold rather than a radial distance.
    pub fn drag_update(&mut self, dx: f64, dy: f64) -> DragUpdate {
        let Some(drag) = self.drag.as_mut() else {
            return DragUpdate::Ignored;
        };

        if !drag.active {
            if dx.abs().max(dy.abs()) < self.threshold {
                return DragUpdate::Ignored;
            }
            drag.active = true;
            self.pending = None;
        }

        if drag.origin == PressTarget::Hide {
            return DragUpdate::Blocked;
        }

        DragUpdate::Move {
            x: drag.start_x + dx.round() as i32,
            // Widget Y grows downward; win_y is measured from the bottom.
            y: drag.start_y - dy.round() as i32,
        }
    }

    /// Returns whether a real move happened and should be persisted.
    pub fn drag_end(&mut self) -> bool {
        matches!(
            self.drag.take(),
            Some(drag) if drag.active && drag.origin != PressTarget::Hide
        )
    }

    /// GestureClick::released. Consume the pending click exactly once.
    pub fn release(&mut self) -> Option<ClickTarget> {
        let target = self.pending.take();
        if self.drag.as_ref().is_some_and(|drag| drag.active) {
            None
        } else {
            target
        }
    }

    /// GestureClick::stopped also fires on the double-click timeout for a
    /// stationary press, so only a real drag may cancel the click.
    pub fn stopped(&mut self) {
        if self.drag.as_ref().is_some_and(|drag| drag.active) {
            self.pending = None;
        }
    }

    pub fn is_moving(&self) -> bool {
        self.drag
            .as_ref()
            .is_some_and(|drag| drag.active && drag.origin != PressTarget::Hide)
    }

    /// True from press until release/cancel, even before the drag threshold.
    /// Width re-anchoring is deferred during this window so the card cannot
    /// shift under a held pointer.
    pub fn is_armed(&self) -> bool {
        self.drag.is_some()
    }

    pub fn cancel_click(&mut self) {
        self.pending = None;
    }

    pub fn cancel(&mut self) {
        self.pending = None;
        self.drag = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Default,
    Pointer,
    Grab,
    Grabbing,
}

impl CursorShape {
    pub fn name(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Pointer => Some("pointer"),
            Self::Grab => Some("grab"),
            Self::Grabbing => Some("grabbing"),
        }
    }
}

pub fn cursor_for(control: Option<usize>, handle: bool, hide: bool, moving: bool) -> CursorShape {
    if moving {
        CursorShape::Grabbing
    } else if handle {
        CursorShape::Grab
    } else if control.is_some() || hide {
        CursorShape::Pointer
    } else {
        CursorShape::Default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed(target: PressTarget) -> PointerState {
        // GTK's newest-first controller order runs drag_begin before pressed.
        let mut pointer = PointerState::new(8.0);
        pointer.drag_begin(Some(target), 1000, 60);
        pointer.pressed(Some(target));
        pointer
    }

    #[test]
    fn control_click_runs_once() {
        let mut pointer = armed(PressTarget::PlayPause);
        assert_eq!(pointer.drag_update(2.0, 2.0), DragUpdate::Ignored);
        assert!(!pointer.drag_end());
        assert_eq!(pointer.release(), Some(ClickTarget::PlayPause));
        assert_eq!(pointer.release(), None);
    }

    #[test]
    fn control_drag_moves_without_clicking() {
        let mut pointer = armed(PressTarget::PlayPause);
        assert_eq!(
            pointer.drag_update(9.0, 0.0),
            DragUpdate::Move { x: 1009, y: 60 }
        );
        assert!(pointer.drag_end());
        assert_eq!(pointer.release(), None);
    }

    #[test]
    fn handle_drag_moves_but_click_is_inert() {
        let mut pointer = armed(PressTarget::Handle);
        assert_eq!(
            pointer.drag_update(8.0, -5.0),
            DragUpdate::Move { x: 1008, y: 65 }
        );
        assert!(pointer.drag_end());
        assert_eq!(pointer.release(), None);
    }

    #[test]
    fn handle_click_is_inert() {
        let mut pointer = armed(PressTarget::Handle);
        assert!(!pointer.drag_end());
        assert_eq!(pointer.release(), None);
    }

    #[test]
    fn hide_click_hides_once() {
        let mut pointer = armed(PressTarget::Hide);
        assert!(!pointer.drag_end());
        assert_eq!(pointer.release(), Some(ClickTarget::Hide));
    }

    #[test]
    fn hide_drag_is_blocked_and_never_saves() {
        let mut pointer = armed(PressTarget::Hide);
        assert_eq!(pointer.drag_update(20.0, 0.0), DragUpdate::Blocked);
        assert!(!pointer.is_moving());
        assert!(!pointer.drag_end());
        assert_eq!(pointer.release(), None);
    }

    #[test]
    fn padding_drag_moves_without_click() {
        let mut pointer = armed(PressTarget::Pad);
        assert_eq!(
            pointer.drag_update(0.0, 10.0),
            DragUpdate::Move { x: 1000, y: 50 }
        );
        assert!(pointer.drag_end());
        assert_eq!(pointer.release(), None);
    }

    #[test]
    fn threshold_is_per_axis() {
        let mut pointer = armed(PressTarget::Handle);
        assert_eq!(pointer.drag_update(7.0, 7.0), DragUpdate::Ignored);
        assert!(matches!(
            pointer.drag_update(8.0, 0.0),
            DragUpdate::Move { .. }
        ));
    }

    #[test]
    fn stationary_stopped_keeps_pending_click() {
        let mut pointer = armed(PressTarget::Next);
        pointer.stopped();
        assert_eq!(pointer.release(), Some(ClickTarget::Next));
    }

    #[test]
    fn release_before_drag_end_is_safe() {
        let mut pointer = armed(PressTarget::Prev);
        let _ = pointer.drag_update(20.0, 0.0);
        assert_eq!(pointer.release(), None);
        assert!(pointer.drag_end());
    }

    #[test]
    fn new_press_overwrites_stale_drag() {
        let mut pointer = armed(PressTarget::Handle);
        let _ = pointer.drag_update(20.0, 0.0);
        pointer.drag_begin(Some(PressTarget::Next), 10, 20);
        pointer.pressed(Some(PressTarget::Next));
        assert_eq!(pointer.drag_update(1.0, 1.0), DragUpdate::Ignored);
        assert!(!pointer.drag_end());
        assert_eq!(pointer.release(), Some(ClickTarget::Next));
    }

    #[test]
    fn armed_covers_press_until_end_or_cancel() {
        let mut pointer = PointerState::new(8.0);
        assert!(!pointer.is_armed());
        pointer.drag_begin(Some(PressTarget::Handle), 10, 20);
        pointer.pressed(Some(PressTarget::Handle));
        assert!(pointer.is_armed());
        pointer.cancel_click();
        assert!(pointer.is_armed());
        assert!(!pointer.drag_end());
        assert!(!pointer.is_armed());

        pointer.drag_begin(Some(PressTarget::Handle), 10, 20);
        assert!(pointer.is_armed());
        pointer.cancel();
        assert!(!pointer.is_armed());
    }

    #[test]
    fn cancel_clears_everything() {
        let mut pointer = armed(PressTarget::PlayPause);
        pointer.cancel();
        assert_eq!(pointer.drag_update(20.0, 0.0), DragUpdate::Ignored);
        assert!(!pointer.drag_end());
        assert_eq!(pointer.release(), None);
    }

    #[test]
    fn cursor_precedence() {
        assert_eq!(cursor_for(Some(0), true, true, true), CursorShape::Grabbing);
        assert_eq!(cursor_for(Some(0), true, true, false), CursorShape::Grab);
        assert_eq!(
            cursor_for(Some(0), false, false, false),
            CursorShape::Pointer
        );
        assert_eq!(cursor_for(None, false, true, false), CursorShape::Pointer);
        assert_eq!(cursor_for(None, false, false, false), CursorShape::Default);
    }
}

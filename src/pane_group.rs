//! Recursive pane splitting, ported from Zed's `crates/workspace/src/pane_group.rs`
//! (rev 5e1fd39), with the collaboration decorators, zoom/maximize, dock-vs-center
//! distinction, and workspace serialization removed.
//!
//! The important structural property is that internal nodes are **n-ary**, not binary:
//! three side-by-side panes are one axis with three members, not nested pairs. That is
//! what keeps the tree shallow and makes resizing behave.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::{iter, mem};

use anyhow::Result;
use gpui::{
    Along, AnyElement, App, Axis, BorderStyle, Bounds, CursorStyle, Display, Element, ElementId,
    Entity, FlexDirection, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId,
    IntoElement, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Size,
    Style, StyleRefinement, Window, px, relative, size,
};
use parking_lot::Mutex;
use smallvec::SmallVec;
use theme::ActiveTheme;

use crate::dir_pane::DirPane;

/// Width of the draggable hit area straddling a divider.
pub const HANDLE_HITBOX_SIZE: f32 = 4.0;
/// Width of the painted divider line itself.
const DIVIDER_SIZE: f32 = 1.0;

const HORIZONTAL_MIN_SIZE: f32 = 80.;
const VERTICAL_MIN_SIZE: f32 = 100.;

const ACTIVE_PANE_BORDER_SIZE: f32 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDirection {
    Up,
    Down,
    Left,
    Right,
}

impl SplitDirection {
    pub fn axis(&self) -> Axis {
        match self {
            Self::Up | Self::Down => Axis::Vertical,
            Self::Left | Self::Right => Axis::Horizontal,
        }
    }

    /// Whether the new pane goes *after* the existing one along the axis.
    pub fn increasing(&self) -> bool {
        match self {
            Self::Left | Self::Up => false,
            Self::Down | Self::Right => true,
        }
    }
}

#[derive(Clone)]
pub struct PaneGroup {
    pub root: Member,
}

impl PaneGroup {
    pub fn new(pane: Entity<DirPane>) -> Self {
        Self {
            root: Member::Pane(pane),
        }
    }

    /// Split `old_pane`, inserting `new_pane` beside it. Infallible: if `old_pane` is
    /// somehow not in the tree we split the first pane instead, matching Zed.
    pub fn split(
        &mut self,
        old_pane: &Entity<DirPane>,
        new_pane: &Entity<DirPane>,
        direction: SplitDirection,
    ) {
        let found = match &mut self.root {
            Member::Pane(pane) => {
                if pane == old_pane {
                    self.root = Member::new_axis(old_pane.clone(), new_pane.clone(), direction);
                    true
                } else {
                    false
                }
            }
            Member::Axis(axis) => axis.split(old_pane, new_pane, direction),
        };

        if !found {
            let first_pane = self.root.first_pane();
            match &mut self.root {
                Member::Pane(_) => {
                    self.root = Member::new_axis(first_pane, new_pane.clone(), direction);
                }
                Member::Axis(axis) => {
                    axis.split(&first_pane, new_pane, direction);
                }
            }
        }
    }

    /// Remove a pane. Returns `Ok(false)` when this is the last pane in the window,
    /// which is refused rather than leaving an empty tree.
    pub fn remove(&mut self, pane: &Entity<DirPane>) -> Result<bool> {
        match &mut self.root {
            Member::Pane(_) => Ok(false),
            Member::Axis(axis) => {
                if let Some(last_member) = axis.remove(pane)? {
                    self.root = last_member;
                }
                Ok(true)
            }
        }
    }

    pub fn contains(&self, pane: &Entity<DirPane>) -> bool {
        self.root.contains(pane)
    }

    /// Structural dump for debugging, e.g. `H[P, P, P]` for three panes in one
    /// horizontal axis versus `H[P, V[P, P]]` for a nested split. Splitting along an
    /// axis that already runs that way must produce the former, never `H[P, H[P, P]]`.
    ///
    /// Gated to match its only caller, the split/close tracing in `workspace`.
    /// Without this it is dead code in a release build, which a debug build
    /// cannot tell you, so it only ever surfaced from `makepkg`.
    #[cfg(debug_assertions)]
    pub fn shape(&self) -> String {
        self.root.shape()
    }

    fn bounding_box_for_pane(&self, pane: &Entity<DirPane>) -> Option<Bounds<Pixels>> {
        match &self.root {
            Member::Pane(_) => None,
            Member::Axis(axis) => axis.bounding_box_for_pane(pane),
        }
    }

    fn pane_at_pixel_position(&self, coordinate: Point<Pixels>) -> Option<&Entity<DirPane>> {
        match &self.root {
            Member::Pane(pane) => Some(pane),
            Member::Axis(axis) => axis.pane_at_pixel_position(coordinate),
        }
    }

    /// Find the neighbouring pane in `direction` by stepping just past the active
    /// pane's own edge and hit-testing the bounding boxes cached during prepaint.
    ///
    /// Returns `None` before the first paint, because nothing has populated those
    /// boxes yet, a directional keybinding pressed at startup does nothing.
    pub fn find_pane_in_direction(
        &self,
        active_pane: &Entity<DirPane>,
        direction: SplitDirection,
    ) -> Option<&Entity<DirPane>> {
        let bounding_box = self.bounding_box_for_pane(active_pane)?;
        let center = bounding_box.center();
        let distance = px(HANDLE_HITBOX_SIZE);

        let target = match direction {
            SplitDirection::Left => Point::new(bounding_box.left() - distance, center.y),
            SplitDirection::Right => Point::new(bounding_box.right() + distance, center.y),
            SplitDirection::Up => Point::new(center.x, bounding_box.top() - distance),
            SplitDirection::Down => Point::new(center.x, bounding_box.bottom() + distance),
        };

        self.pane_at_pixel_position(target)
    }

    pub fn render(
        &self,
        active_pane: &Entity<DirPane>,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement + use<> {
        self.root.render(0, active_pane, window, cx).element
    }
}

#[derive(Clone)]
pub enum Member {
    Axis(PaneAxis),
    Pane(Entity<DirPane>),
}

pub struct MemberRenderResult {
    pub element: AnyElement,
    pub contains_active_pane: bool,
}

impl Member {
    fn new_axis(
        old_pane: Entity<DirPane>,
        new_pane: Entity<DirPane>,
        direction: SplitDirection,
    ) -> Self {
        let axis = direction.axis();
        let members = match direction {
            SplitDirection::Up | SplitDirection::Left => {
                vec![Member::Pane(new_pane), Member::Pane(old_pane)]
            }
            SplitDirection::Down | SplitDirection::Right => {
                vec![Member::Pane(old_pane), Member::Pane(new_pane)]
            }
        };

        Member::Axis(PaneAxis::new(axis, members))
    }

    fn contains(&self, needle: &Entity<DirPane>) -> bool {
        match self {
            Member::Axis(axis) => axis.members.iter().any(|member| member.contains(needle)),
            Member::Pane(pane) => pane == needle,
        }
    }

    #[cfg(debug_assertions)]
    fn shape(&self) -> String {
        match self {
            Member::Pane(_) => "P".to_string(),
            Member::Axis(axis) => {
                let label = match axis.axis {
                    Axis::Horizontal => 'H',
                    Axis::Vertical => 'V',
                };
                let children: Vec<String> = axis.members.iter().map(Member::shape).collect();
                let flexes: Vec<String> = axis
                    .flexes
                    .lock()
                    .iter()
                    .map(|f| format!("{f:.2}"))
                    .collect();
                format!("{label}[{}]<{}>", children.join(", "), flexes.join(" "))
            }
        }
    }

    fn first_pane(&self) -> Entity<DirPane> {
        match self {
            Member::Axis(axis) => axis.members[0].first_pane(),
            Member::Pane(pane) => pane.clone(),
        }
    }

    fn render(
        &self,
        basis: usize,
        active_pane: &Entity<DirPane>,
        window: &mut Window,
        cx: &mut App,
    ) -> MemberRenderResult {
        match self {
            Member::Pane(pane) => {
                // `cached` recycles the subtree unless the pane called `cx.notify()`.
                // Without it every pane re-renders on every frame.
                let mut style = StyleRefinement {
                    display: Some(Display::Flex),
                    flex_direction: Some(FlexDirection::Column),
                    ..Default::default()
                };
                style.size.width = Some(relative(1.).into());
                style.size.height = Some(relative(1.).into());

                MemberRenderResult {
                    element: pane.clone().cached(style).into_any_element(),
                    contains_active_pane: pane == active_pane,
                }
            }
            Member::Axis(axis) => axis.render(basis, active_pane, window, cx),
        }
    }
}

#[derive(Clone)]
pub struct PaneAxis {
    pub axis: Axis,
    pub members: Vec<Member>,
    /// One flex per child, with the invariant `sum(flexes) == flexes.len()`. A child's
    /// extent along the axis is `container.along(axis) * (flexes[ix] / flexes.len())`.
    ///
    /// Shared via `Arc<Mutex<_>>` with the layout element, which is rebuilt every frame
    /// and mutates these from its mouse handlers.
    pub flexes: Arc<Mutex<Vec<f32>>>,
    /// Written during prepaint, read by hit-testing and directional navigation.
    pub bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
}

impl PaneAxis {
    pub fn new(axis: Axis, members: Vec<Member>) -> Self {
        let flexes = Arc::new(Mutex::new(vec![1.; members.len()]));
        let bounding_boxes = Arc::new(Mutex::new(vec![None; members.len()]));
        Self {
            axis,
            members,
            flexes,
            bounding_boxes,
        }
    }

    fn split(
        &mut self,
        old_pane: &Entity<DirPane>,
        new_pane: &Entity<DirPane>,
        direction: SplitDirection,
    ) -> bool {
        for (mut idx, member) in self.members.iter_mut().enumerate() {
            match member {
                Member::Axis(axis) => {
                    if axis.split(old_pane, new_pane, direction) {
                        return true;
                    }
                }
                Member::Pane(pane) => {
                    if pane == old_pane {
                        if direction.axis() == self.axis {
                            // Parent already runs this way: insert a sibling rather than
                            // nesting. This is why `H[A, H[B, C]]` never arises.
                            let split_ix = idx;
                            if direction.increasing() {
                                idx += 1;
                            }
                            self.insert_pane(idx, split_ix, new_pane);
                        } else {
                            *member =
                                Member::new_axis(old_pane.clone(), new_pane.clone(), direction);
                        }
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Insert `new_pane` at `idx`, splitting the space previously held by the pane at
    /// `split_ix` between the two halves.
    ///
    /// Deliberate deviation from Zed, which resets the whole axis to `vec![1.0; n]` here
    /// and so discards every hand-tuned ratio in the axis. Sibling proportions are
    /// preserved; only the pane actually being split changes size.
    fn insert_pane(&mut self, idx: usize, split_ix: usize, new_pane: &Entity<DirPane>) {
        let mut flexes = self.flexes.lock();

        let n = flexes.len() as f32;
        let source_flex = flexes[split_ix];
        // Every child's share of the container is flex/n. After insertion the
        // denominator is n+1, so scale everything to keep the invariant sum == len.
        let scale = (n + 1.) / n;

        flexes[split_ix] = source_flex / 2.;
        flexes.insert(idx, source_flex / 2.);
        for flex in flexes.iter_mut() {
            *flex *= scale;
        }
        drop(flexes);

        self.members.insert(idx, Member::Pane(new_pane.clone()));
        debug_assert!(flex_values_in_bounds(self.flexes.lock().as_slice()));
    }

    /// Remove a pane from this subtree.
    ///
    /// `Ok(Some(member))` means "I am down to one child: splice it into my slot and
    /// delete me." Recursion rewrites `*member` on the way back up, so chains collapse.
    fn remove(&mut self, pane_to_remove: &Entity<DirPane>) -> Result<Option<Member>> {
        let mut found_pane = false;
        let mut remove_member = None;

        for (idx, member) in self.members.iter_mut().enumerate() {
            match member {
                Member::Axis(axis) => {
                    if let Ok(last_member) = axis.remove(pane_to_remove) {
                        if let Some(last_member) = last_member {
                            *member = last_member;
                        }
                        found_pane = true;
                        break;
                    }
                }
                Member::Pane(pane) => {
                    if pane == pane_to_remove {
                        found_pane = true;
                        remove_member = Some(idx);
                        break;
                    }
                }
            }
        }

        anyhow::ensure!(found_pane, "pane not found");

        if let Some(idx) = remove_member {
            self.members.remove(idx);
            let mut flexes = self.flexes.lock();
            let removed = flexes.remove(idx);
            // Redistribute the removed pane's share proportionally across the rest,
            // rather than flattening every sibling back to equal.
            let remaining: f32 = flexes.iter().sum();
            if remaining > 0. {
                let scale = (flexes.len() as f32) / remaining;
                for flex in flexes.iter_mut() {
                    *flex *= scale;
                }
            } else {
                flexes.fill(1.);
            }
            let _ = removed;
        }

        if self.members.len() == 1 {
            let result = self.members.pop();
            *self.flexes.lock() = vec![1.; self.members.len()];
            Ok(result)
        } else {
            debug_assert!(flex_values_in_bounds(self.flexes.lock().as_slice()));
            Ok(None)
        }
    }

    fn bounding_box_for_pane(&self, pane: &Entity<DirPane>) -> Option<Bounds<Pixels>> {
        for (idx, member) in self.members.iter().enumerate() {
            match member {
                Member::Pane(found) => {
                    if pane == found {
                        return self.bounding_boxes.lock().get(idx).copied().flatten();
                    }
                }
                Member::Axis(axis) => {
                    if let Some(rect) = axis.bounding_box_for_pane(pane) {
                        return Some(rect);
                    }
                }
            }
        }
        None
    }

    fn pane_at_pixel_position(&self, coordinate: Point<Pixels>) -> Option<&Entity<DirPane>> {
        // Copied out so the lock is not held while recursing into a child axis.
        let bounding_boxes = self.bounding_boxes.lock().clone();

        for (idx, member) in self.members.iter().enumerate() {
            if let Some(bounds) = bounding_boxes.get(idx).copied().flatten()
                && bounds.contains(&coordinate)
            {
                return match member {
                    Member::Pane(found) => Some(found),
                    Member::Axis(axis) => axis.pane_at_pixel_position(coordinate),
                };
            }
        }
        None
    }

    fn render(
        &self,
        basis: usize,
        active_pane: &Entity<DirPane>,
        window: &mut Window,
        cx: &mut App,
    ) -> MemberRenderResult {
        let mut active_pane_ix = None;
        let mut contains_active_pane = false;
        let mut is_leaf_pane = Vec::with_capacity(self.members.len());
        let mut rendered_children = Vec::with_capacity(self.members.len());

        for (ix, member) in self.members.iter().enumerate() {
            is_leaf_pane.push(matches!(member, Member::Pane(_)));

            let result = member.render((basis + ix) * 10, active_pane, window, cx);
            if result.contains_active_pane {
                contains_active_pane = true;
                if matches!(member, Member::Pane(_)) {
                    active_pane_ix = Some(ix);
                }
            }
            rendered_children.push(result.element);
        }

        let element = pane_axis(
            self.axis,
            basis,
            self.flexes.clone(),
            self.bounding_boxes.clone(),
        )
        .with_is_leaf_pane_mask(is_leaf_pane)
        .with_active_pane(active_pane_ix)
        .children(rendered_children)
        .into_any_element();

        MemberRenderResult {
            element,
            contains_active_pane,
        }
    }
}

fn flex_values_in_bounds(flexes: &[f32]) -> bool {
    (flexes.iter().copied().sum::<f32>() - flexes.len() as f32).abs() < 0.001
}

// ---------------------------------------------------------------------------
// The layout element.
//
// Not a taffy flex container, it does its own arithmetic so that flexes, divider
// hitboxes, and the cached bounding boxes all stay consistent.
// ---------------------------------------------------------------------------

fn pane_axis(
    axis: Axis,
    basis: usize,
    flexes: Arc<Mutex<Vec<f32>>>,
    bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
) -> PaneAxisElement {
    PaneAxisElement {
        axis,
        basis,
        flexes,
        bounding_boxes,
        children: SmallVec::new(),
        active_pane_ix: None,
        is_leaf_pane_mask: Vec::new(),
    }
}

pub struct PaneAxisElement {
    axis: Axis,
    basis: usize,
    flexes: Arc<Mutex<Vec<f32>>>,
    bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
    children: SmallVec<[AnyElement; 2]>,
    active_pane_ix: Option<usize>,
    is_leaf_pane_mask: Vec<bool>,
}

pub struct PaneAxisLayout {
    dragged_handle: Rc<RefCell<Option<usize>>>,
    children: Vec<PaneAxisChildLayout>,
}

struct PaneAxisChildLayout {
    bounds: Bounds<Pixels>,
    element: AnyElement,
    handle: Option<PaneAxisHandleLayout>,
    is_leaf_pane: bool,
}

struct PaneAxisHandleLayout {
    hitbox: Hitbox,
    divider_bounds: Bounds<Pixels>,
}

impl PaneAxisElement {
    fn with_active_pane(mut self, active_pane_ix: Option<usize>) -> Self {
        self.active_pane_ix = active_pane_ix;
        self
    }

    fn with_is_leaf_pane_mask(mut self, mask: Vec<bool>) -> Self {
        self.is_leaf_pane_mask = mask;
        self
    }

    /// Drag-to-resize. Walks a list of "flex successors" from the dragged divider,
    /// draining a bucket of pixel change and clamping each pane to its minimum, moving
    /// flex between neighbours so the sum invariant holds.
    #[allow(clippy::too_many_arguments)] // ported verbatim from Zed; kept comparable
    fn compute_resize(
        flexes: &Arc<Mutex<Vec<f32>>>,
        e: &MouseMoveEvent,
        ix: usize,
        axis: Axis,
        child_start: Point<Pixels>,
        container_size: Size<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let min_size = match axis {
            Axis::Horizontal => px(HORIZONTAL_MIN_SIZE),
            Axis::Vertical => px(VERTICAL_MIN_SIZE),
        };
        let mut flexes = flexes.lock();
        debug_assert!(flex_values_in_bounds(flexes.as_slice()));

        let size = move |ix, flexes: &[f32]| {
            container_size.along(axis) * (flexes[ix] / flexes.len() as f32)
        };

        if min_size - px(1.) > size(ix, flexes.as_slice()) {
            return;
        }

        let mut proposed_current_pixel_change =
            (e.position - child_start).along(axis) - size(ix, flexes.as_slice());

        let flex_changes = |pixel_dx, target_ix, next: isize, flexes: &[f32]| {
            let flex_change = pixel_dx / container_size.along(axis);
            let current_target_flex = flexes[target_ix] + flex_change;
            let next_target_flex = flexes[(target_ix as isize + next) as usize] - flex_change;
            (current_target_flex, next_target_flex)
        };

        let mut successors = iter::from_fn({
            let forward = proposed_current_pixel_change > px(0.);
            let mut ix_offset = 0;
            let len = flexes.len();
            move || {
                let result = if forward {
                    (ix + 1 + ix_offset < len).then(|| ix + ix_offset)
                } else {
                    (ix as isize - ix_offset as isize >= 0).then(|| ix - ix_offset)
                };
                ix_offset += 1;
                result
            }
        });

        while proposed_current_pixel_change.abs() > px(0.) {
            let Some(current_ix) = successors.next() else {
                break;
            };

            let next_target_size = Pixels::max(
                size(current_ix + 1, flexes.as_slice()) - proposed_current_pixel_change,
                min_size,
            );
            let current_target_size = Pixels::max(
                size(current_ix, flexes.as_slice()) + size(current_ix + 1, flexes.as_slice())
                    - next_target_size,
                min_size,
            );

            let current_pixel_change = current_target_size - size(current_ix, flexes.as_slice());
            let (current_target_flex, next_target_flex) =
                flex_changes(current_pixel_change, current_ix, 1, flexes.as_slice());

            flexes[current_ix] = current_target_flex;
            flexes[current_ix + 1] = next_target_flex;

            proposed_current_pixel_change -= current_pixel_change;
        }

        cx.stop_propagation();
        window.refresh();
    }

    /// A 1px painted divider at the boundary plus a 4px hitbox centred on it.
    fn layout_handle(
        axis: Axis,
        pane_bounds: Bounds<Pixels>,
        window: &mut Window,
    ) -> PaneAxisHandleLayout {
        let handle_bounds = Bounds {
            origin: pane_bounds.origin.apply_along(axis, |origin| {
                origin + pane_bounds.size.along(axis) - px(HANDLE_HITBOX_SIZE / 2.)
            }),
            size: pane_bounds
                .size
                .apply_along(axis, |_| px(HANDLE_HITBOX_SIZE)),
        };
        let divider_bounds = Bounds {
            origin: pane_bounds
                .origin
                .apply_along(axis, |origin| origin + pane_bounds.size.along(axis)),
            size: pane_bounds.size.apply_along(axis, |_| px(DIVIDER_SIZE)),
        };

        PaneAxisHandleLayout {
            hitbox: window.insert_hitbox(handle_bounds, HitboxBehavior::BlockMouse),
            divider_bounds,
        }
    }
}

impl IntoElement for PaneAxisElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl ParentElement for PaneAxisElement {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl Element for PaneAxisElement {
    type RequestLayoutState = ();
    type PrepaintState = PaneAxisLayout;

    fn id(&self) -> Option<ElementId> {
        Some(self.basis.into())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let style = Style {
            flex_grow: 1.,
            flex_shrink: 1.,
            flex_basis: relative(0.).into(),
            size: size(relative(1.).into(), relative(1.).into()),
            ..Style::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> PaneAxisLayout {
        let dragged_handle = window.with_element_state::<Rc<RefCell<Option<usize>>>, _>(
            global_id.unwrap(),
            |state, _cx| {
                let state = state.unwrap_or_else(|| Rc::new(RefCell::new(None)));
                (state.clone(), state)
            },
        );

        let flexes = self.flexes.lock().clone();
        let len = self.children.len();
        debug_assert!(flexes.len() == len);

        let total_flex = len as f32;
        let mut origin = bounds.origin;
        let space_per_flex = bounds.size.along(self.axis) / total_flex;

        let mut bounding_boxes = self.bounding_boxes.lock();
        bounding_boxes.clear();

        let mut layout = PaneAxisLayout {
            dragged_handle,
            children: Vec::new(),
        };

        for (ix, mut child) in mem::take(&mut self.children).into_iter().enumerate() {
            let child_flex = flexes[ix];
            let child_size = bounds
                .size
                .apply_along(self.axis, |_| space_per_flex * child_flex)
                .map(|d| d.round());
            let child_bounds = Bounds {
                origin,
                size: child_size,
            };

            bounding_boxes.push(Some(child_bounds));
            child.layout_as_root(child_size.into(), window, cx);
            child.prepaint_at(origin, window, cx);

            origin = origin.apply_along(self.axis, |val| val + child_size.along(self.axis));

            layout.children.push(PaneAxisChildLayout {
                bounds: child_bounds,
                element: child,
                handle: None,
                is_leaf_pane: self.is_leaf_pane_mask.get(ix).copied().unwrap_or(true),
            });
        }
        drop(bounding_boxes);

        for (ix, child_layout) in layout.children.iter_mut().enumerate() {
            if ix < len - 1 {
                child_layout.handle =
                    Some(Self::layout_handle(self.axis, child_layout.bounds, window));
            }
        }

        layout
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for child in &mut layout.children {
            child.element.paint(window, cx);
        }

        for (ix, child) in layout.children.iter_mut().enumerate() {
            // Inset by 1px horizontally so the overlay does not cover the divider.
            let overlay_bounds = Bounds {
                origin: child
                    .bounds
                    .origin
                    .apply_along(Axis::Horizontal, |val| val + px(1.)),
                size: child
                    .bounds
                    .size
                    .apply_along(Axis::Horizontal, |val| val - px(1.)),
            };

            if child.is_leaf_pane && self.active_pane_ix == Some(ix) {
                window.paint_quad(gpui::quad(
                    overlay_bounds,
                    0.,
                    gpui::transparent_black(),
                    ACTIVE_PANE_BORDER_SIZE,
                    cx.theme().colors().border_selected,
                    BorderStyle::Solid,
                ));
            }

            let Some(handle) = child.handle.as_mut() else {
                continue;
            };

            let cursor_style = match self.axis {
                Axis::Vertical => CursorStyle::ResizeRow,
                Axis::Horizontal => CursorStyle::ResizeColumn,
            };

            if layout
                .dragged_handle
                .borrow()
                .is_some_and(|dragged_ix| dragged_ix == ix)
            {
                // Sticky while dragging, so leaving the hitbox does not reset it.
                window.set_window_cursor_style(cursor_style);
            } else {
                window.set_cursor_style(cursor_style, &handle.hitbox);
            }

            window.paint_quad(gpui::fill(
                handle.divider_bounds,
                cx.theme().colors().pane_group_border,
            ));

            window.on_mouse_event({
                let dragged_handle = layout.dragged_handle.clone();
                let flexes = self.flexes.clone();
                let handle_hitbox = handle.hitbox.clone();
                move |e: &MouseDownEvent, phase, window, cx| {
                    if phase.bubble() && handle_hitbox.is_hovered(window) {
                        dragged_handle.replace(Some(ix));
                        if e.click_count >= 2 {
                            // Double-click resets this axis to equal sizes.
                            let mut borrow = flexes.lock();
                            *borrow = vec![1.; borrow.len()];
                            window.refresh();
                        }
                        cx.stop_propagation();
                    }
                }
            });

            window.on_mouse_event({
                let dragged_handle = layout.dragged_handle.clone();
                let flexes = self.flexes.clone();
                let child_bounds = child.bounds;
                let axis = self.axis;
                move |e: &MouseMoveEvent, phase, window, cx| {
                    let dragged_handle = dragged_handle.borrow();
                    if phase.bubble() && *dragged_handle == Some(ix) {
                        Self::compute_resize(
                            &flexes,
                            e,
                            ix,
                            axis,
                            child_bounds.origin,
                            bounds.size,
                            window,
                            cx,
                        )
                    }
                }
            });
        }

        window.on_mouse_event({
            let dragged_handle = layout.dragged_handle.clone();
            move |_: &MouseUpEvent, phase, _window, _cx| {
                if phase.bubble() {
                    dragged_handle.replace(None);
                }
            }
        });
    }
}

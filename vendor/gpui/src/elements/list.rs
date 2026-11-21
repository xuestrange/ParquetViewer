//! A list element that can be used to render a large number of differently sized elements
//! efficiently. Clients of this API need to ensure that elements outside of the scrolled
//! area do not change their height for this element to function correctly. If your elements
//! do change height, notify the list element via [`ListState::splice`] or [`ListState::reset`].
//! In order to minimize re-renders, this element's state is stored intrusively
//! on your own views, so that your code can coordinate directly with the list element's cached state.
//!
//! If all of your elements are the same height, see [`crate::UniformList`] for a simpler API

use crate::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, DispatchPhase, Edges, Element, EntityId,
    FocusHandle, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement,
    Overflow, Pixels, Point, ScrollDelta, ScrollWheelEvent, Size, Style, StyleRefinement, Styled,
    Window, point, px, size,
};
use collections::VecDeque;
use refineable::Refineable as _;
use std::{cell::RefCell, ops::Range, rc::Rc};
use sum_tree::{Bias, Dimensions, SumTree};

type RenderItemFn = dyn FnMut(usize, &mut Window, &mut App) -> AnyElement + 'static;

/// Construct a new list element
pub fn list(
    state: ListState,
    render_item: impl FnMut(usize, &mut Window, &mut App) -> AnyElement + 'static,
) -> List {
    List {
        state,
        render_item: Box::new(render_item),
        style: StyleRefinement::default(),
        sizing_behavior: ListSizingBehavior::default(),
    }
}

/// A list element
pub struct List {
    state: ListState,
    render_item: Box<RenderItemFn>,
    style: StyleRefinement,
    sizing_behavior: ListSizingBehavior,
}

impl List {
    /// Set the sizing behavior for the list.
    pub fn with_sizing_behavior(mut self, behavior: ListSizingBehavior) -> Self {
        self.sizing_behavior = behavior;
        self
    }
}

/// The list state that views must hold on behalf of the list element.
#[derive(Clone)]
pub struct ListState(Rc<RefCell<StateInner>>);

impl std::fmt::Debug for ListState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ListState")
    }
}

struct StateInner {
    last_layout_bounds: Option<Bounds<Pixels>>,
    last_padding: Option<Edges<Pixels>>,
    items: SumTree<ListItem>,
    logical_scroll_top: Option<ListOffset>,
    alignment: ListAlignment,
    overdraw: Pixels,
    reset: bool,
    #[allow(clippy::type_complexity)]
    scroll_handler: Option<Box<dyn FnMut(&ListScrollEvent, &mut Window, &mut App)>>,
    scrollbar_drag_start_height: Option<Pixels>,
    measuring_behavior: ListMeasuringBehavior,
    estimated_item_height: Pixels,
    fixed: Option<FixedHeightItems>,
}

/// An opt-in uniform-height list needs no entry for an off-screen item. The
/// existing measured tree remains available for lists whose heights can vary.
#[derive(Clone)]
struct FixedHeightItems {
    count: usize,
    height: Pixels,
    rendered: Range<usize>,
    rendered_top: ListOffset,
}

impl FixedHeightItems {
    fn content_height(&self) -> Pixels {
        px((self.count as f64 * f64::from(f32::from(self.height))).min(f64::from(f32::MAX)) as f32)
    }

    fn pixel_offset(&self, offset: ListOffset) -> Pixels {
        let offset = self.shift(offset, px(0.));
        px((offset.item_ix as f64 * f64::from(f32::from(self.height))
            + f64::from(f32::from(offset.offset_in_item)))
        .min(f64::from(f32::MAX)) as f32)
    }

    fn offset_at_pixel(&self, offset: Pixels) -> ListOffset {
        self.shift(ListOffset::default(), offset)
    }

    /// Keep the integer row anchor while moving by a small delta. Converting
    /// the full position to f32 pixels first loses individual rows far down a
    /// very large table, even though wheel deltas are small and precise.
    fn shift(&self, offset: ListOffset, distance: Pixels) -> ListOffset {
        if self.count == 0 {
            return ListOffset::default();
        }
        let height = f64::from(f32::from(self.height));
        let local = f64::from(f32::from(offset.offset_in_item)) + f64::from(f32::from(distance));
        if !local.is_finite() {
            return ListOffset {
                item_ix: offset.item_ix.min(self.count),
                offset_in_item: px(0.),
            };
        }
        let delta = (local / height).floor() as i128;
        let index = (offset.item_ix.min(self.count) as i128).saturating_add(delta);
        if index < 0 {
            return ListOffset::default();
        }
        if index >= self.count as i128 {
            return ListOffset {
                item_ix: self.count,
                offset_in_item: px(0.),
            };
        }
        ListOffset {
            item_ix: index as usize,
            offset_in_item: px(local.rem_euclid(height) as f32),
        }
    }

    fn clamp_to_viewport(&self, offset: ListOffset, viewport_height: Pixels) -> ListOffset {
        let offset = self.shift(offset, px(0.));
        let remaining = (self.count - offset.item_ix) as f64 * f64::from(f32::from(self.height))
            - f64::from(f32::from(offset.offset_in_item));
        let viewport = f64::from(f32::from(viewport_height.max(px(0.))));
        if remaining < viewport {
            self.shift(offset, px((remaining - viewport) as f32))
        } else {
            offset
        }
    }

    fn visible_range(&self, offset: ListOffset, height: Pixels) -> Range<usize> {
        let offset = self.shift(offset, px(0.));
        let visible = ((f64::from(f32::from(height.max(px(0.))))
            + f64::from(f32::from(offset.offset_in_item)))
            / f64::from(f32::from(self.height)))
        .ceil() as usize;
        offset.item_ix..offset.item_ix.saturating_add(visible).min(self.count)
    }
}

/// Whether the list is scrolling from top to bottom or bottom to top.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListAlignment {
    /// The list is scrolling from top to bottom, like most lists.
    Top,
    /// The list is scrolling from bottom to top, like a chat log.
    Bottom,
}

/// A scroll event that has been converted to be in terms of the list's items.
pub struct ListScrollEvent {
    /// The range of items currently visible in the list, after applying the scroll event.
    pub visible_range: Range<usize>,

    /// The number of items that are currently visible in the list, after applying the scroll event.
    pub count: usize,

    /// Whether the list has been scrolled.
    pub is_scrolled: bool,
}

/// The sizing behavior to apply during layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ListSizingBehavior {
    /// The list should calculate its size based on the size of its items.
    Infer,
    /// The list should not calculate a fixed size.
    #[default]
    Auto,
}

/// The measuring behavior to apply during layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ListMeasuringBehavior {
    /// Measure all items in the list.
    /// Note: This can be expensive for the first frame in a large list.
    Measure(bool),
    /// Only measure visible items
    #[default]
    Visible,
}

impl ListMeasuringBehavior {
    fn reset(&mut self) {
        match self {
            ListMeasuringBehavior::Measure(has_measured) => *has_measured = false,
            ListMeasuringBehavior::Visible => {}
        }
    }
}

/// The horizontal sizing behavior to apply during layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ListHorizontalSizingBehavior {
    /// List items' width can never exceed the width of the list.
    #[default]
    FitList,
    /// List items' width may go over the width of the list, if any item is wider.
    Unconstrained,
}

struct LayoutItemsResponse {
    max_item_width: Pixels,
    scroll_top: ListOffset,
    item_layouts: VecDeque<ItemLayout>,
}

struct ItemLayout {
    index: usize,
    element: AnyElement,
    size: Size<Pixels>,
}

/// Frame state used by the [List] element after layout.
pub struct ListPrepaintState {
    hitbox: Hitbox,
    layout: LayoutItemsResponse,
}

#[derive(Clone)]
enum ListItem {
    Unmeasured {
        focus_handle: Option<FocusHandle>,
        estimated_height: Pixels,
    },
    Measured {
        size: Size<Pixels>,
        focus_handle: Option<FocusHandle>,
    },
}

impl ListItem {
    fn size(&self) -> Option<Size<Pixels>> {
        if let ListItem::Measured { size, .. } = self {
            Some(*size)
        } else {
            None
        }
    }

    fn focus_handle(&self) -> Option<FocusHandle> {
        match self {
            ListItem::Unmeasured { focus_handle, .. } | ListItem::Measured { focus_handle, .. } => {
                focus_handle.clone()
            }
        }
    }

    fn contains_focused(&self, window: &Window, cx: &App) -> bool {
        match self {
            ListItem::Unmeasured { focus_handle, .. } | ListItem::Measured { focus_handle, .. } => {
                focus_handle
                    .as_ref()
                    .is_some_and(|handle| handle.contains_focused(window, cx))
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ListItemSummary {
    count: usize,
    rendered_count: usize,
    unrendered_count: usize,
    height: Pixels,
    has_focus_handles: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Count(usize);

#[derive(Clone, Debug, Default)]
struct Height(Pixels);

impl ListState {
    /// Construct a new list state, for storage on a view.
    ///
    /// The overdraw parameter controls how much extra space is rendered
    /// above and below the visible area. Elements within this area will
    /// be measured even though they are not visible. This can help ensure
    /// that the list doesn't flicker or pop in when scrolling.
    pub fn new(item_count: usize, alignment: ListAlignment, overdraw: Pixels) -> Self {
        Self::new_with_estimated_item_height(item_count, alignment, overdraw, px(0.))
    }

    /// Construct a lazily measured list with an initial height estimate for
    /// unmeasured items. This allows scrollbars and distant item navigation to
    /// account for items that have not entered the viewport yet. Visible items
    /// replace the estimate with their measured height. Estimates must be finite
    /// and nonnegative; invalid values are treated as zero. `new` retains its
    /// original zero-height behavior.
    pub fn new_with_estimated_item_height(
        item_count: usize,
        alignment: ListAlignment,
        overdraw: Pixels,
        estimated_item_height: Pixels,
    ) -> Self {
        let estimated_item_height = if f32::from(estimated_item_height).is_finite() {
            estimated_item_height.max(px(0.))
        } else {
            px(0.)
        };
        let this = Self(Rc::new(RefCell::new(StateInner {
            last_layout_bounds: None,
            last_padding: None,
            items: SumTree::default(),
            logical_scroll_top: None,
            alignment,
            overdraw,
            scroll_handler: None,
            reset: false,
            scrollbar_drag_start_height: None,
            measuring_behavior: ListMeasuringBehavior::default(),
            estimated_item_height,
            fixed: None,
        })));
        this.splice(0..0, item_count);
        this
    }

    /// Construct a list whose items have an explicitly fixed height. This
    /// stores only the item count and current viewport, so initialization and
    /// resets do not allocate metadata for every item. Use the estimated-height
    /// constructor instead when items can wrap or otherwise change height.
    /// Invalid heights fall back to the estimated-height behavior.
    pub fn new_with_fixed_item_height(
        item_count: usize,
        alignment: ListAlignment,
        overdraw: Pixels,
        item_height: Pixels,
    ) -> Self {
        let this = Self::new_with_estimated_item_height(0, alignment, overdraw, item_height);
        this.set_fixed_item_height(Some(item_height));
        this.reset(item_count);
        this
    }

    /// Enable or disable uniform-height storage while preserving the logical
    /// scroll position. Focusable items require the measured-tree path, so
    /// enabling this mode is refused if any registered focus handles exist.
    /// Returns the effective fixed height after applying the change.
    pub fn set_fixed_item_height(&self, height: Option<Pixels>) -> Option<Pixels> {
        let state = &mut *self.0.borrow_mut();
        let height =
            height.filter(|height| f32::from(*height).is_finite() && *height > Pixels::ZERO);
        if state.fixed.as_ref().map(|fixed| fixed.height) == height {
            return height;
        }
        if let Some(height) = height {
            if state.items.summary().has_focus_handles {
                return None;
            }
            let count = state.item_count();
            state.fixed = Some(FixedHeightItems {
                count,
                height,
                rendered: 0..0,
                rendered_top: ListOffset::default(),
            });
            state.items = SumTree::default();
            state.estimated_item_height = height;
        } else {
            state.materialize_fixed_items();
        }
        state.reset = true;
        state.measuring_behavior.reset();
        state.scrollbar_drag_start_height = None;
        state.fixed.as_ref().map(|fixed| fixed.height)
    }

    /// The explicit uniform item height, or `None` for a measured list.
    pub fn fixed_item_height(&self) -> Option<Pixels> {
        self.0.borrow().fixed.as_ref().map(|fixed| fixed.height)
    }

    /// Set the list to measure all items in the list in the first layout phase.
    ///
    /// This is useful for ensuring that the scrollbar size is correct instead of based on only rendered elements.
    pub fn measure_all(self) -> Self {
        self.0.borrow_mut().measuring_behavior = ListMeasuringBehavior::Measure(false);
        self
    }

    /// Reset this instantiation of the list state.
    ///
    /// Note that this will cause scroll events to be dropped until the next paint.
    pub fn reset(&self, element_count: usize) {
        let old_count = {
            let state = &mut *self.0.borrow_mut();
            state.reset = true;
            state.measuring_behavior.reset();
            state.logical_scroll_top = None;
            state.scrollbar_drag_start_height = None;
            if let Some(fixed) = state.fixed.as_mut() {
                fixed.count = element_count;
                fixed.rendered = 0..0;
                return;
            }
            state.item_count()
        };

        self.splice(0..old_count, element_count);
    }

    /// The number of items in this list.
    pub fn item_count(&self) -> usize {
        self.0.borrow().item_count()
    }

    /// Inform the list state that the items in `old_range` have been replaced
    /// by `count` new items that must be recalculated.
    pub fn splice(&self, old_range: Range<usize>, count: usize) {
        {
            let state = &mut *self.0.borrow_mut();
            if let Some(fixed) = state.fixed.as_mut() {
                let start = old_range.start.min(fixed.count);
                let end = old_range.end.max(start).min(fixed.count);
                fixed.count = fixed
                    .count
                    .saturating_sub(end - start)
                    .saturating_add(count);
                fixed.rendered = 0..0;
                state.adjust_scroll_for_splice(start..end, count);
                return;
            }
        }
        self.splice_focusable(old_range, (0..count).map(|_| None))
    }

    /// Register with the list state that the items in `old_range` have been replaced
    /// by new items. As opposed to [`Self::splice`], this method allows an iterator of optional focus handles
    /// to be supplied to properly integrate with items in the list that can be focused. If a focused item
    /// is scrolled out of view, the list will continue to render it to allow keyboard interaction.
    pub fn splice_focusable(
        &self,
        old_range: Range<usize>,
        focus_handles: impl IntoIterator<Item = Option<FocusHandle>>,
    ) {
        let state = &mut *self.0.borrow_mut();
        // Off-screen focus retention needs the measured tree. A caller using
        // the focus-aware API explicitly opts back into its full semantics.
        state.materialize_fixed_items();

        let mut old_items = state.items.cursor::<Count>(());
        let mut new_items = old_items.slice(&Count(old_range.start), Bias::Right);
        old_items.seek_forward(&Count(old_range.end), Bias::Right);

        let mut spliced_count = 0;
        let estimated_height = state.estimated_item_height;
        new_items.extend(
            focus_handles.into_iter().map(|focus_handle| {
                spliced_count += 1;
                ListItem::Unmeasured {
                    focus_handle,
                    estimated_height,
                }
            }),
            (),
        );
        new_items.append(old_items.suffix(), ());
        drop(old_items);
        state.items = new_items;

        state.adjust_scroll_for_splice(old_range, spliced_count);
    }

    /// Set a handler that will be called when the list is scrolled.
    pub fn set_scroll_handler(
        &self,
        handler: impl FnMut(&ListScrollEvent, &mut Window, &mut App) + 'static,
    ) {
        self.0.borrow_mut().scroll_handler = Some(Box::new(handler))
    }

    /// Get the current scroll offset, in terms of the list's items.
    pub fn logical_scroll_top(&self) -> ListOffset {
        self.0.borrow().logical_scroll_top()
    }

    /// Scroll the list by the given offset
    pub fn scroll_by(&self, distance: Pixels) {
        if distance == px(0.) {
            return;
        }

        let current_offset = self.logical_scroll_top();
        let state = &mut *self.0.borrow_mut();
        if let Some(fixed) = &state.fixed {
            state.logical_scroll_top = Some(fixed.shift(current_offset, distance));
            return;
        }
        let mut cursor = state.items.cursor::<ListItemSummary>(());
        cursor.seek(&Count(current_offset.item_ix), Bias::Right);

        let start_pixel_offset = cursor.start().height + current_offset.offset_in_item;
        let new_pixel_offset = (start_pixel_offset + distance).max(px(0.));
        if new_pixel_offset > start_pixel_offset {
            cursor.seek_forward(&Height(new_pixel_offset), Bias::Right);
        } else {
            cursor.seek(&Height(new_pixel_offset), Bias::Right);
        }

        state.logical_scroll_top = Some(ListOffset {
            item_ix: cursor.start().count,
            offset_in_item: new_pixel_offset - cursor.start().height,
        });
    }

    /// Scroll the list to the given offset
    pub fn scroll_to(&self, mut scroll_top: ListOffset) {
        let state = &mut *self.0.borrow_mut();
        let item_count = state.item_count();
        if scroll_top.item_ix >= item_count {
            scroll_top.item_ix = item_count;
            scroll_top.offset_in_item = px(0.);
        }

        state.logical_scroll_top = Some(if let Some(fixed) = &state.fixed {
            fixed.shift(scroll_top, px(0.))
        } else {
            scroll_top
        });
    }

    /// Scroll the list to the given item, such that the item is fully visible.
    pub fn scroll_to_reveal_item(&self, ix: usize) {
        let state = &mut *self.0.borrow_mut();

        let mut scroll_top = state.logical_scroll_top();
        let height = state
            .last_layout_bounds
            .map_or(px(0.), |bounds| bounds.size.height);
        let padding = state.last_padding.unwrap_or_default();

        if let Some(fixed) = &state.fixed {
            if fixed.count == 0 {
                state.logical_scroll_top = Some(ListOffset::default());
                return;
            }
            let ix = ix.min(fixed.count - 1);
            let viewport = (height - padding.top - padding.bottom).max(px(0.));
            if ix <= scroll_top.item_ix || viewport == Pixels::ZERO {
                scroll_top = ListOffset {
                    item_ix: ix,
                    offset_in_item: px(0.),
                };
            } else {
                let bottom = (ix + 1 - scroll_top.item_ix) as f64
                    * f64::from(f32::from(fixed.height))
                    - f64::from(f32::from(scroll_top.offset_in_item));
                if bottom > f64::from(f32::from(viewport)) {
                    scroll_top = fixed.shift(
                        ListOffset {
                            item_ix: ix + 1,
                            offset_in_item: px(0.),
                        },
                        -viewport,
                    );
                }
            }
            state.logical_scroll_top = Some(scroll_top);
            return;
        }

        if ix <= scroll_top.item_ix {
            scroll_top.item_ix = ix;
            scroll_top.offset_in_item = px(0.);
        } else {
            let mut cursor = state.items.cursor::<ListItemSummary>(());
            cursor.seek(&Count(ix + 1), Bias::Right);
            let bottom = cursor.start().height + padding.top;
            let goal_top = px(0.).max(bottom - height + padding.bottom);

            cursor.seek(&Height(goal_top), Bias::Left);
            let start_ix = cursor.start().count;
            let start_item_top = cursor.start().height;

            if start_ix >= scroll_top.item_ix {
                scroll_top.item_ix = start_ix;
                scroll_top.offset_in_item = goal_top - start_item_top;
            }
        }

        state.logical_scroll_top = Some(scroll_top);
    }

    /// Get the bounds for the given item in window coordinates, if it's
    /// been rendered.
    pub fn bounds_for_item(&self, ix: usize) -> Option<Bounds<Pixels>> {
        let state = &*self.0.borrow();

        let bounds = state.last_layout_bounds.unwrap_or_default();
        let scroll_top = state.logical_scroll_top();
        if let Some(fixed) = &state.fixed {
            let scroll_top = fixed.rendered_top;
            if !fixed.rendered.contains(&ix) || ix < scroll_top.item_ix {
                return None;
            }
            let padding = state.last_padding.unwrap_or_default();
            let top = bounds.top()
                + padding.top
                + px(
                    ((ix - scroll_top.item_ix) as f64 * f64::from(f32::from(fixed.height))) as f32,
                )
                - scroll_top.offset_in_item;
            return Some(Bounds::from_corners(
                point(bounds.left(), top),
                point(bounds.right(), top + fixed.height),
            ));
        }
        if ix < scroll_top.item_ix {
            return None;
        }

        let mut cursor = state.items.cursor::<Dimensions<Count, Height>>(());
        cursor.seek(&Count(scroll_top.item_ix), Bias::Right);

        let scroll_top = cursor.start().1.0 + scroll_top.offset_in_item;

        cursor.seek_forward(&Count(ix), Bias::Right);
        if let Some(&ListItem::Measured { size, .. }) = cursor.item() {
            let &Dimensions(Count(count), Height(top), _) = cursor.start();
            if count == ix {
                let top = bounds.top() + top - scroll_top;
                return Some(Bounds::from_corners(
                    point(bounds.left(), top),
                    point(bounds.right(), top + size.height),
                ));
            }
        }
        None
    }

    /// Call this method when the user starts dragging the scrollbar.
    ///
    /// This will prevent the height reported to the scrollbar from changing during the drag
    /// as items in the overdraw get measured, and help offset scroll position changes accordingly.
    pub fn scrollbar_drag_started(&self) {
        let mut state = self.0.borrow_mut();
        state.scrollbar_drag_start_height = Some(state.content_height());
    }

    /// Called when the user stops dragging the scrollbar.
    ///
    /// See `scrollbar_drag_started`.
    pub fn scrollbar_drag_ended(&self) {
        self.0.borrow_mut().scrollbar_drag_start_height.take();
    }

    /// Set the offset from the scrollbar
    pub fn set_offset_from_scrollbar(&self, point: Point<Pixels>) {
        self.0.borrow_mut().set_offset_from_scrollbar(point);
    }

    /// Returns the maximum scroll offset according to the items we have measured.
    /// This value remains constant while dragging to prevent the scrollbar from moving away unexpectedly.
    pub fn max_offset_for_scrollbar(&self) -> Size<Pixels> {
        let state = self.0.borrow();
        let bounds = state.last_layout_bounds.unwrap_or_default();

        let height = state
            .scrollbar_drag_start_height
            .unwrap_or_else(|| state.content_height());

        Size::new(Pixels::ZERO, Pixels::ZERO.max(height - bounds.size.height))
    }

    /// Returns the current scroll offset adjusted for the scrollbar
    pub fn scroll_px_offset_for_scrollbar(&self) -> Point<Pixels> {
        let state = &self.0.borrow();
        let logical_scroll_top = state.logical_scroll_top();

        if let Some(fixed) = &state.fixed {
            let content_height = fixed.content_height();
            let drag_offset =
                content_height - state.scrollbar_drag_start_height.unwrap_or(content_height);
            return point(
                px(0.),
                -(fixed.pixel_offset(logical_scroll_top) - drag_offset),
            );
        }

        let mut cursor = state.items.cursor::<ListItemSummary>(());
        let summary: ListItemSummary =
            cursor.summary(&Count(logical_scroll_top.item_ix), Bias::Right);
        let content_height = state.items.summary().height;
        let drag_offset =
            // if dragging the scrollbar, we want to offset the point if the height changed
            content_height - state.scrollbar_drag_start_height.unwrap_or(content_height);
        let offset = summary.height + logical_scroll_top.offset_in_item - drag_offset;

        Point::new(px(0.), -offset)
    }

    /// Return the bounds of the viewport in pixels.
    pub fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().last_layout_bounds.unwrap_or_default()
    }
}

impl StateInner {
    fn item_count(&self) -> usize {
        self.fixed
            .as_ref()
            .map_or_else(|| self.items.summary().count, |fixed| fixed.count)
    }

    fn content_height(&self) -> Pixels {
        self.fixed.as_ref().map_or_else(
            || self.items.summary().height,
            FixedHeightItems::content_height,
        )
    }

    fn materialize_fixed_items(&mut self) {
        if let Some(fixed) = self.fixed.take() {
            self.items = SumTree::from_iter(
                (0..fixed.count).map(|_| ListItem::Unmeasured {
                    focus_handle: None,
                    estimated_height: fixed.height,
                }),
                (),
            );
            self.measuring_behavior.reset();
        }
    }

    fn adjust_scroll_for_splice(&mut self, old_range: Range<usize>, count: usize) {
        if let Some(ListOffset {
            item_ix,
            offset_in_item,
        }) = self.logical_scroll_top.as_mut()
        {
            if old_range.contains(item_ix) {
                *item_ix = old_range.start;
                *offset_in_item = px(0.);
            } else if old_range.end <= *item_ix {
                *item_ix = item_ix
                    .saturating_sub(old_range.end - old_range.start)
                    .saturating_add(count);
            }
        }
    }

    fn visible_range(&self, height: Pixels, scroll_top: &ListOffset) -> Range<usize> {
        if let Some(fixed) = &self.fixed {
            return fixed.visible_range(*scroll_top, height);
        }
        let mut cursor = self.items.cursor::<ListItemSummary>(());
        cursor.seek(&Count(scroll_top.item_ix), Bias::Right);
        let start_y = cursor.start().height + scroll_top.offset_in_item;
        cursor.seek_forward(&Height(start_y + height), Bias::Left);
        scroll_top.item_ix..cursor.start().count + 1
    }

    fn scroll(
        &mut self,
        scroll_top: &ListOffset,
        height: Pixels,
        delta: Point<Pixels>,
        current_view: EntityId,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Drop scroll events after a reset, since we can't calculate
        // the new logical scroll top without the item heights
        if self.reset {
            return;
        }

        let padding = self.last_padding.unwrap_or_default();
        if let Some(fixed) = &self.fixed {
            let viewport = (height - padding.top - padding.bottom).max(Pixels::ZERO);
            let new_top = fixed.clamp_to_viewport(fixed.shift(*scroll_top, -delta.y), viewport);
            let bottom = fixed.clamp_to_viewport(
                ListOffset {
                    item_ix: fixed.count,
                    offset_in_item: px(0.),
                },
                viewport,
            );
            let at_bottom = new_top.item_ix == bottom.item_ix
                && new_top.offset_in_item == bottom.offset_in_item;
            self.logical_scroll_top = if self.alignment == ListAlignment::Bottom && at_bottom {
                None
            } else {
                Some(new_top)
            };
            if self.scroll_handler.is_some() {
                let visible_range = self.visible_range(viewport, &new_top);
                let count = self.item_count();
                let is_scrolled = self.logical_scroll_top.is_some();
                self.scroll_handler.as_mut().unwrap()(
                    &ListScrollEvent {
                        visible_range,
                        count,
                        is_scrolled,
                    },
                    window,
                    cx,
                );
            }
            cx.notify(current_view);
            return;
        }
        let scroll_max =
            (self.items.summary().height + padding.top + padding.bottom - height).max(px(0.));
        let new_scroll_top = (self.scroll_top(scroll_top) - delta.y)
            .max(px(0.))
            .min(scroll_max);

        if self.alignment == ListAlignment::Bottom && new_scroll_top == scroll_max {
            self.logical_scroll_top = None;
        } else {
            let (start, ..) =
                self.items
                    .find::<ListItemSummary, _>((), &Height(new_scroll_top), Bias::Right);
            let item_ix = start.count;
            let offset_in_item = new_scroll_top - start.height;
            self.logical_scroll_top = Some(ListOffset {
                item_ix,
                offset_in_item,
            });
        }

        if self.scroll_handler.is_some() {
            let visible_range = self.visible_range(height, scroll_top);
            self.scroll_handler.as_mut().unwrap()(
                &ListScrollEvent {
                    visible_range,
                    count: self.items.summary().count,
                    is_scrolled: self.logical_scroll_top.is_some(),
                },
                window,
                cx,
            );
        }

        cx.notify(current_view);
    }

    fn logical_scroll_top(&self) -> ListOffset {
        self.logical_scroll_top
            .unwrap_or_else(|| match self.alignment {
                ListAlignment::Top => ListOffset {
                    item_ix: 0,
                    offset_in_item: px(0.),
                },
                ListAlignment::Bottom => ListOffset {
                    item_ix: self.item_count(),
                    offset_in_item: px(0.),
                },
            })
    }

    fn scroll_top(&self, logical_scroll_top: &ListOffset) -> Pixels {
        if let Some(fixed) = &self.fixed {
            return fixed.pixel_offset(*logical_scroll_top);
        }
        let (start, ..) = self.items.find::<ListItemSummary, _>(
            (),
            &Count(logical_scroll_top.item_ix),
            Bias::Right,
        );
        start.height + logical_scroll_top.offset_in_item
    }

    fn layout_all_items(
        &mut self,
        available_width: Pixels,
        render_item: &mut RenderItemFn,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.fixed.is_some() {
            return;
        }
        match &mut self.measuring_behavior {
            ListMeasuringBehavior::Visible => {
                return;
            }
            ListMeasuringBehavior::Measure(has_measured) => {
                if *has_measured {
                    return;
                }
                *has_measured = true;
            }
        }

        let mut cursor = self.items.cursor::<Count>(());
        let available_item_space = size(
            AvailableSpace::Definite(available_width),
            AvailableSpace::MinContent,
        );

        let mut measured_items = Vec::default();

        for (ix, item) in cursor.enumerate() {
            let size = item.size().unwrap_or_else(|| {
                let mut element = render_item(ix, window, cx);
                element.layout_as_root(available_item_space, window, cx)
            });

            measured_items.push(ListItem::Measured {
                size,
                focus_handle: item.focus_handle(),
            });
        }

        self.items = SumTree::from_iter(measured_items, ());
    }

    fn layout_items(
        &mut self,
        available_width: Option<Pixels>,
        available_height: Pixels,
        padding: &Edges<Pixels>,
        render_item: &mut RenderItemFn,
        window: &mut Window,
        cx: &mut App,
    ) -> LayoutItemsResponse {
        if self.fixed.is_some() {
            return self.layout_fixed_items(
                available_width,
                available_height,
                padding,
                render_item,
                window,
                cx,
            );
        }
        let old_items = self.items.clone();
        let mut measured_items = VecDeque::new();
        let mut item_layouts = VecDeque::new();
        let mut rendered_height = padding.top;
        let mut max_item_width = px(0.);
        let mut scroll_top = self.logical_scroll_top();
        let mut rendered_focused_item = false;

        let available_item_space = size(
            available_width.map_or(AvailableSpace::MinContent, |width| {
                AvailableSpace::Definite(width)
            }),
            AvailableSpace::MinContent,
        );

        let mut cursor = old_items.cursor::<Count>(());

        // Render items after the scroll top, including those in the trailing overdraw
        cursor.seek(&Count(scroll_top.item_ix), Bias::Right);
        for (ix, item) in cursor.by_ref().enumerate() {
            let visible_height = rendered_height - scroll_top.offset_in_item;
            if visible_height >= available_height + self.overdraw {
                break;
            }

            // Use the previously cached height and focus handle if available
            let mut size = item.size();

            // If we're within the visible area or the height wasn't cached, render and measure the item's element
            if visible_height < available_height || size.is_none() {
                let item_index = scroll_top.item_ix + ix;
                let mut element = render_item(item_index, window, cx);
                let element_size = element.layout_as_root(available_item_space, window, cx);
                size = Some(element_size);
                if visible_height < available_height {
                    item_layouts.push_back(ItemLayout {
                        index: item_index,
                        element,
                        size: element_size,
                    });
                    if item.contains_focused(window, cx) {
                        rendered_focused_item = true;
                    }
                }
            }

            let size = size.unwrap();
            rendered_height += size.height;
            max_item_width = max_item_width.max(size.width);
            measured_items.push_back(ListItem::Measured {
                size,
                focus_handle: item.focus_handle(),
            });
        }
        rendered_height += padding.bottom;

        // Prepare to start walking upward from the item at the scroll top.
        cursor.seek(&Count(scroll_top.item_ix), Bias::Right);

        // If the rendered items do not fill the visible region, then adjust
        // the scroll top upward.
        if rendered_height - scroll_top.offset_in_item < available_height {
            while rendered_height < available_height {
                cursor.prev();
                if let Some(item) = cursor.item() {
                    let item_index = cursor.start().0;
                    let mut element = render_item(item_index, window, cx);
                    let element_size = element.layout_as_root(available_item_space, window, cx);
                    let focus_handle = item.focus_handle();
                    rendered_height += element_size.height;
                    measured_items.push_front(ListItem::Measured {
                        size: element_size,
                        focus_handle,
                    });
                    item_layouts.push_front(ItemLayout {
                        index: item_index,
                        element,
                        size: element_size,
                    });
                    if item.contains_focused(window, cx) {
                        rendered_focused_item = true;
                    }
                } else {
                    break;
                }
            }

            scroll_top = ListOffset {
                item_ix: cursor.start().0,
                offset_in_item: rendered_height - available_height,
            };

            match self.alignment {
                ListAlignment::Top => {
                    scroll_top.offset_in_item = scroll_top.offset_in_item.max(px(0.));
                    self.logical_scroll_top = Some(scroll_top);
                }
                ListAlignment::Bottom => {
                    scroll_top = ListOffset {
                        item_ix: cursor.start().0,
                        offset_in_item: rendered_height - available_height,
                    };
                    self.logical_scroll_top = None;
                }
            };
        }

        // Measure items in the leading overdraw
        let mut leading_overdraw = scroll_top.offset_in_item;
        while leading_overdraw < self.overdraw {
            cursor.prev();
            if let Some(item) = cursor.item() {
                let size = if let ListItem::Measured { size, .. } = item {
                    *size
                } else {
                    let mut element = render_item(cursor.start().0, window, cx);
                    element.layout_as_root(available_item_space, window, cx)
                };

                leading_overdraw += size.height;
                measured_items.push_front(ListItem::Measured {
                    size,
                    focus_handle: item.focus_handle(),
                });
            } else {
                break;
            }
        }

        let measured_range = cursor.start().0..(cursor.start().0 + measured_items.len());
        let mut cursor = old_items.cursor::<Count>(());
        let mut new_items = cursor.slice(&Count(measured_range.start), Bias::Right);
        new_items.extend(measured_items, ());
        cursor.seek(&Count(measured_range.end), Bias::Right);
        new_items.append(cursor.suffix(), ());
        self.items = new_items;

        // If none of the visible items are focused, check if an off-screen item is focused
        // and include it to be rendered after the visible items so keyboard interaction continues
        // to work for it.
        if !rendered_focused_item {
            let mut cursor = self
                .items
                .filter::<_, Count>((), |summary| summary.has_focus_handles);
            cursor.next();
            while let Some(item) = cursor.item() {
                if item.contains_focused(window, cx) {
                    let item_index = cursor.start().0;
                    let mut element = render_item(cursor.start().0, window, cx);
                    let size = element.layout_as_root(available_item_space, window, cx);
                    item_layouts.push_back(ItemLayout {
                        index: item_index,
                        element,
                        size,
                    });
                    break;
                }
                cursor.next();
            }
        }

        LayoutItemsResponse {
            max_item_width,
            scroll_top,
            item_layouts,
        }
    }

    fn layout_fixed_items(
        &mut self,
        available_width: Option<Pixels>,
        available_height: Pixels,
        padding: &Edges<Pixels>,
        render_item: &mut RenderItemFn,
        window: &mut Window,
        cx: &mut App,
    ) -> LayoutItemsResponse {
        let fixed = self.fixed.as_ref().unwrap().clone();
        let viewport = (available_height - padding.top - padding.bottom).max(Pixels::ZERO);
        let scroll_top = fixed.clamp_to_viewport(self.logical_scroll_top(), viewport);
        let visible = if viewport == Pixels::ZERO {
            0..0
        } else {
            fixed.visible_range(scroll_top, viewport)
        };
        let available_item_space = size(
            available_width.map_or(AvailableSpace::MinContent, AvailableSpace::Definite),
            AvailableSpace::Definite(fixed.height),
        );
        let mut max_item_width = Pixels::ZERO;
        let mut item_layouts = VecDeque::new();
        for index in visible.clone() {
            let mut element = render_item(index, window, cx);
            let item_size = element.layout_as_root(available_item_space, window, cx);
            if (f32::from(item_size.height - fixed.height)).abs() > 0.01 {
                // An incorrect opt-in must retain the original variable-height
                // behavior rather than clip content or corrupt scroll geometry.
                self.materialize_fixed_items();
                return self.layout_items(
                    available_width,
                    available_height,
                    padding,
                    render_item,
                    window,
                    cx,
                );
            }
            max_item_width = max_item_width.max(item_size.width);
            item_layouts.push_back(ItemLayout {
                index,
                element,
                size: item_size,
            });
        }
        // Known heights need no off-screen measuring or retained item cache.
        let fixed = self.fixed.as_mut().unwrap();
        fixed.rendered = visible;
        fixed.rendered_top = scroll_top;
        if self.logical_scroll_top.is_some() || self.alignment == ListAlignment::Top {
            self.logical_scroll_top = Some(scroll_top);
        }
        LayoutItemsResponse {
            max_item_width,
            scroll_top,
            item_layouts,
        }
    }

    fn prepaint_items(
        &mut self,
        bounds: Bounds<Pixels>,
        padding: Edges<Pixels>,
        autoscroll: bool,
        render_item: &mut RenderItemFn,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<LayoutItemsResponse, ListOffset> {
        window.transact(|window| {
            match self.measuring_behavior {
                ListMeasuringBehavior::Measure(has_measured) if !has_measured => {
                    self.layout_all_items(bounds.size.width, render_item, window, cx);
                }
                _ => {}
            }

            let mut layout_response = self.layout_items(
                Some(bounds.size.width),
                bounds.size.height,
                &padding,
                render_item,
                window,
                cx,
            );

            // Avoid honoring autoscroll requests from elements other than our children.
            window.take_autoscroll();

            // Only paint the visible items, if there is actually any space for them (taking padding into account)
            if bounds.size.height > padding.top + padding.bottom {
                let mut item_origin = bounds.origin + Point::new(px(0.), padding.top);
                item_origin.y -= layout_response.scroll_top.offset_in_item;
                for item in &mut layout_response.item_layouts {
                    window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        item.element.prepaint_at(item_origin, window, cx);
                    });

                    if let Some(autoscroll_bounds) = window.take_autoscroll()
                        && autoscroll
                    {
                        if autoscroll_bounds.top() < bounds.top() {
                            return Err(ListOffset {
                                item_ix: item.index,
                                offset_in_item: autoscroll_bounds.top() - item_origin.y,
                            });
                        } else if autoscroll_bounds.bottom() > bounds.bottom() {
                            if let Some(fixed) = &self.fixed {
                                let viewport = bounds.size.height - padding.top - padding.bottom;
                                return Err(fixed.shift(
                                    ListOffset {
                                        item_ix: item.index,
                                        offset_in_item: px(0.),
                                    },
                                    autoscroll_bounds.bottom() - item_origin.y - viewport,
                                ));
                            }
                            let mut cursor = self.items.cursor::<Count>(());
                            cursor.seek(&Count(item.index), Bias::Right);
                            let mut height = bounds.size.height - padding.top - padding.bottom;

                            // Account for the height of the element down until the autoscroll bottom.
                            height -= autoscroll_bounds.bottom() - item_origin.y;

                            // Keep decreasing the scroll top until we fill all the available space.
                            while height > Pixels::ZERO {
                                cursor.prev();
                                let Some(item) = cursor.item() else { break };

                                let size = item.size().unwrap_or_else(|| {
                                    let mut item = render_item(cursor.start().0, window, cx);
                                    let item_available_size =
                                        size(bounds.size.width.into(), AvailableSpace::MinContent);
                                    item.layout_as_root(item_available_size, window, cx)
                                });
                                height -= size.height;
                            }

                            return Err(ListOffset {
                                item_ix: cursor.start().0,
                                offset_in_item: if height < Pixels::ZERO {
                                    -height
                                } else {
                                    Pixels::ZERO
                                },
                            });
                        }
                    }

                    item_origin.y += item.size.height;
                }
            } else {
                layout_response.item_layouts.clear();
            }

            Ok(layout_response)
        })
    }

    // Scrollbar support

    fn set_offset_from_scrollbar(&mut self, point: Point<Pixels>) {
        let Some(bounds) = self.last_layout_bounds else {
            return;
        };
        let height = bounds.size.height;

        let padding = self.last_padding.unwrap_or_default();
        let content_height = self.content_height();
        let scroll_max = (content_height + padding.top + padding.bottom - height).max(px(0.));
        let drag_offset =
            // if dragging the scrollbar, we want to offset the point if the height changed
            content_height - self.scrollbar_drag_start_height.unwrap_or(content_height);
        let new_scroll_top = (point.y - drag_offset).abs().max(px(0.)).min(scroll_max);

        if let Some(fixed) = &self.fixed {
            self.logical_scroll_top =
                if self.alignment == ListAlignment::Bottom && new_scroll_top == scroll_max {
                    None
                } else {
                    Some(fixed.offset_at_pixel(new_scroll_top))
                };
            return;
        }

        if self.alignment == ListAlignment::Bottom && new_scroll_top == scroll_max {
            self.logical_scroll_top = None;
        } else {
            let (start, _, _) =
                self.items
                    .find::<ListItemSummary, _>((), &Height(new_scroll_top), Bias::Right);

            let item_ix = start.count;
            let offset_in_item = new_scroll_top - start.height;
            self.logical_scroll_top = Some(ListOffset {
                item_ix,
                offset_in_item,
            });
        }
    }
}

impl std::fmt::Debug for ListItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unmeasured { .. } => write!(f, "Unrendered"),
            Self::Measured { size, .. } => f.debug_struct("Rendered").field("size", size).finish(),
        }
    }
}

/// An offset into the list's items, in terms of the item index and the number
/// of pixels off the top left of the item.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListOffset {
    /// The index of an item in the list
    pub item_ix: usize,
    /// The number of pixels to offset from the item index.
    pub offset_in_item: Pixels,
}

impl Element for List {
    type RequestLayoutState = ();
    type PrepaintState = ListPrepaintState;

    fn id(&self) -> Option<crate::ElementId> {
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
    ) -> (crate::LayoutId, Self::RequestLayoutState) {
        let layout_id = match self.sizing_behavior {
            ListSizingBehavior::Infer => {
                let mut style = Style::default();
                style.overflow.y = Overflow::Scroll;
                style.refine(&self.style);
                window.with_text_style(style.text_style().cloned(), |window| {
                    let state = &mut *self.state.0.borrow_mut();

                    let available_height = if let Some(last_bounds) = state.last_layout_bounds {
                        last_bounds.size.height
                    } else {
                        // If we don't have the last layout bounds (first render),
                        // we might just use the overdraw value as the available height to layout enough items.
                        state.overdraw
                    };
                    let padding = style.padding.to_pixels(
                        state.last_layout_bounds.unwrap_or_default().size.into(),
                        window.rem_size(),
                    );

                    let layout_response = state.layout_items(
                        None,
                        available_height,
                        &padding,
                        &mut self.render_item,
                        window,
                        cx,
                    );
                    let max_element_width = layout_response.max_item_width;

                    let total_height = state.content_height();

                    window.request_measured_layout(
                        style,
                        move |known_dimensions, available_space, _window, _cx| {
                            let width =
                                known_dimensions
                                    .width
                                    .unwrap_or(match available_space.width {
                                        AvailableSpace::Definite(x) => x,
                                        AvailableSpace::MinContent | AvailableSpace::MaxContent => {
                                            max_element_width
                                        }
                                    });
                            let height = match available_space.height {
                                AvailableSpace::Definite(height) => total_height.min(height),
                                AvailableSpace::MinContent | AvailableSpace::MaxContent => {
                                    total_height
                                }
                            };
                            size(width, height)
                        },
                    )
                })
            }
            ListSizingBehavior::Auto => {
                let mut style = Style::default();
                style.refine(&self.style);
                window.with_text_style(style.text_style().cloned(), |window| {
                    window.request_layout(style, None, cx)
                })
            }
        };
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> ListPrepaintState {
        let state = &mut *self.state.0.borrow_mut();
        state.reset = false;

        let mut style = Style::default();
        style.refine(&self.style);

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);

        // If the width of the list has changed, invalidate all cached item heights
        if state.fixed.is_none()
            && state
                .last_layout_bounds
                .is_none_or(|last_bounds| last_bounds.size.width != bounds.size.width)
        {
            let new_items = SumTree::from_iter(
                state.items.iter().map(|item| ListItem::Unmeasured {
                    focus_handle: item.focus_handle(),
                    estimated_height: state.estimated_item_height,
                }),
                (),
            );

            state.items = new_items;
        }

        let padding = style
            .padding
            .to_pixels(bounds.size.into(), window.rem_size());
        let layout =
            match state.prepaint_items(bounds, padding, true, &mut self.render_item, window, cx) {
                Ok(layout) => layout,
                Err(autoscroll_request) => {
                    state.logical_scroll_top = Some(autoscroll_request);
                    state
                        .prepaint_items(bounds, padding, false, &mut self.render_item, window, cx)
                        .unwrap()
                }
            };

        state.last_layout_bounds = Some(bounds);
        state.last_padding = Some(padding);
        ListPrepaintState { hitbox, layout }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<crate::Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let current_view = window.current_view();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for item in &mut prepaint.layout.item_layouts {
                item.element.paint(window, cx);
            }
        });

        let list_state = self.state.clone();
        let height = bounds.size.height;
        let scroll_top = prepaint.layout.scroll_top;
        let hitbox_id = prepaint.hitbox.id;
        let mut accumulated_scroll_delta = ScrollDelta::default();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox_id.should_handle_scroll(window) {
                accumulated_scroll_delta = accumulated_scroll_delta.coalesce(event.delta);
                let pixel_delta = accumulated_scroll_delta.pixel_delta(px(20.));
                list_state.0.borrow_mut().scroll(
                    &scroll_top,
                    height,
                    pixel_delta,
                    current_view,
                    window,
                    cx,
                )
            }
        });
    }
}

impl IntoElement for List {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for List {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl sum_tree::Item for ListItem {
    type Summary = ListItemSummary;

    fn summary(&self, _: ()) -> Self::Summary {
        match self {
            ListItem::Unmeasured {
                focus_handle,
                estimated_height,
            } => ListItemSummary {
                count: 1,
                rendered_count: 0,
                unrendered_count: 1,
                height: *estimated_height,
                has_focus_handles: focus_handle.is_some(),
            },
            ListItem::Measured {
                size, focus_handle, ..
            } => ListItemSummary {
                count: 1,
                rendered_count: 1,
                unrendered_count: 0,
                height: size.height,
                has_focus_handles: focus_handle.is_some(),
            },
        }
    }
}

impl sum_tree::ContextLessSummary for ListItemSummary {
    fn zero() -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &Self) {
        self.count += summary.count;
        self.rendered_count += summary.rendered_count;
        self.unrendered_count += summary.unrendered_count;
        self.height += summary.height;
        self.has_focus_handles |= summary.has_focus_handles;
    }
}

impl<'a> sum_tree::Dimension<'a, ListItemSummary> for Count {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a ListItemSummary, _: ()) {
        self.0 += summary.count;
    }
}

impl<'a> sum_tree::Dimension<'a, ListItemSummary> for Height {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a ListItemSummary, _: ()) {
        self.0 += summary.height;
    }
}

impl sum_tree::SeekTarget<'_, ListItemSummary, ListItemSummary> for Count {
    fn cmp(&self, other: &ListItemSummary, _: ()) -> std::cmp::Ordering {
        self.0.partial_cmp(&other.count).unwrap()
    }
}

impl sum_tree::SeekTarget<'_, ListItemSummary, ListItemSummary> for Height {
    fn cmp(&self, other: &ListItemSummary, _: ()) -> std::cmp::Ordering {
        self.0.partial_cmp(&other.height).unwrap()
    }
}

#[cfg(test)]
mod test {

    #[test]
    fn fixed_height_millions_of_items_have_no_per_row_tree_metadata() {
        use super::{ListAlignment, ListState};
        use crate::px;
        let state = ListState::new_with_fixed_item_height(
            10_000_000,
            ListAlignment::Top,
            px(120.),
            px(28.),
        );
        assert_eq!(state.item_count(), 10_000_000);
        assert_eq!(state.0.borrow().items.summary().count, 0);
        assert_eq!(state.max_offset_for_scrollbar().height, px(280_000_000.));
        state.reset(1_000_000_000);
        assert_eq!(state.item_count(), 1_000_000_000);
        assert_eq!(state.0.borrow().items.summary().count, 0);
        state.reset(0);
        state.scroll_to_reveal_item(usize::MAX);
        assert_eq!(state.item_count(), 0);
        assert_eq!(state.logical_scroll_top().item_ix, 0);
        assert_eq!(state.max_offset_for_scrollbar().height, px(0.));
    }

    #[test]
    fn fixed_height_reveal_and_small_wheel_deltas_keep_precise_row_anchors() {
        use super::{ListAlignment, ListOffset, ListState};
        use crate::{Bounds, Edges, point, px, size};
        let state = ListState::new_with_fixed_item_height(
            10_000_000,
            ListAlignment::Top,
            px(120.),
            px(28.),
        );
        {
            let mut inner = state.0.borrow_mut();
            inner.last_layout_bounds =
                Some(Bounds::new(point(px(0.), px(0.)), size(px(500.), px(280.))));
            inner.last_padding = Some(Edges::default());
        }
        state.scroll_to_reveal_item(usize::MAX);
        assert_eq!(state.logical_scroll_top().item_ix, 9_999_990);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(0.));
        state.scroll_to_reveal_item(0);
        assert_eq!(state.logical_scroll_top().item_ix, 0);
        state.reset(1_000_000_000);
        state.scroll_to(ListOffset {
            item_ix: 500_000_000,
            offset_in_item: px(7.),
        });
        state.scroll_by(px(1.));
        assert_eq!(state.logical_scroll_top().item_ix, 500_000_000);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(8.));
        state.scroll_by(px(28.));
        assert_eq!(state.logical_scroll_top().item_ix, 500_000_001);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(8.));
        state.scroll_by(-px(36.));
        assert_eq!(state.logical_scroll_top().item_ix, 500_000_000);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(0.));
        state.scroll_to(ListOffset {
            item_ix: usize::MAX,
            offset_in_item: px(100.),
        });
        assert_eq!(state.logical_scroll_top().item_ix, state.item_count());
        state.scroll_by(px(-1.));
        assert_eq!(state.logical_scroll_top().item_ix, state.item_count() - 1);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(27.));
    }

    #[test]
    fn fixed_height_scrollbar_proportions_and_mode_switches_preserve_position() {
        use super::{ListAlignment, ListOffset, ListState};
        use crate::{Bounds, point, px, size};
        let state = ListState::new_with_fixed_item_height(
            10_000_000,
            ListAlignment::Top,
            px(120.),
            px(28.),
        );
        state.0.borrow_mut().last_layout_bounds =
            Some(Bounds::new(point(px(0.), px(0.)), size(px(500.), px(280.))));
        let max = state.max_offset_for_scrollbar().height;
        state.set_offset_from_scrollbar(point(px(0.), -max * 0.5));
        assert!(state.logical_scroll_top().item_ix.abs_diff(4_999_995) <= 1);
        state.set_offset_from_scrollbar(point(px(0.), -max * 2.));
        assert!(state.logical_scroll_top().item_ix.abs_diff(9_999_990) <= 1);
        state.set_offset_from_scrollbar(point(px(0.), px(0.)));
        assert_eq!(state.logical_scroll_top().item_ix, 0);

        state.reset(100);
        state.scroll_to(ListOffset {
            item_ix: 50,
            offset_in_item: px(7.),
        });
        assert_eq!(state.set_fixed_item_height(None), None);
        assert_eq!(state.0.borrow().items.summary().count, 100);
        assert_eq!(state.logical_scroll_top().item_ix, 50);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(7.));
        assert_eq!(state.set_fixed_item_height(Some(px(28.))), Some(px(28.)));
        assert_eq!(state.0.borrow().items.summary().count, 0);
        assert_eq!(state.logical_scroll_top().item_ix, 50);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(7.));
        state.splice(50..60, 3);
        assert_eq!(state.item_count(), 93);
        assert_eq!(state.logical_scroll_top().item_ix, 50);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(0.));
        state.splice_focusable(0..0, [None, None]);
        assert_eq!(state.fixed_item_height(), None);
        assert_eq!(state.0.borrow().items.summary().count, 95);
        assert_eq!(state.logical_scroll_top().item_ix, 52);
    }

    use gpui::{ScrollDelta, ScrollWheelEvent};

    use crate::{self as gpui, TestAppContext};

    #[gpui::test]
    fn fixed_height_draw_resize_and_inferred_layout_only_visit_visible_items(
        cx: &mut TestAppContext,
    ) {
        use crate::{
            AppContext, Context, IntoElement, ListAlignment, ListOffset, ListSizingBehavior,
            ListState, Render, Styled, Window, div, list, point, px, size,
        };
        use std::{cell::Cell, rc::Rc};
        let cx = cx.add_empty_window();
        let state = ListState::new_with_fixed_item_height(
            10_000_000,
            ListAlignment::Top,
            px(120.),
            px(28.),
        );
        let rendered = Rc::new(Cell::new(0usize));
        struct TestView {
            state: ListState,
            rendered: Rc<Cell<usize>>,
        }
        impl Render for TestView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let rendered = self.rendered.clone();
                list(self.state.clone(), move |_, _, _| {
                    rendered.set(rendered.get() + 1);
                    div().h(px(28.)).w_full().into_any()
                })
                .with_sizing_behavior(ListSizingBehavior::Infer)
                .w_full()
                .h_full()
            }
        }
        for width in [500., 360.] {
            rendered.set(0);
            cx.draw(point(px(0.), px(0.)), size(px(width), px(280.)), |_, cx| {
                cx.new(|_| TestView {
                    state: state.clone(),
                    rendered: rendered.clone(),
                })
            });
            assert!(rendered.get() <= 24, "{} rendered items", rendered.get());
            assert_eq!(state.0.borrow().items.summary().count, 0);
            assert_eq!(state.fixed_item_height(), Some(px(28.)));
        }
        state.scroll_to(ListOffset {
            item_ix: state.item_count(),
            offset_in_item: px(0.),
        });
        cx.draw(point(px(0.), px(0.)), size(px(360.), px(280.)), |_, cx| {
            cx.new(|_| TestView {
                state: state.clone(),
                rendered: rendered.clone(),
            })
        });
        assert_eq!(state.logical_scroll_top().item_ix, 9_999_990);
        assert!(state.bounds_for_item(9_999_999).is_some());
        assert!(state.bounds_for_item(0).is_none());
        assert_eq!(state.0.borrow().items.summary().count, 0);
    }

    #[gpui::test]
    fn fixed_height_focus_registration_restores_measured_focus_semantics(cx: &mut TestAppContext) {
        use crate::{
            AppContext, Context, IntoElement, ListAlignment, ListState, Render, Styled, Window,
            div, list, point, px, size,
        };
        let cx = cx.add_empty_window();
        let state = ListState::new_with_fixed_item_height(20, ListAlignment::Top, px(10.), px(28.));
        struct TestView(ListState);
        impl Render for TestView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                list(self.0.clone(), |_, _, _| {
                    div().h(px(28.)).w_full().into_any()
                })
                .w_full()
                .h_full()
            }
        }
        cx.draw(point(px(0.), px(0.)), size(px(200.), px(140.)), |_, cx| {
            cx.new(|cx| {
                state.splice_focusable(0..1, [Some(cx.focus_handle())]);
                TestView(state.clone())
            })
        });
        assert_eq!(state.fixed_item_height(), None);
        assert_eq!(state.item_count(), 20);
        assert!(state.0.borrow().items.summary().has_focus_handles);
        assert_eq!(state.set_fixed_item_height(Some(px(28.))), None);
        assert!(state.0.borrow().items.summary().has_focus_handles);
    }

    #[gpui::test]
    fn test_reset_after_paint_before_scroll(cx: &mut TestAppContext) {
        use crate::{
            AppContext, Context, Element, IntoElement, ListState, Render, Styled, Window, div,
            list, point, px, size,
        };

        let cx = cx.add_empty_window();

        let state = ListState::new(5, crate::ListAlignment::Top, px(10.));

        // Ensure that the list is scrolled to the top
        state.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: px(0.0),
        });

        struct TestView(ListState);
        impl Render for TestView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                list(self.0.clone(), |_, _, _| {
                    div().h(px(10.)).w_full().into_any()
                })
                .w_full()
                .h_full()
            }
        }

        // Paint
        cx.draw(point(px(0.), px(0.)), size(px(100.), px(20.)), |_, cx| {
            cx.new(|_| TestView(state.clone()))
        });

        // Reset
        state.reset(5);

        // And then receive a scroll event _before_ the next paint
        cx.simulate_event(ScrollWheelEvent {
            position: point(px(1.), px(1.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-500.))),
            ..Default::default()
        });

        // Scroll position should stay at the top of the list
        assert_eq!(state.logical_scroll_top().item_ix, 0);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(0.));
    }

    #[gpui::test]
    fn test_scroll_by_positive_and_negative_distance(cx: &mut TestAppContext) {
        use crate::{
            AppContext, Context, Element, IntoElement, ListState, Render, Styled, Window, div,
            list, point, px, size,
        };

        let cx = cx.add_empty_window();

        let state = ListState::new(5, crate::ListAlignment::Top, px(10.));

        struct TestView(ListState);
        impl Render for TestView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                list(self.0.clone(), |_, _, _| {
                    div().h(px(20.)).w_full().into_any()
                })
                .w_full()
                .h_full()
            }
        }

        // Paint
        cx.draw(point(px(0.), px(0.)), size(px(100.), px(100.)), |_, cx| {
            cx.new(|_| TestView(state.clone()))
        });

        // Test positive distance: start at item 1, move down 30px
        state.scroll_by(px(30.));

        // Should move to item 2
        let offset = state.logical_scroll_top();
        assert_eq!(offset.item_ix, 1);
        assert_eq!(offset.offset_in_item, px(10.));

        // Test negative distance: start at item 2, move up 30px
        state.scroll_by(px(-30.));

        // Should move back to item 1
        let offset = state.logical_scroll_top();
        assert_eq!(offset.item_ix, 0);
        assert_eq!(offset.offset_in_item, px(0.));

        // Test zero distance
        state.scroll_by(px(0.));
        let offset = state.logical_scroll_top();
        assert_eq!(offset.item_ix, 0);
        assert_eq!(offset.offset_in_item, px(0.));
    }
}

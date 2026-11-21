# Local GPUI patch

This directory vendors the published `gpui` 0.2.2 crate from crates.io.
The original Apache-2.0 license is preserved in `LICENSE-APACHE`. All crate
source files are unchanged except the patches below. Cargo registry bookkeeping
files `.cargo-ok` and `.cargo-checksum.json` are omitted.

- Package: https://crates.io/crates/gpui/0.2.2
- Crate archive SHA-256: `979b45cfa6ec723b6f42330915a1b3769b930d02b2d505f9697f8ca602bee707`
- Upstream VCS metadata: commit `69e2130295c2649963eb639fc70b4f2ee8ea1624`,
  `crates/gpui`; the published archive records `dirty: true`, so the archive,
  rather than that commit alone, is the exact source provenance.

## Release window-state lock before re-entering AppKit

On macOS 27, activating the viewer can cause AppKit to synchronously send
`windowDidResignKey:` while GPUI handles a spurious `windowDidBecomeKey:`.
The published implementation calls `resignKeyWindow` while holding the same
window-state mutex that the nested delegate callback immediately locks,
deadlocking the UI thread. The application hang was confirmed with a native
process sample.

Capture the existing native window pointer and drop the mutex guard before
sending `resignKeyWindow`. Keep the existing spurious-event workaround and
early return. The native window remains owned by the live callback's window
state; this change adds no ownership transfer or new asynchronous operation.

The same capture-and-unlock ordering is present in the current upstream
`crates/gpui_macos/src/window.rs` implementation, verified on 2026-10-30:
https://github.com/zed-industries/zed/blob/main/crates/gpui_macos/src/window.rs
This is a narrow local backport against the published 0.2.2 crate, rather than
an upgrade to the current upstream workspace.

```diff
--- gpui-0.2.2/src/platform/mac/window.rs
+++ vendor/gpui/src/platform/mac/window.rs
@@ -1988,8 +1988,12 @@
     // in theory, we're not supposed to invoke this method manually but it balances out
     // the spurious `becomeKeyWindow` event and helps us work around that bug.
     if selector == sel!(windowDidBecomeKey:) && !is_active {
-        unsafe {
-            let _: () = msg_send![lock.native_window, resignKeyWindow];
+        let native_window = lock.native_window;
+        // AppKit may synchronously send windowDidResignKey: from this call.
+        // Release the window-state mutex before re-entering its delegate.
+        drop(lock);
+        unsafe {
+            let _: () = msg_send![native_window, resignKeyWindow];
             return;
         }
     }
```

## Optional height estimates for lazily measured lists

Add `ListState::new_with_estimated_item_height` while retaining the existing
constructor's zero-height default. Store the estimate only on unmeasured items;
visible rows still replace it with their actual measured height. Propagate the
estimate through splices and width invalidation. Negative or nonfinite estimates
are treated as zero.

The viewer uses a 28-pixel baseline for unseen rows, so the scrollbar and distant
item navigation account for the entire result without shaping every cell. Two
list states retain their own width-dependent caches; the application synchronizes
their logical offsets and measures each visible row once across both panes.
Tests in `app::variable_row_tests` cover far navigation, default/zero/invalid
estimates, resets, and pane synchronization.

This API is a local extension against the published crate, not an upstream API.

```diff
--- gpui-0.2.2/src/elements/list.rs
+++ vendor/gpui/src/elements/list.rs
@@ -71,6 +71,7 @@
     scroll_handler: Option<Box<dyn FnMut(&ListScrollEvent, &mut Window, &mut App)>>,
     scrollbar_drag_start_height: Option<Pixels>,
     measuring_behavior: ListMeasuringBehavior,
+    estimated_item_height: Pixels,
 }
 
 /// Whether the list is scrolling from top to bottom or bottom to top.
@@ -156,6 +157,7 @@
 enum ListItem {
     Unmeasured {
         focus_handle: Option<FocusHandle>,
+        estimated_height: Pixels,
     },
     Measured {
         size: Size<Pixels>,
@@ -174,7 +176,7 @@
 
     fn focus_handle(&self) -> Option<FocusHandle> {
         match self {
-            ListItem::Unmeasured { focus_handle } | ListItem::Measured { focus_handle, .. } => {
+            ListItem::Unmeasured { focus_handle, .. } | ListItem::Measured { focus_handle, .. } => {
                 focus_handle.clone()
             }
         }
@@ -182,7 +184,7 @@
 
     fn contains_focused(&self, window: &Window, cx: &App) -> bool {
         match self {
-            ListItem::Unmeasured { focus_handle } | ListItem::Measured { focus_handle, .. } => {
+            ListItem::Unmeasured { focus_handle, .. } | ListItem::Measured { focus_handle, .. } => {
                 focus_handle
                     .as_ref()
                     .is_some_and(|handle| handle.contains_focused(window, cx))
@@ -214,6 +216,26 @@
     /// be measured even though they are not visible. This can help ensure
     /// that the list doesn't flicker or pop in when scrolling.
     pub fn new(item_count: usize, alignment: ListAlignment, overdraw: Pixels) -> Self {
+        Self::new_with_estimated_item_height(item_count, alignment, overdraw, px(0.))
+    }
+
+    /// Construct a lazily measured list with an initial height estimate for
+    /// unmeasured items. This allows scrollbars and distant item navigation to
+    /// account for items that have not entered the viewport yet. Visible items
+    /// replace the estimate with their measured height. Estimates must be finite
+    /// and nonnegative; invalid values are treated as zero. `new` retains its
+    /// original zero-height behavior.
+    pub fn new_with_estimated_item_height(
+        item_count: usize,
+        alignment: ListAlignment,
+        overdraw: Pixels,
+        estimated_item_height: Pixels,
+    ) -> Self {
+        let estimated_item_height = if f32::from(estimated_item_height).is_finite() {
+            estimated_item_height.max(px(0.))
+        } else {
+            px(0.)
+        };
         let this = Self(Rc::new(RefCell::new(StateInner {
             last_layout_bounds: None,
             last_padding: None,
@@ -225,6 +247,7 @@
             reset: false,
             scrollbar_drag_start_height: None,
             measuring_behavior: ListMeasuringBehavior::default(),
+            estimated_item_height,
         })));
         this.splice(0..0, item_count);
         this
@@ -281,10 +304,14 @@
         old_items.seek_forward(&Count(old_range.end), Bias::Right);
 
         let mut spliced_count = 0;
+        let estimated_height = state.estimated_item_height;
         new_items.extend(
             focus_handles.into_iter().map(|focus_handle| {
                 spliced_count += 1;
-                ListItem::Unmeasured { focus_handle }
+                ListItem::Unmeasured {
+                    focus_handle,
+                    estimated_height,
+                }
             }),
             (),
         );
@@ -1030,6 +1057,7 @@
             let new_items = SumTree::from_iter(
                 state.items.iter().map(|item| ListItem::Unmeasured {
                     focus_handle: item.focus_handle(),
+                    estimated_height: state.estimated_item_height,
                 }),
                 (),
             );
@@ -1114,11 +1142,14 @@
 
     fn summary(&self, _: ()) -> Self::Summary {
         match self {
-            ListItem::Unmeasured { focus_handle } => ListItemSummary {
+            ListItem::Unmeasured {
+                focus_handle,
+                estimated_height,
+            } => ListItemSummary {
                 count: 1,
                 rendered_count: 0,
                 unrendered_count: 1,
-                height: px(0.),
+                height: *estimated_height,
                 has_focus_handles: focus_handle.is_some(),
             },
             ListItem::Measured {
```

## Constant-size metadata for explicitly uniform-height lists

Add opt-in `ListState::new_with_fixed_item_height`,
`set_fixed_item_height(Option<Pixels>)`, and `fixed_item_height()`. The existing
`list` element and its public scrolling interface remain in use. A fixed list
stores its row count, positive finite item height, and last visible range; its
per-row `SumTree` stays empty. Construction, reset, ordinary splices, and width
changes therefore do not build or invalidate one metadata entry per result row.
Only visible elements are laid out and painted. `Infer` sizing, item bounds,
autoscroll, alignment, reveal, scroll handlers, and scrollbar APIs use the same
logical row offsets as the measured path.

The viewer opts in at 28 logical pixels when no displayed column wraps. Both
table panes and both frozen-row panes use the same mode. Enabling wrap restores
the original estimated/measured tree; disabling wrap returns to uniform storage.
Mode changes preserve the logical row anchor and its within-row pixel offset.
Publishing a new result switches modes while empty to avoid materializing the
previous result's row count. Wheel scrolling uses integer row anchors with local
pixel arithmetic, so small movements remain precise far down a large result;
the scrollbar's overall pixel extent still has GPUI's existing `f32` precision.

This mode requires uniform, non-focusable items. Calling `splice_focusable`
first materializes the original measured tree, and a tree containing focus
handles refuses uniform storage. If a rendered item disagrees with the supplied
height, layout falls back to measured storage rather than clipping its content.
Invalid fixed heights also fall back to the estimated-height constructor. The
original variable-height constructors and rendering path retain their behavior.
Explicitly switching a large list to variable heights or registering focusable
items still requires per-row tree metadata; this patch does not make that path
sparse or bound the loaded Arrow data.

Regression coverage in `elements::list::test` checks ten million and one billion
rows with zero tree entries, empty/reset behavior, first/last reveal, precise
far-down wheel deltas, scrollbar proportions and clamping, splice/focus fallback,
and position preservation across mode changes. GPUI drawing tests cover visible
work bounds, width changes, `Infer`, item bounds, and focus-handle retention.
`app::variable_row_tests` additionally checks both table panes with ten million
rows and switching wrap storage without losing their synchronized position.
These APIs are local extensions against GPUI 0.2.2, not upstream APIs.

## Let the application own custom titlebar mouse events on macOS 27

Register the GPUIView `_opaqueRectForWindowMoveWhenInTitlebar` override and
return the view's bounds. Current upstream GPUI uses this AppKit selector to
mark custom titlebar content as application-owned, preventing AppKit from
starting system titlebar drags or delaying mouse events to disambiguate a
double-click. Its source explicitly identifies the click delay on macOS 27.

This is a narrow backport of the implementation in Zed's
`crates/gpui_macos/src/window.rs`, verified on 2026-10-31 against the latest
commit affecting that file, `0373366f43e38375f344c733d0dbc9f856333366`:
https://github.com/zed-industries/zed/blob/0373366f43e38375f344c733d0dbc9f856333366/crates/gpui_macos/src/window.rs
The upstream implementation and this vendored code retain the Apache-2.0
license. The selector is an undocumented AppKit hook already used by upstream;
it is not presented as a public Apple API.

Unlike upstream's configurable `app_owns_titlebar_drag` flag, this application
always returns the bounds. Its GPUI windows use a custom titlebar with the
standard AppKit buttons and initiate native dragging from the designated
filename region. Native NSOpenPanel/NSSavePanel windows are not GPUIView
instances and are unaffected.

The callback only reads NSView.bounds, without acquiring MacWindowState's
mutex, so AppKit can query it during another native window callback. No
Objective-C ownership transfer or runtime method replacement is introduced.

```diff
--- gpui-0.2.2/src/platform/mac/window.rs
+++ vendor/gpui/src/platform/mac/window.rs
@@ -245,6 +245,12 @@
                 );
 
+                decl.add_method(
+                    sel!(_opaqueRectForWindowMoveWhenInTitlebar),
+                    opaque_rect_for_window_move_when_in_titlebar
+                        as extern "C" fn(&Object, Sel) -> NSRect,
+                );
+
                 decl.add_method(
                     sel!(characterIndexForPoint:),
@@ -2344,6 +2350,10 @@
+extern "C" fn opaque_rect_for_window_move_when_in_titlebar(this: &Object, _: Sel) -> NSRect {
+    unsafe { msg_send![this, bounds] }
+}
+
 extern "C" fn accepts_first_mouse(this: &Object, _: Sel, _: id) -> BOOL {
```

## Keep Metal atlas textures alive while they still have live keys

Removing a tile first drops its key from the map, then looks up the texture by
index and decrements its reference count. The atlas releases the texture slot
and returns its index to the free list only after the last key is released.
Pending GPU commands can still read a partially occupied atlas texture.
See the [Metal atlas implementation](src/platform/mac/metal_atlas.rs).

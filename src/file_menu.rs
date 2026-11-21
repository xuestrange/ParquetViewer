//! File actions in the custom toolbar.
use super::*;

struct FileCommand {
    label: &'static str,
    icon: Icon,
    shortcut: &'static str,
    enabled: bool,
    action: Box<dyn gpui::Action>,
}

impl Viewer {
    pub(in crate::app) fn render_file_menu(
        &self,
        window: &Window,
        toolbar_height: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let dark = self.preferences.dark;
        let c = Colors::new(dark);
        let has_file = !self.paths.is_empty();
        let can_load = self.engine.is_some() && self.busy.is_none() && self.panel.is_none();
        let commands = [
            FileCommand {
                label: "Load Range…",
                icon: Icon::RowRange,
                shortcut: "",
                enabled: can_load,
                action: Box::new(LoadRange),
            },
            FileCommand {
                label: "Load All Records",
                icon: Icon::AllRows,
                shortcut: "⌘E",
                enabled: can_load,
                action: Box::new(LoadAll),
            },
            FileCommand {
                label: "New Window",
                icon: Icon::Expand,
                shortcut: "⌘N",
                enabled: true,
                action: Box::new(NewWindow),
            },
            FileCommand {
                label: "Clear Current File",
                icon: Icon::Clear,
                shortcut: "",
                enabled: has_file,
                action: Box::new(New),
            },
            FileCommand {
                label: "Open File…",
                icon: Icon::Open,
                shortcut: "⌘O",
                enabled: true,
                action: Box::new(OpenFile),
            },
            FileCommand {
                label: "Import Text File…",
                icon: Icon::Import,
                shortcut: "",
                enabled: true,
                action: Box::new(ImportTextFile),
            },
            FileCommand {
                label: if self.csv_options.is_some() {
                    "Reimport…"
                } else {
                    "Reload"
                },
                icon: Icon::Reload,
                shortcut: "⌘R",
                enabled: has_file,
                action: Box::new(Reload),
            },
            FileCommand {
                label: "Close Window",
                icon: Icon::Close,
                shortcut: "⌘W",
                enabled: true,
                action: Box::new(CloseWindow),
            },
        ];
        let left = 8.
            + window_chrome::leading_inset(window)
            + command_widths(window)[0]
            + COMMAND_GROUP_GAP;
        let viewport = window.viewport_size();
        let width = 260_f32.min((f32::from(viewport.width) - left - 8.).max(160.));
        let height = (f32::from(viewport.height) - toolbar_height - 8.).max(30.);
        let dismiss =
            |this: &mut Viewer, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>| {
                this.file_menu_open = false;
                cx.stop_propagation();
                cx.notify();
            };
        div()
            .id("file-menu-backdrop")
            .occlude()
            .absolute()
            .inset_0()
            .on_mouse_down(MouseButton::Left, cx.listener(dismiss))
            .on_mouse_down(MouseButton::Right, cx.listener(dismiss))
            .child(
                controls::menu_surface("file-menu", c)
                    .absolute()
                    .top(px(toolbar_height))
                    .left(px(left))
                    .w(px(width))
                    .max_h(px(height))
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .children(commands.into_iter().enumerate().map(|(index, command)| {
                        let FileCommand {
                            label,
                            icon,
                            shortcut,
                            enabled,
                            action,
                        } = command;
                        controls::menu_item(("file-command", index), c, false, enabled)
                            .child(icons::glyph(icon, 14., dark))
                            .child(div().flex_1().min_w_0().truncate().child(t(label)))
                            .when(!shortcut.is_empty(), |row| {
                                row.child(div().text_color(rgb(c.muted)).child(shortcut))
                            })
                            .when(enabled, |row| {
                                row.on_click(cx.listener(move |this, _, window, cx| {
                                    this.file_menu_open = false;
                                    this.focus.focus(window);
                                    window.dispatch_action(action.boxed_clone(), cx);
                                    cx.notify();
                                }))
                            })
                    })),
            )
            .into_any_element()
    }
}

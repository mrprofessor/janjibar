# gpui-kit pieces we use

gpui-kit 0.9.0 is three layers: GPUI itself (`gpui_kit::*`), `gpui-base`
(`gpui_kit::base`) and the styled component library `gpui-component`
(`gpui_kit::component`). Assets (Lucide icons) come from `gpui-kit-assets`.

## Startup

```rust
gpui_kit::application().run(|cx| {
    gpui_kit::init(cx);           // component + base init, theme global
    ui::init(cx);                 // our fonts, theme override, key bindings
    gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(window, cx)))?;
});
```

`open_window` wraps our view in `base::Root`, which hosts popovers, dialogs
and notifications. Call it inside `cx.update` from async code.

## Theme override

The kit keeps a `Theme` global (`gpui_kit::component::Theme`). It derefs to
`ThemeColor` (fields like `background`, `border`, `table_head`, `table_hover`,
`table_active`, `scrollbar_thumb`...) and also holds `tokens`, a second copy
some components read (`cx.theme().tokens.table_hover`). Change colours only
through `Theme::update(cx, |theme| ...)`, which reconciles `tokens` with the
colours and refreshes windows. Switch mode with `Theme::change` before that,
never inside it. `crates/ui/src/theme.rs` does this once at startup from our
graphite tokens. Read colours in views with
`cx.theme()` (trait `ActiveTheme`) only for kit components; our own views use
`ui::theme` tokens directly.

## Table

```rust
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};

struct ResourceTable { columns: Arc<[model::Column]>, rows: Arc<[model::TableRow]> }

impl TableDelegate for ResourceTable {
    fn columns_count(&self, _: &App) -> usize { self.columns.len() }
    fn rows_count(&self, _: &App) -> usize { self.rows.len() }
    fn column(&self, ix: usize, _: &App) -> Column { self.columns[ix].clone() }
    fn render_td(&mut self, row: usize, col: usize, _: &mut Window,
                 _: &mut Context<TableState<Self>>) -> impl IntoElement { /* cell */ }
}

let state = cx.new(|cx| TableState::new(ResourceTable::default(), window, cx));
// in render (Sizable trait in scope):
DataTable::new(&state).with_size(Size::Small).header_row_height(px(28.)) // 30 px rows
```

- `Column::new(key, name).width(px(..))`, `.text_right()`, `.sortable()`,
  `.resizable(bool)`, `.fixed_left()`.
- Override `render_tr` to style a row (it is refined onto the kit's row, so
  `.rounded(px(8.))` works), `render_th` for headers, `render_empty` for the
  empty state, `cell_label` for screen readers.
- `perform_sort` is called with the column and direction; sorting belongs in
  the engine eventually, the delegate just records the choice.
- After swapping rows: `state.update(cx, |t, cx| { t.delegate_mut().rows = rows; cx.notify(); })`.
  `refresh` is only for column changes. `ui::resource_table::set_rows` also
  keeps the selected object selected by key. Note `set_selected_row`
  scrolls to the row, so call it only when the index really changed.
- Events: subscribe to `TableEvent` (`SelectRow`, `DoubleClickedRow`,
  `ColumnWidthsChanged`...).

## Helpers

- `h_flex()` and `v_flex()` (flex row and column with `items_center` for
  `h_flex`).
- `Icon::new(IconName::Box)` and the full Lucide catalog through
  `gpui_kit::assets::IconName`. Size with `.size_3p5()` or `.with_size(..)`.
- `Sizable` (`.small()`, `.xsmall()`, `.with_size(Size::Size(px(..)))`).
- `ActiveTheme` (`cx.theme()`).

## Fonts

The kit ships no fonts; its defaults are the system UI font and Menlo. We
bundle Geist and JetBrains Mono (OFL) under `crates/ui/assets/fonts` and call
`cx.text_system().add_fonts(vec![Cow::Borrowed(BYTES), ...])` in `ui::init`,
then set `Theme::font_family` and `mono_font_family`.

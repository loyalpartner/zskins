use gpui::AnyElement;

/// A data source the launcher lists, filters, renders, and activates.
pub trait Source: 'static {
    fn placeholder(&self) -> &'static str;
    fn empty_text(&self) -> &'static str;

    fn filter(&self, query: &str) -> Vec<usize>;

    /// `selected` only drives content highlight; the row container, hover, and
    /// click handling live in the launcher.
    fn render_item(&self, ix: usize, selected: bool) -> AnyElement;

    fn activate(&self, ix: usize);
}

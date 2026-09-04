//! State shared by the compact composer selector menus.

/// The selector whose menu is currently anchored above the composer controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectorKind {
    Profile,
    Thinking,
    Model,
    Context,
}

impl SelectorKind {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Thinking => "thinking",
            Self::Model => "model",
            Self::Context => "context",
        }
    }
}

/// Enforces the one-open-menu rule and validates item selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectorMenuState {
    open: Option<SelectorKind>,
    highlighted: usize,
}

impl SelectorMenuState {
    pub fn open(&self) -> Option<SelectorKind> {
        self.open
    }

    pub fn toggle(&mut self, kind: SelectorKind) {
        self.open_at(kind, 0);
    }

    pub fn open_at(&mut self, kind: SelectorKind, selected: usize) {
        if self.open == Some(kind) {
            self.dismiss();
        } else {
            self.open = Some(kind);
            self.highlighted = selected;
        }
    }

    pub fn highlighted(&self) -> Option<usize> {
        self.open.map(|_| self.highlighted)
    }

    pub fn move_highlight(&mut self, delta: isize, option_count: usize) -> Option<usize> {
        self.open?;
        if option_count == 0 {
            return None;
        }
        self.highlighted =
            (self.highlighted as isize + delta).rem_euclid(option_count as isize) as usize;
        Some(self.highlighted)
    }

    pub fn dismiss(&mut self) {
        self.open = None;
    }

    pub fn choose_highlighted(&mut self, kind: SelectorKind, option_count: usize) -> Option<usize> {
        self.choose(kind, self.highlighted, option_count)
    }

    /// Return the exact selected index only when the matching menu is open and
    /// the index exists. Invalid clicks leave the current menu open.
    pub fn choose(
        &mut self,
        kind: SelectorKind,
        index: usize,
        option_count: usize,
    ) -> Option<usize> {
        if self.open != Some(kind) || index >= option_count {
            return None;
        }
        self.open = None;
        Some(index)
    }
}

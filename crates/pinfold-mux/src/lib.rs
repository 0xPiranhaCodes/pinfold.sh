//! The pure layout model of the multiplexer: session, tab, split tree and pane.
//!
//! It runs nothing and owns no terminal. Pane ids come from the caller, and every operation
//! either changes the model or returns an error and leaves it as it was.
//!
//! ```
//! use pinfold_mux::{Direction, Placement, Rect, Session, Size, SplitDirection};
//!
//! let mut session = Session::default();
//! session.new_tab(1)?;
//! session.split(1, 2, SplitDirection::Right)?;
//! assert_eq!(session.move_focus(Direction::Left), Some(1));
//!
//! let tab = session.active_tab().ok_or("no tab")?;
//! let left = Rect { x: 0, y: 0, cols: 39, rows: 24 };
//! let right = Rect { x: 40, y: 0, cols: 40, rows: 24 };
//! assert_eq!(
//!     tab.layout(Size { cols: 80, rows: 24 }),
//!     [(1, Placement::At(left)), (2, Placement::At(right))],
//! );
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

/// A pane, named by the caller. Unique across a session.
pub type PaneId = u32;

/// The smallest rectangle a pane is placed in: a wide character across one row.
pub const MIN_PANE: Size = Size { cols: 2, rows: 1 };

/// The denominator of every split's share. Divisible by each integer up to 16.
///
/// A fixed denominator keeps a share's sum an integer. With a free one, resizing at different
/// window sizes grows the denominator until it overflows.
const WHOLE: u32 = 720_720;

/// Focus moves on this canvas, so it works in any tab size.
///
/// shortcut: a tab squeezed by [`MIN_PANE`] can differ from this canvas and pick another
/// neighbour, pass the tab's size if that shows in use.
const CANVAS: Size = Size {
    cols: u16::MAX,
    rows: u16::MAX,
};

/// A size in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    /// Width.
    pub cols: u16,
    /// Height.
    pub rows: u16,
}

/// A rectangle of cells, from the top left of its tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Width.
    pub cols: u16,
    /// Height.
    pub rows: u16,
}

/// Where a pane goes in a layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// In this rectangle.
    At(Rect),
    /// Nowhere, because another pane is zoomed.
    Hidden,
    /// Nowhere, because the tab has no room for it at [`MIN_PANE`].
    TooSmall,
}

/// Where a split puts its second pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDirection {
    /// Side by side, with a border one column wide.
    Right,
    /// Stacked, with a border one row high.
    Down,
}

/// A direction to move focus or a border.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Towards column 0.
    Left,
    /// Away from column 0.
    Right,
    /// Towards row 0.
    Up,
    /// Away from row 0.
    Down,
}

impl Direction {
    /// The kind of split that has a border to cross here, and whether this is towards its
    /// second pane.
    fn across(self) -> (SplitDirection, bool) {
        match self {
            Self::Left => (SplitDirection::Right, false),
            Self::Right => (SplitDirection::Right, true),
            Self::Up => (SplitDirection::Down, false),
            Self::Down => (SplitDirection::Down, true),
        }
    }
}

/// Why an operation changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    /// No tab holds this pane.
    #[error("no pane {0}")]
    UnknownPane(PaneId),
    /// A tab already holds this pane.
    #[error("pane {0} already exists")]
    DuplicatePane(PaneId),
    /// There is no tab at this position.
    #[error("no tab at position {0}")]
    UnknownTab(usize),
    /// No split along that direction, or its border cannot move any further.
    #[error("that border cannot move")]
    CannotResize,
}

/// One entry of a tab's split tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Node {
    Pane(PaneId),
    /// `share / WHOLE` of the space goes to the first child.
    Split {
        direction: SplitDirection,
        share: u32,
    },
}

impl Node {
    fn pane(&self) -> Option<PaneId> {
        match *self {
            Self::Pane(pane) => Some(pane),
            Self::Split { .. } => None,
        }
    }
}

/// A tab: a binary split tree of panes, and which one has focus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    /// In pre-order: a split is followed by its first subtree, then its second. Without
    /// pointers a node is 8 bytes, and walking the tree cannot overflow the stack however
    /// deep it is.
    nodes: Vec<Node>,
    focus: PaneId,
    zoomed: bool,
}

impl Tab {
    fn new(pane: PaneId) -> Self {
        Self {
            nodes: vec![Node::Pane(pane)],
            focus: pane,
            zoomed: false,
        }
    }

    /// The panes, in tree order.
    pub fn panes(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.nodes.iter().filter_map(Node::pane)
    }

    /// The pane with focus.
    #[must_use]
    pub fn focused(&self) -> PaneId {
        self.focus
    }

    /// Whether the focused pane fills the tab.
    #[must_use]
    pub fn zoomed(&self) -> bool {
        self.zoomed
    }

    /// Places every pane in a tab of `size`, in tree order, each exactly once.
    ///
    /// Each border takes one cell. A split keeps both sides at their minimum if it can. If it
    /// cannot, its first side takes all the space and each pane of the second is
    /// [`Placement::TooSmall`].
    ///
    /// ```
    /// use pinfold_mux::{Placement, Rect, Session, Size, SplitDirection};
    ///
    /// let mut session = Session::default();
    /// session.new_tab(1)?;
    /// session.split(1, 2, SplitDirection::Right)?;
    /// let tab = session.active_tab().ok_or("no tab")?;
    ///
    /// let narrow = Rect { x: 0, y: 0, cols: 3, rows: 24 };
    /// assert_eq!(
    ///     tab.layout(Size { cols: 3, rows: 24 }),
    ///     [(1, Placement::At(narrow)), (2, Placement::TooSmall)],
    /// );
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn layout(&self, size: Size) -> Vec<(PaneId, Placement)> {
        let place = |rect: Rect| {
            if rect.cols >= MIN_PANE.cols && rect.rows >= MIN_PANE.rows {
                Placement::At(rect)
            } else {
                Placement::TooSmall
            }
        };
        self.placed(size)
            .into_iter()
            .map(|(pane, rect)| match (self.zoomed, pane == self.focus) {
                (false, _) => (pane, place(rect)),
                (true, true) => (pane, place(Rect::whole(size))),
                (true, false) => (pane, Placement::Hidden),
            })
            .collect()
    }

    fn index_of(&self, pane: PaneId) -> Option<usize> {
        self.nodes.iter().position(|node| *node == Node::Pane(pane))
    }

    /// The parent of each node.
    fn parents(&self) -> Vec<Option<usize>> {
        let mut pending = vec![None];
        let mut parents = Vec::with_capacity(self.nodes.len());
        for (index, node) in self.nodes.iter().enumerate() {
            parents.push(pending.pop().flatten());
            if matches!(node, Node::Split { .. }) {
                pending.extend([Some(index), Some(index)]);
            }
        }
        parents
    }

    /// What each split's first and second sides need along its direction, by node.
    fn needs(&self) -> Vec<(u16, u16)> {
        let mut needs = vec![(0, 0); self.nodes.len()];
        // Backwards, a split comes after its subtrees, with the first one on top.
        let mut below: Vec<Size> = Vec::new();
        for (node, need) in self.nodes.iter().zip(&mut needs).rev() {
            let size = match *node {
                Node::Pane(_) => MIN_PANE,
                Node::Split { direction, .. } => {
                    let first = below.pop().unwrap_or(MIN_PANE);
                    let second = below.pop().unwrap_or(MIN_PANE);
                    match direction {
                        SplitDirection::Right => {
                            *need = (first.cols, second.cols);
                            Size {
                                cols: first.cols.saturating_add(second.cols).saturating_add(1),
                                rows: first.rows.max(second.rows),
                            }
                        }
                        SplitDirection::Down => {
                            *need = (first.rows, second.rows);
                            Size {
                                cols: first.cols.max(second.cols),
                                rows: first.rows.saturating_add(second.rows).saturating_add(1),
                            }
                        }
                    }
                }
            };
            below.push(size);
        }
        needs
    }

    /// The rectangle of each node.
    fn rects(&self, size: Size) -> Vec<Rect> {
        let mut pending = vec![Rect::whole(size)];
        let mut rects = Vec::with_capacity(self.nodes.len());
        for (node, need) in self.nodes.iter().zip(self.needs()) {
            let rect = pending.pop().unwrap_or_default();
            if let Node::Split { direction, share } = *node {
                let (first, second) = rect.divide(direction, share, need);
                pending.extend([second, first]);
            }
            rects.push(rect);
        }
        rects
    }

    fn placed(&self, size: Size) -> Vec<(PaneId, Rect)> {
        let rects = self.nodes.iter().zip(self.rects(size));
        rects
            .filter_map(|(node, rect)| Some((node.pane()?, rect)))
            .collect()
    }

    /// The pane across the border in `direction` that shares most of it with the focused one.
    fn neighbour(&self, direction: Direction) -> Option<PaneId> {
        let panes = self.placed(CANVAS);
        let from = panes.iter().find(|(pane, _)| *pane == self.focus)?.1;
        let shared = |&(pane, rect): &(PaneId, Rect)| Some((from.shared(rect, direction)?, pane));
        panes.iter().filter_map(shared).max().map(|(_, pane)| pane)
    }

    fn split(&mut self, pane: PaneId, new: PaneId, direction: SplitDirection) -> Option<()> {
        let index = self.index_of(pane)?;
        self.nodes.insert(
            index,
            Node::Split {
                direction,
                share: WHOLE / 2,
            },
        );
        self.nodes.insert(index + 2, Node::Pane(new));
        self.focus = new;
        self.zoomed = false;
        Some(())
    }

    /// Takes a pane out, and returns `false` if it is the last one here and stays.
    fn close(&mut self, pane: PaneId) -> bool {
        let Some(index) = self.index_of(pane) else {
            return true;
        };
        let Some(&Some(parent)) = self.parents().get(index) else {
            return false;
        };
        // The parent comes first, so what remains of it is the sibling, in the parent's place.
        self.nodes.remove(index);
        self.nodes.remove(parent);
        if self.focus == pane {
            let sibling = self.nodes.get(parent..).unwrap_or_default();
            self.focus = sibling.iter().find_map(Node::pane).unwrap_or(pane);
        }
        true
    }

    fn resize(
        &mut self,
        pane: PaneId,
        direction: Direction,
        cells: u16,
        size: Size,
    ) -> Result<(), LayoutError> {
        let (across, forward) = direction.across();
        let parents = self.parents();
        let mut at = self.index_of(pane).ok_or(LayoutError::UnknownPane(pane))?;
        loop {
            at = parents
                .get(at)
                .copied()
                .flatten()
                .ok_or(LayoutError::CannotResize)?;
            let node = self.nodes.get(at);
            if matches!(node, Some(Node::Split { direction, .. }) if *direction == across) {
                break;
            }
        }

        let rect = self.rects(size).get(at).copied().unwrap_or_default();
        let need = self.needs().get(at).copied().unwrap_or_default();
        let Some(Node::Split { share, .. }) = self.nodes.get_mut(at) else {
            return Err(LayoutError::CannotResize);
        };
        let length = rect.length(across);
        let before = first_length(length, *share, need).ok_or(LayoutError::CannotResize)?;
        // Rounded up, so the border moves by at least `cells`. The same call the other way
        // takes the same steps, which is why a resize undoes exactly.
        let room = u64::from(length.saturating_sub(1)).max(1);
        let steps = (u64::from(cells) * u64::from(WHOLE)).div_ceil(room);
        let current = u64::from(*share);
        let moved = if forward {
            current.checked_add(steps)
        } else {
            current.checked_sub(steps)
        };
        let moved = moved
            .filter(|moved| (1..u64::from(WHOLE)).contains(moved))
            .and_then(|moved| u32::try_from(moved).ok())
            .ok_or(LayoutError::CannotResize)?;
        // Refused, or presses past a pane's minimum would each have to be undone unseen.
        if first_length(length, moved, need) == Some(before) {
            return Err(LayoutError::CannotResize);
        }
        *share = moved;
        Ok(())
    }
}

impl Rect {
    fn whole(size: Size) -> Self {
        Self {
            x: 0,
            y: 0,
            cols: size.cols,
            rows: size.rows,
        }
    }

    fn length(self, direction: SplitDirection) -> u16 {
        match direction {
            SplitDirection::Right => self.cols,
            SplitDirection::Down => self.rows,
        }
    }

    /// Both sides of a split, and an empty second side if they do not both fit.
    fn divide(self, direction: SplitDirection, share: u32, need: (u16, u16)) -> (Self, Self) {
        let Some(first) = first_length(self.length(direction), share, need) else {
            return (self, Self::default());
        };
        let gap = first.saturating_add(1);
        match direction {
            SplitDirection::Right => (
                Self {
                    cols: first,
                    ..self
                },
                Self {
                    x: self.x.saturating_add(gap),
                    cols: self.cols.saturating_sub(gap),
                    ..self
                },
            ),
            SplitDirection::Down => (
                Self {
                    rows: first,
                    ..self
                },
                Self {
                    y: self.y.saturating_add(gap),
                    rows: self.rows.saturating_sub(gap),
                    ..self
                },
            ),
        }
    }

    /// How much of a border `other` shares with `self`, if it lies across one in `direction`.
    fn shared(self, other: Self, direction: Direction) -> Option<u32> {
        let end = |start: u16, length: u16| u32::from(start) + u32::from(length);
        let overlap = |start: u16, length: u16, other_start: u16, other_length: u16| {
            let stop = end(start, length).min(end(other_start, other_length));
            stop.saturating_sub(u32::from(start.max(other_start)))
        };
        let (across, forward) = direction.across();
        let (near, far) = if forward {
            (self, other)
        } else {
            (other, self)
        };
        let (touching, shared) = match across {
            SplitDirection::Right => (
                end(near.x, near.cols) + 1 == u32::from(far.x),
                overlap(self.y, self.rows, other.y, other.rows),
            ),
            SplitDirection::Down => (
                end(near.y, near.rows) + 1 == u32::from(far.y),
                overlap(self.x, self.cols, other.x, other.cols),
            ),
        };
        (touching && shared > 0).then_some(shared)
    }
}

/// The length of a split's first side, or `None` if its two sides do not fit in `length`.
fn first_length(length: u16, share: u32, (first, second): (u16, u16)) -> Option<u16> {
    let room = length.checked_sub(1)?;
    let most = room.checked_sub(second)?;
    if first > most {
        return None;
    }
    let wanted = u64::from(room) * u64::from(share) / u64::from(WHOLE);
    Some(u16::try_from(wanted).unwrap_or(most).clamp(first, most))
}

/// A session: its tabs, and which one is active.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    tabs: Vec<Tab>,
    active: usize,
}

impl Session {
    /// The tabs, in order.
    #[must_use]
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// The active tab, or `None` if the session has no tabs.
    #[must_use]
    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// Adds a tab holding `pane` after the others and activates it.
    ///
    /// # Errors
    ///
    /// [`LayoutError::DuplicatePane`] if `pane` already exists.
    pub fn new_tab(&mut self, pane: PaneId) -> Result<(), LayoutError> {
        self.check_new(pane)?;
        self.tabs.push(Tab::new(pane));
        self.active = self.tabs.len().saturating_sub(1);
        Ok(())
    }

    /// Closes the tab at `index` and returns it, so the caller can close its panes.
    ///
    /// ```
    /// use pinfold_mux::Session;
    ///
    /// let mut session = Session::default();
    /// session.new_tab(1)?;
    /// let closed = session.close_tab(0)?;
    /// assert_eq!(closed.panes().collect::<Vec<_>>(), [1]);
    /// assert!(session.tabs().is_empty());
    /// # Ok::<(), pinfold_mux::LayoutError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnknownTab`] if there is no tab at `index`.
    pub fn close_tab(&mut self, index: usize) -> Result<Tab, LayoutError> {
        if index >= self.tabs.len() {
            return Err(LayoutError::UnknownTab(index));
        }
        let tab = self.tabs.remove(index);
        if index < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        Ok(tab)
    }

    /// Activates the tab at `index`.
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnknownTab`] if there is no tab at `index`.
    pub fn switch_tab(&mut self, index: usize) -> Result<(), LayoutError> {
        if index >= self.tabs.len() {
            return Err(LayoutError::UnknownTab(index));
        }
        self.active = index;
        Ok(())
    }

    /// Activates the next tab, or the previous one, wrapping at the ends.
    ///
    /// ```
    /// use pinfold_mux::Session;
    ///
    /// let mut session = Session::default();
    /// session.new_tab(1)?;
    /// session.new_tab(2)?;
    /// session.step_tab(true);
    /// assert_eq!(session.active(), 0);
    /// session.step_tab(false);
    /// assert_eq!(session.active(), 1);
    /// # Ok::<(), pinfold_mux::LayoutError>(())
    /// ```
    pub fn step_tab(&mut self, forward: bool) {
        let count = self.tabs.len().max(1);
        let step = if forward { 1 } else { count - 1 };
        self.active = (self.active + step) % count;
    }

    /// The index of the active tab.
    #[must_use]
    pub fn active(&self) -> usize {
        self.active
    }

    /// Focuses `pane`, if the active tab holds it.
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnknownPane`] if it does not.
    pub fn focus(&mut self, pane: PaneId) -> Result<(), LayoutError> {
        let tab = self.tabs.get_mut(self.active);
        let tab = tab
            .filter(|tab| tab.index_of(pane).is_some())
            .ok_or(LayoutError::UnknownPane(pane))?;
        tab.focus = pane;
        Ok(())
    }

    /// Focuses `pane` in whichever tab holds it, and leaves the active tab as it is.
    ///
    /// ```
    /// use pinfold_mux::{Session, SplitDirection};
    ///
    /// let mut session = Session::default();
    /// session.new_tab(1)?;
    /// session.split(1, 2, SplitDirection::Right)?;
    /// session.new_tab(3)?;
    /// session.focus_in_tab(1)?;
    /// assert_eq!((session.active(), session.tabs()[0].focused()), (1, 1));
    /// # Ok::<(), pinfold_mux::LayoutError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnknownPane`] if no tab holds it.
    pub fn focus_in_tab(&mut self, pane: PaneId) -> Result<(), LayoutError> {
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.index_of(pane).is_some());
        tab.ok_or(LayoutError::UnknownPane(pane))?.focus = pane;
        Ok(())
    }

    /// Splits `pane` in half and focuses `new` in the second half, in whichever tab holds
    /// `pane`. That tab stops zooming, so the new pane is seen in place.
    ///
    /// # Errors
    ///
    /// [`LayoutError::DuplicatePane`] if `new` exists, [`LayoutError::UnknownPane`] if `pane`
    /// does not.
    pub fn split(
        &mut self,
        pane: PaneId,
        new: PaneId,
        direction: SplitDirection,
    ) -> Result<(), LayoutError> {
        self.check_new(new)?;
        self.tabs
            .iter_mut()
            .find_map(|tab| tab.split(pane, new, direction))
            .ok_or(LayoutError::UnknownPane(pane))
    }

    /// Closes `pane`, and its sibling takes the space. Closing the last pane of a tab closes
    /// the tab. If it had focus, focus goes to the sibling's first pane.
    ///
    /// ```
    /// use pinfold_mux::{Session, SplitDirection};
    ///
    /// let mut session = Session::default();
    /// session.new_tab(1)?;
    /// session.split(1, 2, SplitDirection::Down)?;
    /// session.close_pane(2)?;
    /// assert_eq!(session.active_tab().map(|tab| tab.focused()), Some(1));
    /// session.close_pane(1)?;
    /// assert!(session.tabs().is_empty());
    /// # Ok::<(), pinfold_mux::LayoutError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnknownPane`] if `pane` does not exist.
    pub fn close_pane(&mut self, pane: PaneId) -> Result<(), LayoutError> {
        let holds = |tab: &Tab| tab.index_of(pane).is_some();
        let index = self
            .tabs
            .iter()
            .position(holds)
            .ok_or(LayoutError::UnknownPane(pane))?;
        if !self.tabs.get_mut(index).is_some_and(|tab| tab.close(pane)) {
            self.close_tab(index)?;
        }
        Ok(())
    }

    /// Moves focus in the active tab across the border in `direction`, and returns the pane
    /// that now has it, or `None` if there is no pane that way. If the tab is zoomed, the
    /// zoom moves with the focus.
    pub fn move_focus(&mut self, direction: Direction) -> Option<PaneId> {
        let tab = self.tabs.get_mut(self.active)?;
        tab.focus = tab.neighbour(direction)?;
        Some(tab.focus)
    }

    /// Moves the border of the nearest split around `pane` that runs across `direction`, by at
    /// least `cells` in a tab of `size`. The opposite call undoes it exactly, at any tab size.
    ///
    /// ```
    /// use pinfold_mux::{Direction, Session, Size, SplitDirection};
    ///
    /// let size = Size { cols: 80, rows: 24 };
    /// let mut session = Session::default();
    /// session.new_tab(1)?;
    /// session.split(1, 2, SplitDirection::Right)?;
    /// let before = session.clone();
    ///
    /// session.resize(1, Direction::Right, 7, size)?;
    /// session.resize(1, Direction::Left, 7, size)?;
    /// assert_eq!(session, before);
    /// # Ok::<(), pinfold_mux::LayoutError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`LayoutError::UnknownPane`] if `pane` does not exist, and
    /// [`LayoutError::CannotResize`] if no such split exists or its border cannot move.
    pub fn resize(
        &mut self,
        pane: PaneId,
        direction: Direction,
        cells: u16,
        size: Size,
    ) -> Result<(), LayoutError> {
        let holds = |tab: &&mut Tab| tab.index_of(pane).is_some();
        let tab = self
            .tabs
            .iter_mut()
            .find(holds)
            .ok_or(LayoutError::UnknownPane(pane))?;
        tab.resize(pane, direction, cells, size)
    }

    /// Toggles whether the focused pane of the active tab fills that tab.
    pub fn toggle_zoom(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.zoomed = !tab.zoomed;
        }
    }

    fn check_new(&self, pane: PaneId) -> Result<(), LayoutError> {
        if self.tabs.iter().any(|tab| tab.index_of(pane).is_some()) {
            return Err(LayoutError::DuplicatePane(pane));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    const WINDOW: Size = Size { cols: 80, rows: 24 };

    fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Placement {
        Placement::At(Rect { x, y, cols, rows })
    }

    fn panes(session: &Session, tab: usize) -> Vec<PaneId> {
        session
            .tabs()
            .get(tab)
            .map(|tab| tab.panes().collect())
            .unwrap_or_default()
    }

    fn layout(session: &Session, size: Size) -> Vec<(PaneId, Placement)> {
        session
            .active_tab()
            .map(|tab| tab.layout(size))
            .unwrap_or_default()
    }

    fn focused(session: &Session) -> Option<PaneId> {
        session.active_tab().map(Tab::focused)
    }

    /// A 2x2 grid of panes 1 and 2 above 3 and 4.
    fn grid() -> Result<Session, LayoutError> {
        let mut session = Session::default();
        session.new_tab(1)?;
        session.split(1, 3, SplitDirection::Down)?;
        session.split(1, 2, SplitDirection::Right)?;
        session.split(3, 4, SplitDirection::Right)?;
        Ok(session)
    }

    #[test]
    fn a_border_takes_one_cell() -> Result<(), LayoutError> {
        assert_eq!(
            layout(&grid()?, WINDOW),
            [
                (1, rect(0, 0, 39, 11)),
                (2, rect(40, 0, 40, 11)),
                (3, rect(0, 12, 39, 12)),
                (4, rect(40, 12, 40, 12)),
            ],
        );
        Ok(())
    }

    #[test]
    fn closing_a_pane_gives_its_sibling_the_space() -> Result<(), LayoutError> {
        for closed in [1, 2] {
            let mut session = Session::default();
            session.new_tab(1)?;
            session.split(1, 2, SplitDirection::Right)?;
            session.close_pane(closed)?;
            let left = 3 - closed;
            assert_eq!(layout(&session, WINDOW), [(left, rect(0, 0, 80, 24))]);
            assert_eq!(focused(&session), Some(left));
        }

        // A sibling that is a subtree keeps its own split.
        let mut session = grid()?;
        session.close_pane(1)?;
        assert_eq!(panes(&session, 0), [2, 3, 4]);
        assert_eq!(layout(&session, WINDOW)[0], (2, rect(0, 0, 80, 11)));
        Ok(())
    }

    #[test]
    fn focus_goes_to_the_first_pane_of_a_closed_panes_sibling() -> Result<(), LayoutError> {
        let mut session = Session::default();
        session.new_tab(1)?;
        session.split(1, 2, SplitDirection::Right)?;
        session.split(2, 3, SplitDirection::Down)?;
        session.split(1, 4, SplitDirection::Down)?;
        session.close_pane(4)?;
        assert_eq!(focused(&session), Some(1));
        session.close_pane(1)?;
        assert_eq!(focused(&session), Some(2));
        Ok(())
    }

    #[test]
    fn stepping_wraps_and_an_empty_session_stays_put() -> Result<(), LayoutError> {
        let mut session = Session::default();
        session.step_tab(true);
        session.step_tab(false);
        assert_eq!(session.active(), 0);
        for pane in 1..=3 {
            session.new_tab(pane)?;
        }
        let order: Vec<usize> = (0..4)
            .map(|_| {
                session.step_tab(true);
                session.active()
            })
            .collect();
        assert_eq!(order, [0, 1, 2, 0]);
        session.step_tab(false);
        assert_eq!(session.active(), 2);
        Ok(())
    }

    #[test]
    fn only_a_pane_of_the_active_tab_takes_focus() -> Result<(), LayoutError> {
        let mut session = grid()?;
        session.new_tab(9)?;
        session.switch_tab(0)?;
        session.focus(1)?;
        assert_eq!(focused(&session), Some(1));
        assert_eq!(session.focus(9), Err(LayoutError::UnknownPane(9)));
        assert_eq!(focused(&session), Some(1));
        Ok(())
    }

    #[test]
    fn closing_a_background_pane_keeps_the_focus() -> Result<(), LayoutError> {
        let mut session = grid()?;
        session.close_pane(1)?;
        assert_eq!(focused(&session), Some(4));
        Ok(())
    }

    #[test]
    fn closing_the_last_pane_closes_its_tab_and_keeps_the_active_one() -> Result<(), LayoutError> {
        let mut session = Session::default();
        session.new_tab(1)?;
        session.new_tab(2)?;
        session.new_tab(3)?;
        session.switch_tab(1)?;
        session.close_pane(1)?;
        assert_eq!(focused(&session), Some(2));
        session.close_tab(1)?;
        assert_eq!(focused(&session), Some(2));
        session.close_tab(0)?;
        assert_eq!(focused(&session), None);
        assert!(session.tabs().is_empty());
        Ok(())
    }

    #[test]
    fn a_refused_operation_changes_nothing() -> Result<(), LayoutError> {
        let mut session = grid()?;
        let before = session.clone();
        assert_eq!(session.new_tab(1), Err(LayoutError::DuplicatePane(1)));
        assert_eq!(
            session.split(1, 2, SplitDirection::Right),
            Err(LayoutError::DuplicatePane(2))
        );
        assert_eq!(
            session.split(9, 10, SplitDirection::Right),
            Err(LayoutError::UnknownPane(9))
        );
        assert_eq!(session.close_pane(9), Err(LayoutError::UnknownPane(9)));
        assert_eq!(session.close_tab(1).err(), Some(LayoutError::UnknownTab(1)));
        assert_eq!(session.switch_tab(1), Err(LayoutError::UnknownTab(1)));
        assert_eq!(
            session.resize(9, Direction::Left, 1, WINDOW),
            Err(LayoutError::UnknownPane(9))
        );
        assert_eq!(session, before);
        Ok(())
    }

    #[test]
    fn focus_moves_to_the_pane_across_the_border() -> Result<(), LayoutError> {
        let mut session = grid()?;
        assert_eq!(focused(&session), Some(4));
        assert_eq!(session.move_focus(Direction::Right), None);
        assert_eq!(session.move_focus(Direction::Down), None);
        assert_eq!(session.move_focus(Direction::Left), Some(3));
        assert_eq!(session.move_focus(Direction::Up), Some(1));
        assert_eq!(session.move_focus(Direction::Right), Some(2));
        assert_eq!(session.move_focus(Direction::Down), Some(4));
        Ok(())
    }

    #[test]
    fn focus_prefers_the_pane_sharing_most_of_the_border() -> Result<(), LayoutError> {
        let mut session = Session::default();
        session.new_tab(1)?;
        session.split(1, 2, SplitDirection::Right)?;
        session.split(2, 3, SplitDirection::Down)?;
        session.resize(3, Direction::Down, 6, WINDOW)?;
        assert_eq!(session.move_focus(Direction::Left), Some(1));
        assert_eq!(session.move_focus(Direction::Right), Some(2));
        Ok(())
    }

    #[test]
    fn zoom_fills_the_tab_until_toggled_or_split() -> Result<(), LayoutError> {
        let mut session = grid()?;
        session.toggle_zoom();
        assert_eq!(
            layout(&session, WINDOW),
            [
                (1, Placement::Hidden),
                (2, Placement::Hidden),
                (3, Placement::Hidden),
                (4, rect(0, 0, 80, 24)),
            ],
        );
        session.move_focus(Direction::Up);
        assert_eq!(layout(&session, WINDOW)[1], (2, rect(0, 0, 80, 24)));

        let unzoomed = {
            let mut copy = session.clone();
            copy.toggle_zoom();
            layout(&copy, WINDOW)
        };
        session.split(2, 5, SplitDirection::Down)?;
        assert_eq!(session.active_tab().map(Tab::zoomed), Some(false));
        session.close_pane(5)?;
        assert_eq!(layout(&session, WINDOW), unzoomed);
        Ok(())
    }

    #[test]
    fn what_does_not_fit_is_reported() -> Result<(), LayoutError> {
        let session = grid()?;
        assert_eq!(
            layout(&session, Size { cols: 80, rows: 2 }),
            [
                (1, rect(0, 0, 39, 2)),
                (2, rect(40, 0, 40, 2)),
                (3, Placement::TooSmall),
                (4, Placement::TooSmall),
            ],
        );
        let none = [1, 2, 3, 4].map(|pane| (pane, Placement::TooSmall));
        assert_eq!(layout(&session, Size { cols: 0, rows: 0 }), none);
        assert_eq!(layout(&session, Size { cols: 1, rows: 24 }), none);
        Ok(())
    }

    #[test]
    fn a_narrow_side_is_held_at_its_minimum() -> Result<(), LayoutError> {
        let mut session = Session::default();
        session.new_tab(1)?;
        session.split(1, 2, SplitDirection::Right)?;
        session.split(2, 3, SplitDirection::Right)?;
        // The second side needs 5 columns, more than half of 7.
        assert_eq!(
            layout(&session, Size { cols: 8, rows: 1 }),
            [
                (1, rect(0, 0, 2, 1)),
                (2, rect(3, 0, 2, 1)),
                (3, rect(6, 0, 2, 1))
            ],
        );
        Ok(())
    }

    #[test]
    fn resizing_moves_the_nearest_border_along_the_axis() -> Result<(), LayoutError> {
        let mut session = grid()?;
        session.resize(1, Direction::Right, 5, WINDOW)?;
        session.resize(1, Direction::Down, 2, WINDOW)?;
        let placed = layout(&session, WINDOW);
        assert_eq!(placed[0], (1, rect(0, 0, 44, 13)));
        assert_eq!(placed[2], (3, rect(0, 14, 39, 10)));
        Ok(())
    }

    #[test]
    fn a_share_survives_the_window_shrinking() -> Result<(), LayoutError> {
        let mut session = Session::default();
        session.new_tab(1)?;
        session.split(1, 2, SplitDirection::Right)?;
        session.resize(1, Direction::Right, 30, WINDOW)?;
        let before = layout(&session, WINDOW);
        assert_eq!(
            layout(&session, Size { cols: 6, rows: 24 }),
            [(1, rect(0, 0, 3, 24)), (2, rect(4, 0, 2, 24))],
        );
        assert_eq!(layout(&session, WINDOW), before);
        Ok(())
    }

    #[test]
    fn a_border_stops_at_a_panes_minimum_without_banking_presses() -> Result<(), LayoutError> {
        let mut session = Session::default();
        session.new_tab(1)?;
        session.split(1, 2, SplitDirection::Right)?;
        let mut moves = 0;
        while session.resize(1, Direction::Right, 1, WINDOW).is_ok() {
            moves += 1;
        }
        assert_eq!(layout(&session, WINDOW)[1], (2, rect(78, 0, 2, 24)));
        assert_eq!(moves, 38);
        assert_eq!(
            session.resize(1, Direction::Right, 1, WINDOW),
            Err(LayoutError::CannotResize)
        );
        session.resize(1, Direction::Left, 1, WINDOW)?;
        assert_eq!(layout(&session, WINDOW)[1], (2, rect(77, 0, 3, 24)));
        Ok(())
    }

    #[test]
    fn a_resize_without_a_border_is_refused() -> Result<(), LayoutError> {
        let mut session = Session::default();
        session.new_tab(1)?;
        session.split(1, 2, SplitDirection::Right)?;
        assert_eq!(
            session.resize(1, Direction::Down, 1, WINDOW),
            Err(LayoutError::CannotResize)
        );
        assert_eq!(
            session.resize(1, Direction::Right, u16::MAX, WINDOW),
            Err(LayoutError::CannotResize)
        );
        assert_eq!(
            session.resize(1, Direction::Right, 1, Size { cols: 0, rows: 0 }),
            Err(LayoutError::CannotResize)
        );
        Ok(())
    }

    #[test]
    fn a_very_deep_tree_is_walked_without_recursion() -> Result<(), LayoutError> {
        let mut session = Session::default();
        session.new_tab(0)?;
        for pane in 0..5_000 {
            session.split(pane, pane + 1, SplitDirection::Right)?;
        }
        let placed = layout(&session, WINDOW);
        assert_eq!(placed.len(), 5_001);
        session.resize(0, Direction::Right, 1, CANVAS)?;
        assert_eq!(session.move_focus(Direction::Left), Some(4_999));
        Ok(())
    }

    /// `size_of` arithmetic for what the session holds, by capacity rather than by length, so
    /// it includes what `Vec` over-allocates. Allocator headers are not in it.
    #[test]
    fn eight_panes_in_one_tab_take_288_bytes() -> Result<(), LayoutError> {
        let mut session = Session::default();
        session.new_tab(0)?;
        for pane in 1..8 {
            session.split(pane - 1, pane, SplitDirection::Right)?;
        }
        let tab = session.tabs.first().ok_or(LayoutError::UnknownTab(0))?;
        assert_eq!(tab.nodes.len(), 15);
        assert_eq!(size_of::<Node>(), 8);

        let bytes = size_of::<Session>()
            + session.tabs.capacity() * size_of::<Tab>()
            + tab.nodes.capacity() * size_of::<Node>();
        eprintln!(
            "8 panes: {bytes} bytes = Session {} + {} tab slots x {} + {} node slots x {}",
            size_of::<Session>(),
            session.tabs.capacity(),
            size_of::<Tab>(),
            tab.nodes.capacity(),
            size_of::<Node>(),
        );
        assert_eq!(bytes, 288);
        Ok(())
    }

    #[derive(Clone, Copy, Debug)]
    enum Op {
        NewTab(PaneId),
        CloseTab(usize),
        SwitchTab(usize),
        Split(PaneId, PaneId, SplitDirection),
        ClosePane(PaneId),
        MoveFocus(Direction),
        Resize(PaneId, Direction, u16),
        ToggleZoom,
    }

    fn direction() -> impl Strategy<Value = Direction> {
        prop_oneof![
            Just(Direction::Left),
            Just(Direction::Right),
            Just(Direction::Up),
            Just(Direction::Down),
        ]
    }

    fn split_direction() -> impl Strategy<Value = SplitDirection> {
        prop_oneof![Just(SplitDirection::Right), Just(SplitDirection::Down)]
    }

    fn op() -> impl Strategy<Value = Op> {
        let pane = || 0..24_u32;
        prop_oneof![
            2 => pane().prop_map(Op::NewTab),
            1 => (0..4_usize).prop_map(Op::CloseTab),
            1 => (0..4_usize).prop_map(Op::SwitchTab),
            6 => (pane(), pane(), split_direction()).prop_map(|(a, b, c)| Op::Split(a, b, c)),
            3 => pane().prop_map(Op::ClosePane),
            2 => direction().prop_map(Op::MoveFocus),
            3 => (pane(), direction(), prop_oneof![0..9_u16, any::<u16>()])
                .prop_map(|(a, b, c)| Op::Resize(a, b, c)),
            1 => Just(Op::ToggleZoom),
        ]
    }

    fn size() -> impl Strategy<Value = Size> {
        (
            prop_oneof![0..200_u16, any::<u16>()],
            prop_oneof![0..60_u16, any::<u16>()],
        )
            .prop_map(|(cols, rows)| Size { cols, rows })
    }

    /// Runs `op`, and keeps `live` the set of panes a correct model holds.
    fn apply(session: &mut Session, live: &mut BTreeSet<PaneId>, op: Op, size: Size) {
        match op {
            Op::NewTab(pane) => {
                if session.new_tab(pane).is_ok() {
                    live.insert(pane);
                }
            }
            Op::CloseTab(index) => {
                if let Ok(tab) = session.close_tab(index) {
                    live.retain(|pane| !tab.panes().any(|closed| closed == *pane));
                }
            }
            Op::SwitchTab(index) => {
                let _ = session.switch_tab(index);
            }
            Op::Split(pane, new, direction) => {
                if session.split(pane, new, direction).is_ok() {
                    live.insert(new);
                }
            }
            Op::ClosePane(pane) => {
                if session.close_pane(pane).is_ok() {
                    live.remove(&pane);
                }
            }
            Op::MoveFocus(direction) => {
                let _ = session.move_focus(direction);
            }
            Op::Resize(pane, direction, cells) => {
                let _ = session.resize(pane, direction, cells, size);
            }
            Op::ToggleZoom => session.toggle_zoom(),
        }
    }

    fn opposite(direction: Direction) -> Direction {
        match direction {
            Direction::Left => Direction::Right,
            Direction::Right => Direction::Left,
            Direction::Up => Direction::Down,
            Direction::Down => Direction::Up,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn no_operation_sequence_loses_or_overlaps_a_pane(
            ops in proptest::collection::vec(op(), 0..80),
            sizes in proptest::collection::vec(size(), 1..8),
        ) {
            let mut session = Session::default();
            let mut live = BTreeSet::new();
            for (step, op) in ops.into_iter().enumerate() {
                let tab_size = sizes[step % sizes.len()];
                apply(&mut session, &mut live, op, tab_size);

                let mut held: Vec<PaneId> = session.tabs().iter().flat_map(Tab::panes).collect();
                held.sort_unstable();
                prop_assert!(held.iter().copied().eq(live.iter().copied()));
                prop_assert_eq!(session.active_tab().is_some(), !session.tabs().is_empty());

                for tab in session.tabs() {
                    prop_assert!(tab.panes().any(|pane| pane == tab.focused()));

                    let placed = tab.layout(tab_size);
                    prop_assert!(placed.iter().map(|(pane, _)| *pane).eq(tab.panes()));
                    let rects: Vec<Rect> = placed
                        .iter()
                        .filter_map(|(_, place)| match *place {
                            Placement::At(rect) => Some(rect),
                            _ => None,
                        })
                        .collect();
                    for (index, a) in rects.iter().enumerate() {
                        prop_assert!(a.cols >= MIN_PANE.cols && a.rows >= MIN_PANE.rows);
                        prop_assert!(u32::from(a.x) + u32::from(a.cols) <= u32::from(tab_size.cols));
                        prop_assert!(u32::from(a.y) + u32::from(a.rows) <= u32::from(tab_size.rows));
                        for b in &rects[index + 1..] {
                            let apart = a.x + a.cols <= b.x
                                || b.x + b.cols <= a.x
                                || a.y + a.rows <= b.y
                                || b.y + b.rows <= a.y;
                            prop_assert!(apart, "{a:?} overlaps {b:?}");
                        }
                    }
                    if !tab.zoomed() && rects.len() == placed.len() {
                        // With every pane placed, panes and borders cover the whole tab, and
                        // each of the panes-1 splits costs at most one line of cells.
                        let area = |rect: &Rect| u64::from(rect.cols) * u64::from(rect.rows);
                        let covered: u64 = rects.iter().map(area).sum();
                        let whole = u64::from(tab_size.cols) * u64::from(tab_size.rows);
                        let line = u64::from(tab_size.cols.max(tab_size.rows));
                        prop_assert!(covered <= whole);
                        prop_assert!(covered + line * (rects.len() as u64 - 1) >= whole);
                    }
                }
            }
        }

        #[test]
        fn a_resize_is_undone_exactly(
            ops in proptest::collection::vec(op(), 0..60),
            tab_size in size(),
            direction in direction(),
            cells in 0..40_u16,
        ) {
            let mut session = Session::default();
            let mut live = BTreeSet::new();
            for op in ops {
                apply(&mut session, &mut live, op, tab_size);
            }
            for pane in live {
                let before = session.clone();
                if session.resize(pane, direction, cells, tab_size).is_ok() {
                    prop_assert_ne!(&session, &before);
                    prop_assert!(session.resize(pane, opposite(direction), cells, tab_size).is_ok());
                    prop_assert_eq!(&session, &before);
                }
            }
        }
    }
}

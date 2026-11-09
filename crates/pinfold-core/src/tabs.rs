//! The session: which panes make up which tabs, and where the active tab puts them.
//!
//! The core owns this because it is what must outlive a window. A window sends commands and
//! draws the layout it gets back, so a new window finds the same tabs and splits. The caps turn
//! into a [`Refusal`], never an error that ends the link. [`MAX_TABS`] and [`MAX_PANES`] say why
//! they are what they are.
//!
//! Only the active tab is laid out. A hidden tab's programs keep their old size until the tab is
//! shown, so a window resize costs no wake-ups in panes nobody can see.

use std::process::Command;

use pinfold_mux::{Direction, PaneId, Placement, Size, SplitDirection};
use pinfold_proto::link::{MAX_PANES, MAX_TABS, Rect, Refusal, Request, Side, SplitWay};

use crate::io::Reactor;

/// The size of a pane opened before any window has said how much room it has.
pub(crate) const FIRST_SIZE: Size = Size { cols: 80, rows: 24 };

fn direction(side: Side) -> Direction {
    match side {
        Side::Left => Direction::Left,
        Side::Right => Direction::Right,
        Side::Up => Direction::Up,
        Side::Down => Direction::Down,
    }
}

impl Reactor {
    /// Starts `command` as a new tab after the others, activates it and returns its pane.
    ///
    /// # Errors
    ///
    /// Returns why not, past a cap or if the program cannot start, and changes nothing.
    pub fn open_tab(
        &mut self,
        command: Command,
        rows: u16,
        columns: u16,
    ) -> Result<usize, Refusal> {
        self.open_tab_with(command, rows, columns, true)
    }

    /// [`Reactor::open_tab`], which leaves the active tab as it is unless `focus`. A pane the
    /// API spawns must not take the keyboard from the person.
    pub(crate) fn open_tab_with(
        &mut self,
        command: Command,
        rows: u16,
        columns: u16,
        focus: bool,
    ) -> Result<usize, Refusal> {
        if self.session.tabs().len() >= MAX_TABS {
            return Err(Refusal::Tabs);
        }
        self.check_pane_room()?;
        let pane = self
            .spawn(command, rows, columns)
            .map_err(|_| Refusal::Spawn)?;
        // A number that was never used cannot be a duplicate.
        let active = self.session.active();
        let _ = self.session.new_tab(pane as PaneId);
        if !focus {
            let _ = self.session.switch_tab(active);
        }
        self.lay_out();
        Ok(pane)
    }

    /// Splits `pane` in half, starts `command` in the second half and focuses it.
    ///
    /// # Errors
    ///
    /// Returns why not, and changes nothing: a cap, a program that cannot start, or a tab with
    /// no room for the new pane, which also covers a `pane` that does not exist.
    pub fn split(
        &mut self,
        pane: usize,
        command: Command,
        way: SplitWay,
    ) -> Result<usize, Refusal> {
        self.split_with(pane, command, way, true)
    }

    /// [`Reactor::split`], which leaves focus where it was unless `focus`.
    pub(crate) fn split_with(
        &mut self,
        pane: usize,
        command: Command,
        way: SplitWay,
        focus: bool,
    ) -> Result<usize, Refusal> {
        self.check_pane_room()?;
        let new = self.next_pane as PaneId;
        let direction = match way {
            SplitWay::Right => SplitDirection::Right,
            SplitWay::Down => SplitDirection::Down,
        };
        let before = self.session.clone();
        if self.session.split(pane as PaneId, new, direction).is_err() {
            return Err(Refusal::Room);
        }
        let holds = |tab: &&pinfold_mux::Tab| tab.panes().any(|held| held == pane as PaneId);
        let kept = before
            .tabs()
            .iter()
            .find(holds)
            .map(pinfold_mux::Tab::focused);
        if let Some(kept) = kept.filter(|_| !focus) {
            let _ = self.session.focus_in_tab(kept);
        }
        let spawned = self.place_of(new).and_then(|size| {
            self.spawn(command, size.rows, size.cols)
                .map_err(|_| Refusal::Spawn)
        });
        match spawned {
            Ok(pane) => {
                self.lay_out();
                Ok(pane)
            }
            Err(reason) => {
                self.session = before;
                Err(reason)
            }
        }
    }

    /// Closes a pane, which ends its program and frees its terminal and history. Its tab goes
    /// with its last pane. Returns false if no tab holds it.
    // shortcut: dropping a pane waits for its killed child on the reactor's thread, which only a
    // process stuck in the kernel makes long. Reap on a later turn if one ever stalls the core.
    pub fn close_pane(&mut self, pane: usize) -> bool {
        if self.session.close_pane(pane as PaneId).is_err() {
            return false;
        }
        self.drop_pane(pane);
        self.lay_out();
        true
    }

    /// Carries out a window's command, and does nothing for the other requests. A pane or tab
    /// that does not exist also does nothing, as a window can be one step behind.
    pub(crate) fn command(&mut self, request: Request<'_>) -> Result<(), Refusal> {
        let changed = match request {
            Request::Resize { rows, columns } => {
                let cols = columns;
                self.area = Some(Size { cols, rows });
                true
            }
            Request::NewTab => {
                let size = self.area.unwrap_or(FIRST_SIZE);
                let focused = self.session.active_tab().map(pinfold_mux::Tab::focused);
                let command = self.shell_from(focused.map(|pane| pane as usize));
                self.open_tab(command, size.rows, size.cols)?;
                false
            }
            Request::Split { pane, way } => {
                let command = self.shell_from(Some(pane as usize));
                self.split(pane as usize, command, way)?;
                false
            }
            Request::ClosePane { pane } => {
                self.close_pane(pane as usize);
                false
            }
            Request::SwitchTab { index } => self.session.switch_tab(index.into()).is_ok(),
            Request::StepTab { forward } => {
                self.session.step_tab(forward);
                true
            }
            Request::Focus { pane } => self.session.focus(pane).is_ok(),
            Request::MoveFocus { side } => self.session.move_focus(direction(side)).is_some(),
            Request::MoveBorder { side, cells } => {
                let focused = self.session.active_tab().map(pinfold_mux::Tab::focused);
                let moved = focused
                    .zip(self.area)
                    .map(|(pane, area)| self.session.resize(pane, direction(side), cells, area));
                matches!(moved, Some(Ok(())))
            }
            Request::Zoom => {
                self.session.toggle_zoom();
                true
            }
            _ => false,
        };
        if changed {
            self.lay_out();
        }
        Ok(())
    }

    /// Returns the panes the active tab shows, and where, once a window has given a size.
    pub(crate) fn shown(&self) -> Vec<(u32, Rect)> {
        let (Some(tab), Some(area)) = (self.session.active_tab(), self.area) else {
            return Vec::new();
        };
        let place = |(pane, placement)| match placement {
            Placement::At(rect) => Some((
                pane,
                Rect {
                    x: rect.x,
                    y: rect.y,
                    columns: rect.cols,
                    rows: rect.rows,
                },
            )),
            Placement::Hidden | Placement::TooSmall => None,
        };
        tab.layout(area).into_iter().filter_map(place).collect()
    }

    /// Sizes each pane the active tab shows, and marks the layout new for the window.
    pub(crate) fn lay_out(&mut self) {
        self.revision += 1;
        for (pane, rect) in self.shown() {
            let pane = pane as usize;
            let size = self.engine(pane).map(|engine| engine.size());
            if size != Some((rect.rows, rect.columns)) {
                // A PTY that refuses a size is the pane's problem, not a reason to drop the link.
                let _ = self.resize(pane, rect.rows, rect.columns);
            }
        }
    }

    fn check_pane_room(&self) -> Result<(), Refusal> {
        if self.pane_count() >= MAX_PANES || self.next_pane > u32::MAX as usize {
            return Err(Refusal::Panes);
        }
        Ok(())
    }

    /// Returns the size `pane` gets in the tab that holds it.
    fn place_of(&self, pane: PaneId) -> Result<Size, Refusal> {
        let Some(area) = self.area else {
            return Ok(FIRST_SIZE);
        };
        let holds = |tab: &&pinfold_mux::Tab| tab.panes().any(|held| held == pane);
        let tab = self.session.tabs().iter().find(holds);
        let placed = tab.into_iter().flat_map(|tab| tab.layout(area));
        placed
            .into_iter()
            .find_map(|(held, placement)| match placement {
                Placement::At(rect) if held == pane => Some(Size {
                    cols: rect.cols,
                    rows: rect.rows,
                }),
                _ => None,
            })
            .ok_or(Refusal::Room)
    }
}

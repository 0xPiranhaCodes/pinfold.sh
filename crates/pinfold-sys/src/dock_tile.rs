//! The Dock tile's progress bar on macOS (ADR-0051 §9): the app's icon with a bar across its
//! foot. The views are built when progress first shows and dropped when it goes, so the tile
//! keeps no view or image while none shows, and the tile is drawn only when told.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSImageView, NSProgressIndicator, NSProgressIndicatorStyle};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// What the tile shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The plain icon.
    Hidden,
    /// A bar this many percent full, at most 100.
    Percent(u8),
    /// A bar with no percent.
    Indeterminate,
}

/// The Dock tile, and its views while it shows progress.
pub struct DockTile {
    mtm: MainThreadMarker,
    views: Option<(Retained<NSImageView>, Retained<NSProgressIndicator>)>,
}

impl DockTile {
    /// The app's tile on the main thread, and nothing off it.
    #[must_use]
    pub fn new() -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        Some(Self { mtm, views: None })
    }

    /// Shows `progress` and draws the tile once. Call it on a change only.
    pub fn show(&mut self, progress: Progress) {
        let tile = NSApplication::sharedApplication(self.mtm).dockTile();
        if progress == Progress::Hidden {
            if self.views.take().is_some() {
                tile.setContentView(None);
                tile.display();
            }
            return;
        }
        let mtm = self.mtm;
        let (_, bar) = self.views.get_or_insert_with(|| {
            let app = NSApplication::sharedApplication(mtm);
            let size = tile.size();
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), size);
            let icon = NSImageView::initWithFrame(NSImageView::alloc(mtm), frame);
            icon.setImage(app.applicationIconImage().as_deref());
            let foot = NSRect::new(
                NSPoint::new(size.width * 0.1, size.height * 0.06),
                NSSize::new(size.width * 0.8, size.height * 0.14),
            );
            let bar = NSProgressIndicator::initWithFrame(NSProgressIndicator::alloc(mtm), foot);
            bar.setStyle(NSProgressIndicatorStyle::Bar);
            bar.setMinValue(0.0);
            bar.setMaxValue(100.0);
            icon.addSubview(&bar);
            tile.setContentView(Some(&icon));
            (icon, bar)
        });
        match progress {
            Progress::Percent(percent) => {
                bar.setIndeterminate(false);
                bar.setDoubleValue(f64::from(percent.min(100)));
            }
            _ => bar.setIndeterminate(true),
        }
        tile.display();
    }
}

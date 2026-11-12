//! Where the last frame drew what a click can reach: `draw` notes each tab, row and option with its area, the loop
//! finds the one under the pointer and the reducer (`app`) decides what clicking it does. It decides nothing itself.

use ratatui::layout::{Position, Rect};

/// A list a row of which can be clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum List {
    Attention,
    Slots,
    Queue,
    Repositories,
    Latest,
    History,
    Records,
    Insights,
    Sizing,
    Caches,
    Checks,
    Log,
}

/// What a click lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// The `n`th tab, from 0.
    Tab(usize),
    /// The header's count of what needs attention.
    Attention,
    /// Row `n` of a list, from 0.
    Row(List, usize),
    /// Option `n` of the list open over the view.
    Choice(usize),
}

/// One frame's clickable areas, the last drawn on top.
#[derive(Debug, Default)]
pub(crate) struct Hits {
    areas: Vec<Hit>,
}

#[derive(Debug)]
struct Hit {
    area: Rect,
    target: Target,
}

impl Hits {
    /// Notes `area` as `target`.
    pub(crate) fn add(&mut self, area: Rect, target: Target) {
        if area.width > 0 && area.height > 0 {
            self.areas.push(Hit { area, target });
        }
    }

    /// Notes each row of `area` from `first` as row `first + n` of `list`, for `count` rows.
    pub(crate) fn rows(&mut self, area: Rect, list: List, first: usize, count: usize) {
        for (offset, y) in (area.y..area.bottom()).enumerate().take(count) {
            self.add(
                Rect {
                    y,
                    height: 1,
                    ..area
                },
                Target::Row(list, first + offset),
            );
        }
    }

    /// Forgets the areas under `area`, as when something is drawn over them.
    pub(crate) fn cover(&mut self, area: Rect) {
        self.areas.retain(|hit| !hit.area.intersects(area));
    }

    /// What is at column `x`, row `y`.
    pub(crate) fn at(&self, x: u16, y: u16) -> Option<Target> {
        self.areas
            .iter()
            .rev()
            .find(|hit| hit.area.contains(Position { x, y }))
            .map(|hit| hit.target)
    }
}

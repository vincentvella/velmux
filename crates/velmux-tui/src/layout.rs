//! Binary split tree of pane ids, one per location (base or slot).

use ratatui::layout::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// Side by side, divided by a vertical line.
    Right,
    /// Stacked, divided by a horizontal line.
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toward {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Layout {
    Leaf(u64),
    Split { dir: Dir, ratio: f32, a: Box<Layout>, b: Box<Layout> },
}

/// A divider line between two halves of a split.
pub struct Divider {
    pub rect: Rect,
    pub dir: Dir,
}

const MIN_RATIO: f32 = 0.1;

impl Layout {
    pub fn leaves(&self) -> Vec<u64> {
        match self {
            Layout::Leaf(id) => vec![*id],
            Layout::Split { a, b, .. } => {
                let mut v = a.leaves();
                v.extend(b.leaves());
                v
            }
        }
    }

    pub fn contains(&self, id: u64) -> bool {
        self.leaves().contains(&id)
    }

    /// Split the leaf `target`, placing `new` after it (right or below).
    pub fn split(&mut self, target: u64, new: u64, dir: Dir) -> bool {
        match self {
            Layout::Leaf(id) if *id == target => {
                *self = Layout::Split {
                    dir,
                    ratio: 0.5,
                    a: Box::new(Layout::Leaf(target)),
                    b: Box::new(Layout::Leaf(new)),
                };
                true
            }
            Layout::Leaf(_) => false,
            Layout::Split { a, b, .. } => a.split(target, new, dir) || b.split(target, new, dir),
        }
    }

    /// Remove a leaf, collapsing its parent into the sibling. `None` if the tree empties.
    pub fn remove(self, id: u64) -> Option<Layout> {
        match self {
            Layout::Leaf(x) if x == id => None,
            leaf @ Layout::Leaf(_) => Some(leaf),
            Layout::Split { dir, ratio, a, b } => match (a.remove(id), b.remove(id)) {
                (Some(a), Some(b)) => Some(Layout::Split { dir, ratio, a: Box::new(a), b: Box::new(b) }),
                (Some(one), None) | (None, Some(one)) => Some(one),
                (None, None) => None,
            },
        }
    }

    /// Move the divider of the nearest `dir` split containing `id` by `delta`.
    pub fn nudge(&mut self, id: u64, dir: Dir, delta: f32) -> bool {
        let Layout::Split { dir: d, ratio, a, b } = self else { return false };
        let inner = if a.contains(id) {
            a
        } else if b.contains(id) {
            b
        } else {
            return false;
        };
        if inner.nudge(id, dir, delta) {
            return true;
        }
        if *d == dir {
            *ratio = (*ratio + delta).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
            return true;
        }
        false
    }

    /// Lay the tree out in `area`: each pane's rect plus the dividers between them.
    pub fn rects(&self, area: Rect) -> (Vec<(u64, Rect)>, Vec<Divider>) {
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        self.place(area, &mut panes, &mut dividers);
        (panes, dividers)
    }

    fn place(&self, area: Rect, panes: &mut Vec<(u64, Rect)>, dividers: &mut Vec<Divider>) {
        match self {
            Layout::Leaf(id) => panes.push((*id, area)),
            Layout::Split { dir, ratio, a, b } => {
                let span = match dir {
                    Dir::Right => area.width,
                    Dir::Down => area.height,
                };
                if span < 3 {
                    // Too small to divide: give it all to the first half.
                    a.place(area, panes, dividers);
                    for id in b.leaves() {
                        panes.push((id, Rect::new(area.x, area.y, 0, 0)));
                    }
                    return;
                }
                let first = ((span - 1) as f32 * ratio).round().clamp(1.0, (span - 2) as f32) as u16;
                let rest = span - 1 - first;
                let (ra, div, rb) = match dir {
                    Dir::Right => (
                        Rect::new(area.x, area.y, first, area.height),
                        Rect::new(area.x + first, area.y, 1, area.height),
                        Rect::new(area.x + first + 1, area.y, rest, area.height),
                    ),
                    Dir::Down => (
                        Rect::new(area.x, area.y, area.width, first),
                        Rect::new(area.x, area.y + first, area.width, 1),
                        Rect::new(area.x, area.y + first + 1, area.width, rest),
                    ),
                };
                a.place(ra, panes, dividers);
                dividers.push(Divider { rect: div, dir: *dir });
                b.place(rb, panes, dividers);
            }
        }
    }
}

/// The pane nearest to `from` in direction `toward`, among `rects`.
pub fn neighbor(rects: &[(u64, Rect)], from: u64, toward: Toward) -> Option<u64> {
    let f = rects.iter().find(|(id, _)| *id == from)?.1;
    let overlaps = |a0: u16, a1: u16, b0: u16, b1: u16| a0 < b1 && b0 < a1;
    rects
        .iter()
        .filter(|(id, r)| *id != from && r.width > 0 && r.height > 0)
        .filter_map(|(id, r)| {
            let dist = match toward {
                Toward::Left if r.right() <= f.x && overlaps(r.y, r.bottom(), f.y, f.bottom()) => f.x - r.right(),
                Toward::Right if r.x >= f.right() && overlaps(r.y, r.bottom(), f.y, f.bottom()) => r.x - f.right(),
                Toward::Up if r.bottom() <= f.y && overlaps(r.x, r.right(), f.x, f.right()) => f.y - r.bottom(),
                Toward::Down if r.y >= f.bottom() && overlaps(r.x, r.right(), f.x, f.right()) => r.y - f.bottom(),
                _ => return None,
            };
            Some((dist, *id))
        })
        .min()
        .map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 101, 41)
    }

    #[test]
    fn split_and_rects_tile_the_area() {
        let mut l = Layout::Leaf(1);
        assert!(l.split(1, 2, Dir::Right));
        assert!(l.split(2, 3, Dir::Down));
        let (panes, dividers) = l.rects(area());
        assert_eq!(l.leaves(), vec![1, 2, 3]);
        assert_eq!(panes[0], (1, Rect::new(0, 0, 50, 41)));
        assert_eq!(panes[1], (2, Rect::new(51, 0, 50, 20)));
        assert_eq!(panes[2], (3, Rect::new(51, 21, 50, 20)));
        assert_eq!(dividers.len(), 2);
        let covered: u32 = panes.iter().map(|(_, r)| r.area()).sum::<u32>()
            + dividers.iter().map(|d| d.rect.area()).sum::<u32>();
        assert_eq!(covered, area().area());
    }

    #[test]
    fn remove_collapses_into_sibling() {
        let mut l = Layout::Leaf(1);
        l.split(1, 2, Dir::Right);
        l.split(2, 3, Dir::Down);
        let l = l.remove(2).unwrap();
        assert_eq!(l.leaves(), vec![1, 3]);
        let l = l.remove(1).unwrap();
        assert_eq!(l, Layout::Leaf(3));
        assert_eq!(l.remove(3), None);
    }

    #[test]
    fn neighbors_follow_geometry() {
        let mut l = Layout::Leaf(1);
        l.split(1, 2, Dir::Right);
        l.split(2, 3, Dir::Down);
        let (rects, _) = l.rects(area());
        assert_eq!(neighbor(&rects, 1, Toward::Right), Some(2));
        assert_eq!(neighbor(&rects, 3, Toward::Up), Some(2));
        assert_eq!(neighbor(&rects, 3, Toward::Left), Some(1));
        assert_eq!(neighbor(&rects, 1, Toward::Left), None);
    }

    #[test]
    fn nudge_moves_nearest_matching_divider() {
        let mut l = Layout::Leaf(1);
        l.split(1, 2, Dir::Right);
        l.split(2, 3, Dir::Down);
        assert!(l.nudge(3, Dir::Right, 0.1));
        assert!(l.nudge(3, Dir::Down, -0.2));
        let Layout::Split { ratio, b, .. } = &l else { panic!() };
        assert!((ratio - 0.6).abs() < 1e-6);
        let Layout::Split { ratio, .. } = b.as_ref() else { panic!() };
        assert!((ratio - 0.3).abs() < 1e-6);
        assert!(!Layout::Leaf(1).nudge(1, Dir::Right, 0.1));
    }
}

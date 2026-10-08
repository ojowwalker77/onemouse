//! Screen geometry. The secondary's displays are placed in the Mac's global
//! coordinate space (points, origin top-left of the main display, y down) as
//! one block, the way System Settings → Displays arranges monitors. The
//! cursor crosses wherever the two touch.
//!
//! The block's top-left corner is the *arrangement origin*. It is always
//! snapped so the block touches a Mac display without overlapping any.

use onemouse_protocol::Display;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    fn distance_sq(self, other: Point) -> f64 {
        (self.x - other.x).powi(2) + (self.y - other.y).powi(2)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn from_display(d: &Display) -> Self {
        Self::new(d.x.into(), d.y.into(), d.width.into(), d.height.into())
    }

    pub fn max_x(&self) -> f64 {
        self.x + self.width
    }

    pub fn max_y(&self) -> f64 {
        self.y + self.height
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.max_x() && p.y >= self.y && p.y < self.max_y()
    }

    /// Whether the two share any area (touching edges don't count).
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.max_x()
            && other.x < self.max_x()
            && self.y < other.max_y()
            && other.y < self.max_y()
    }

    /// Nearest point inside, keeping a whole pixel/point from the far edges.
    pub fn clamp(&self, p: Point) -> Point {
        Point::new(
            p.x.clamp(self.x, (self.max_x() - 1.0).max(self.x)),
            p.y.clamp(self.y, (self.max_y() - 1.0).max(self.y)),
        )
    }

    fn distance_sq(&self, p: Point) -> f64 {
        self.clamp(p).distance_sq(p)
    }

    /// Smallest rect containing all of `rects`.
    pub fn union(rects: impl IntoIterator<Item = Rect>) -> Option<Rect> {
        rects.into_iter().reduce(|a, b| {
            let (x, y) = (a.x.min(b.x), a.y.min(b.y));
            Rect::new(
                x,
                y,
                a.max_x().max(b.max_x()) - x,
                a.max_y().max(b.max_y()) - y,
            )
        })
    }
}

/// Which side of the Mac the secondary starts on before the user arranges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "top" | "above" => Some(Self::Top),
            "bottom" | "below" => Some(Self::Bottom),
            _ => None,
        }
    }
}

/// One secondary display placed in Mac points.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub display: Display,
    /// Where it sits in Mac global points.
    pub rect: Rect,
}

impl Placed {
    /// Its own virtual-desktop pixels.
    pub fn pixels(&self) -> Rect {
        Rect::from_display(&self.display)
    }

    /// Pixels per Mac point.
    pub fn scale(&self) -> f64 {
        f64::from(self.display.width) / self.rect.width
    }

    pub fn to_pixels(&self, p: Point) -> Point {
        let px = self.pixels();
        let s = self.scale();
        Point::new(
            px.x + (p.x - self.rect.x) * s,
            px.y + (p.y - self.rect.y) * s,
        )
    }

    pub fn to_points(&self, p: Point) -> Point {
        let px = self.pixels();
        let s = self.scale();
        Point::new(
            self.rect.x + (p.x - px.x) / s,
            self.rect.y + (p.y - px.y) / s,
        )
    }
}

/// Places the secondary's displays with the block's top-left at `origin`.
/// Each display is as big in points as it looks (pixels ÷ its scale);
/// offsets between displays use the primary display's scale.
pub fn place(displays: &[Display], origin: Point) -> Vec<Placed> {
    let reference = displays
        .iter()
        .find(|d| d.primary)
        .or(displays.first())
        .map_or(1.0, scale_of);
    let left = displays.iter().map(|d| d.x).min().unwrap_or(0);
    let top = displays.iter().map(|d| d.y).min().unwrap_or(0);
    displays
        .iter()
        .map(|d| Placed {
            display: d.clone(),
            rect: Rect::new(
                origin.x + f64::from(d.x - left) / reference,
                origin.y + f64::from(d.y - top) / reference,
                f64::from(d.width) / scale_of(d),
                f64::from(d.height) / scale_of(d),
            ),
        })
        .collect()
}

fn scale_of(d: &Display) -> f64 {
    if d.scale > 0.0 { d.scale.into() } else { 1.0 }
}

/// Size of the secondary's block in Mac points.
pub fn block_size(displays: &[Display]) -> (f64, f64) {
    Rect::union(place(displays, Point::new(0.0, 0.0)).iter().map(|p| p.rect))
        .map_or((0.0, 0.0), |r| (r.width, r.height))
}

/// Shortest shared edge between the Mac and the secondary, in points, so
/// there's always room to cross.
pub const MIN_SHARED_EDGE: f64 = 40.0;

/// The valid origin nearest `desired`: the block touches a Mac display edge
/// (sharing at least [`MIN_SHARED_EDGE`]) without overlapping any.
pub fn snap(mac: &[Rect], (w, h): (f64, f64), desired: Point) -> Option<Point> {
    // Like `clamp`, but a range narrower than the shared edge centers.
    let fit = |v: f64, lo: f64, hi: f64| {
        if lo <= hi {
            v.clamp(lo, hi)
        } else {
            (lo + hi) / 2.0
        }
    };
    mac.iter()
        .flat_map(|r| {
            let y = fit(
                desired.y,
                r.y - h + MIN_SHARED_EDGE,
                r.max_y() - MIN_SHARED_EDGE,
            );
            let x = fit(
                desired.x,
                r.x - w + MIN_SHARED_EDGE,
                r.max_x() - MIN_SHARED_EDGE,
            );
            [
                Point::new(r.max_x(), y),
                Point::new(r.x - w, y),
                Point::new(x, r.max_y()),
                Point::new(x, r.y - h),
            ]
        })
        .filter(|p| {
            let block = Rect::new(p.x, p.y, w, h);
            !mac.iter().any(|r| r.overlaps(&block))
        })
        .min_by(|a, b| a.distance_sq(desired).total_cmp(&b.distance_sq(desired)))
}

/// Initial origin before the user arranges anything: centered on `side` of
/// the main display.
pub fn default_origin(mac: &[Rect], (w, h): (f64, f64), side: Side) -> Option<Point> {
    let main = mac
        .iter()
        .find(|r| r.contains(Point::new(0.0, 0.0)))
        .or(mac.first())?;
    let mid_x = main.x + (main.width - w) / 2.0;
    let mid_y = main.y + (main.height - h) / 2.0;
    let desired = match side {
        Side::Right => Point::new(main.max_x(), mid_y),
        Side::Left => Point::new(main.x - w, mid_y),
        Side::Bottom => Point::new(mid_x, main.max_y()),
        Side::Top => Point::new(mid_x, main.y - h),
    };
    snap(mac, (w, h), desired)
}

/// Rounds a delta away from zero to at least one unit, so pushing against
/// an edge always probes past it.
fn probe(d: f64) -> f64 {
    if d > 0.0 {
        d.max(1.0)
    } else if d < 0.0 {
        d.min(-1.0)
    } else {
        0.0
    }
}

/// The Mac cursor at `pos` moves by (`dx`, `dy`) points. If that pushes it
/// off the Mac and onto the secondary, returns the display index and the
/// pixel it lands on.
pub fn crossing(
    mac: &[Rect],
    secondary: &[Placed],
    pos: Point,
    dx: f64,
    dy: f64,
) -> Option<(usize, Point)> {
    let (dx, dy) = (probe(dx), probe(dy));
    let diagonal = Point::new(pos.x + dx, pos.y + dy);
    if mac.iter().any(|r| r.contains(diagonal)) {
        return None;
    }
    // Also try each axis alone, so a diagonal push slides along the edge.
    [
        diagonal,
        Point::new(pos.x + dx, pos.y),
        Point::new(pos.x, pos.y + dy),
    ]
    .into_iter()
    .filter(|p| *p != pos && !mac.iter().any(|r| r.contains(*p)))
    .find_map(|p| {
        let i = secondary.iter().position(|s| s.rect.contains(p))?;
        let s = &secondary[i];
        Some((i, s.pixels().clamp(s.to_pixels(p))))
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// New cursor position on the secondary, in its pixels.
    Stay(Point),
    /// The cursor went back onto the Mac, at this point.
    Leave(Point),
}

/// Moves the cursor on the secondary by (`dx`, `dy`) pixels. It never lands
/// outside a display; pushing out where the Mac touches leaves.
pub fn move_on_secondary(mac: &[Rect], secondary: &[Placed], pos: Point, dx: f64, dy: f64) -> Step {
    let Some(current) = secondary.iter().min_by(|a, b| {
        a.pixels()
            .distance_sq(pos)
            .total_cmp(&b.pixels().distance_sq(pos))
    }) else {
        return Step::Stay(pos);
    };
    let target = Point::new(pos.x + dx, pos.y + dy);
    if secondary.iter().any(|s| s.pixels().contains(target)) {
        return Step::Stay(target);
    }
    for p in [
        target,
        Point::new(target.x, pos.y),
        Point::new(pos.x, target.y),
    ] {
        let on_mac = current.to_points(p);
        if let Some(r) = mac.iter().find(|r| r.contains(on_mac)) {
            return Step::Leave(r.clamp(on_mac));
        }
    }
    Step::Stay(current.pixels().clamp(target))
}

/// The secondary display under `pos` (pixels), or the nearest one.
pub fn display_at(secondary: &[Display], pos: Point) -> Option<&Display> {
    secondary.iter().min_by(|a, b| {
        Rect::from_display(a)
            .distance_sq(pos)
            .total_cmp(&Rect::from_display(b).distance_sq(pos))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // An LG 1920x1080 above a 1470x956 MacBook Air screen.
    const AIR: Rect = Rect::new(0.0, 0.0, 1470.0, 956.0);
    const LG: Rect = Rect::new(-223.0, -1080.0, 1920.0, 1080.0);
    const MAC: &[Rect] = &[AIR, LG];

    fn display(x: i32, y: i32, width: u32, height: u32, scale: f32, primary: bool) -> Display {
        Display {
            id: 1,
            x,
            y,
            width,
            height,
            scale,
            primary,
        }
    }

    fn pc() -> Vec<Display> {
        vec![display(0, 0, 1920, 1080, 1.25, true)]
    }

    #[test]
    fn places_displays_at_their_visual_size() {
        let two = [
            display(0, 0, 1920, 1080, 1.25, true),
            display(-2560, 0, 2560, 1440, 2.0, false),
        ];
        let placed = place(&two, Point::new(100.0, 50.0));
        // The left monitor's 2560 px at the primary's 1.25 → 2048 pt offset.
        assert_eq!(placed[1].rect, Rect::new(100.0, 50.0, 1280.0, 720.0));
        assert_eq!(placed[0].rect, Rect::new(2148.0, 50.0, 1536.0, 864.0));
        assert_eq!(block_size(&two), (3584.0, 864.0));

        let p = &placed[0];
        let px = p.to_pixels(Point::new(2158.0, 58.0));
        assert_eq!(px, Point::new(12.5, 10.0));
        assert_eq!(p.to_points(px), Point::new(2158.0, 58.0));
    }

    #[test]
    fn snaps_to_the_nearest_touching_edge_without_overlap() {
        let size = (1536.0, 864.0);
        // Dropped overlapping the Air's right half → pushed out to the right.
        assert_eq!(
            snap(MAC, size, Point::new(1000.0, 50.0)),
            Some(Point::new(1470.0, 50.0))
        );
        // Dragged far down-right → hangs off the Air keeping 40 pt shared.
        assert_eq!(
            snap(MAC, size, Point::new(3000.0, 2000.0)),
            Some(Point::new(1470.0, 916.0))
        );
        // Right of the LG, above the Air's level.
        assert_eq!(
            snap(MAC, size, Point::new(1800.0, -900.0)),
            Some(Point::new(1697.0, -900.0))
        );
        assert_eq!(snap(&[], size, Point::new(0.0, 0.0)), None);
    }

    #[test]
    fn default_origin_centers_on_the_main_display_side() {
        let size = block_size(&pc());
        assert_eq!(
            default_origin(MAC, size, Side::Right),
            Some(Point::new(1470.0, 46.0))
        );
        assert_eq!(
            default_origin(MAC, size, Side::Bottom),
            Some(Point::new(-33.0, 956.0))
        );
        // The Air's top is taken by the LG, so it lands on the LG's top.
        assert_eq!(
            default_origin(MAC, size, Side::Top),
            Some(Point::new(-33.0, -1944.0))
        );
    }

    #[test]
    fn crosses_where_the_secondary_touches() {
        let pc = place(&pc(), Point::new(1470.0, 46.0));
        let at = Point::new(1469.0, 446.0);
        assert_eq!(
            crossing(MAC, &pc, at, 3.0, 0.0),
            Some((0, Point::new(2.5, 500.0)))
        );
        // Sub-point pushes still probe past the edge.
        assert_eq!(
            crossing(MAC, &pc, at, 0.2, 0.0),
            Some((0, Point::new(0.0, 500.0)))
        );
        // A diagonal push slides into the PC.
        assert!(crossing(MAC, &pc, at, 2.0, 2.0).is_some());
        // Not pushing outward, or still room on the Mac.
        assert_eq!(crossing(MAC, &pc, at, -3.0, 0.0), None);
        assert_eq!(
            crossing(MAC, &pc, Point::new(1400.0, 446.0), 3.0, 0.0),
            None
        );
        // The Air's right edge above the PC's top: nothing there.
        assert_eq!(crossing(MAC, &pc, Point::new(1469.0, 10.0), 3.0, 0.0), None);
    }

    #[test]
    fn moves_clamps_and_leaves_where_the_mac_touches() {
        let pc = place(&pc(), Point::new(1470.0, 46.0));
        let at = Point::new;
        assert_eq!(
            move_on_secondary(MAC, &pc, at(100.0, 100.0), 10.0, -5.0),
            Step::Stay(at(110.0, 95.0))
        );
        // Top, right and bottom edges clamp.
        assert_eq!(
            move_on_secondary(MAC, &pc, at(1915.0, 5.0), 50.0, -50.0),
            Step::Stay(at(1919.0, 0.0))
        );
        // Left edge where the Air is → back on the Mac at the same height.
        assert_eq!(
            move_on_secondary(MAC, &pc, at(0.0, 500.0), -2.5, 0.0),
            Step::Leave(at(1468.0, 446.0))
        );

        // PC placed low: its left edge below the Air's bottom clamps…
        let low = place(&[display(0, 0, 1920, 1080, 1.0, true)], at(1470.0, 900.0));
        assert_eq!(
            move_on_secondary(MAC, &low, at(0.0, 500.0), -5.0, 0.0),
            Step::Stay(at(0.0, 500.0))
        );
        // …and only the part next to the Air leaves.
        assert!(matches!(
            move_on_secondary(MAC, &low, at(0.0, 20.0), -5.0, 0.0),
            Step::Leave(_)
        ));
    }

    #[test]
    fn walks_between_secondary_displays() {
        let two = [
            display(0, 0, 1920, 1080, 1.0, true),
            display(1920, 0, 1920, 1080, 1.0, false),
        ];
        let pc = place(&two, Point::new(1470.0, 0.0));
        assert_eq!(
            move_on_secondary(MAC, &pc, Point::new(1915.0, 500.0), 10.0, 0.0),
            Step::Stay(Point::new(1925.0, 500.0))
        );
    }
}

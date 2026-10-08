//! Screen geometry: when the cursor leaves the Mac, where it lands on the
//! secondary, how it moves there, and where it comes back.
//!
//! Mac rects are in global display points (origin top-left of the main
//! display, y down). Secondary rects are in its virtual desktop pixels.

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

    /// Nearest point inside, keeping a whole pixel/point from the far edges.
    pub fn clamp(&self, p: Point) -> Point {
        Point::new(
            p.x.clamp(self.x, (self.max_x() - 1.0).max(self.x)),
            p.y.clamp(self.y, (self.max_y() - 1.0).max(self.y)),
        )
    }

    fn distance_sq(&self, p: Point) -> f64 {
        let c = self.clamp(p);
        (c.x - p.x).powi(2) + (c.y - p.y).powi(2)
    }
}

/// Which side of the Mac the secondary sits on.
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

    /// Unit vector pointing from the Mac towards the secondary.
    fn dir(self) -> (f64, f64) {
        match self {
            Self::Left => (-1.0, 0.0),
            Self::Right => (1.0, 0.0),
            Self::Top => (0.0, -1.0),
            Self::Bottom => (0.0, 1.0),
        }
    }

    fn outward(self, dx: f64, dy: f64) -> bool {
        let (ux, uy) = self.dir();
        dx * ux + dy * uy > 0.0
    }

    /// Position of `p` along the shared edge of `r`, from 0 to 1.
    fn along(self, r: &Rect, p: Point) -> f64 {
        let t = match self {
            Self::Left | Self::Right => (p.y - r.y) / r.height,
            Self::Top | Self::Bottom => (p.x - r.x) / r.width,
        };
        t.clamp(0.0, 1.0)
    }

    /// Point `inset` units inside `r`'s edge that faces `towards`, at `t`.
    fn on_edge(self, towards: (f64, f64), r: &Rect, t: f64, inset: f64) -> Point {
        let p = match self {
            Self::Left | Self::Right => {
                let x = if towards.0 > 0.0 {
                    r.max_x() - 1.0 - inset
                } else {
                    r.x + inset
                };
                Point::new(x, r.y + t * r.height)
            }
            Self::Top | Self::Bottom => {
                let y = if towards.1 > 0.0 {
                    r.max_y() - 1.0 - inset
                } else {
                    r.y + inset
                };
                Point::new(r.x + t * r.width, y)
            }
        };
        r.clamp(p)
    }
}

/// The cursor is pushing against a free Mac edge facing the secondary.
/// Returns the Mac display it is leaving and the position along its edge.
pub fn crossing(side: Side, mac: &[Rect], pos: Point, dx: f64, dy: f64) -> Option<(Rect, f64)> {
    if !side.outward(dx, dy) {
        return None;
    }
    let display = mac.iter().find(|r| r.contains(pos))?;
    let (ux, uy) = side.dir();
    let beyond = Point::new(pos.x + ux, pos.y + uy);
    if mac.iter().any(|r| r.contains(beyond)) {
        return None;
    }
    Some((*display, side.along(display, pos)))
}

/// Where the cursor lands on the secondary: on the display edge facing the
/// Mac, at the same relative position it left the Mac's edge.
pub fn entry_point(side: Side, secondary: &[Rect], t: f64) -> Option<Point> {
    let key = |r: &Rect| match side {
        Side::Right => r.x,
        Side::Left => -r.max_x(),
        Side::Bottom => r.y,
        Side::Top => -r.max_y(),
    };
    let facing = secondary.iter().min_by(|a, b| key(a).total_cmp(&key(b)))?;
    let (ux, uy) = side.dir();
    Some(side.on_edge((-ux, -uy), facing, t, 0.0))
}

/// Where the cursor reappears on the Mac display it left from.
pub fn return_point(side: Side, mac_display: &Rect, t: f64) -> Point {
    side.on_edge(side.dir(), mac_display, t, 1.0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// New cursor position on the secondary.
    Stay(Point),
    /// The cursor crossed back to the Mac at this position along the edge.
    Leave(f64),
}

/// Moves the cursor on the secondary by (`dx`, `dy`) pixels. It never lands
/// outside a display; pushing out through a free edge facing the Mac leaves.
pub fn move_on_secondary(side: Side, secondary: &[Rect], pos: Point, dx: f64, dy: f64) -> Step {
    let Some(current) = secondary
        .iter()
        .min_by(|a, b| a.distance_sq(pos).total_cmp(&b.distance_sq(pos)))
    else {
        return Step::Leave(0.5);
    };
    let target = Point::new(pos.x + dx, pos.y + dy);
    if secondary.iter().any(|r| r.contains(target)) {
        return Step::Stay(target);
    }

    let clamped = current.clamp(target);
    let (ux, uy) = side.dir();
    let exits_towards_mac = match side {
        Side::Right => target.x < current.x,
        Side::Left => target.x >= current.max_x(),
        Side::Bottom => target.y < current.y,
        Side::Top => target.y >= current.max_y(),
    };
    if exits_towards_mac && side.outward(-dx, -dy) {
        let edge = side.on_edge((-ux, -uy), current, side.along(current, clamped), 0.0);
        let beyond = Point::new(edge.x - ux, edge.y - uy);
        if !secondary.iter().any(|r| r.contains(beyond)) {
            return Step::Leave(side.along(current, clamped));
        }
    }
    Step::Stay(clamped)
}

/// The secondary display under `pos`, or the nearest one.
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

    // An LG 1920x1080 above a 1280x832 MacBook Air screen.
    const LG: Rect = Rect::new(-320.0, -1080.0, 1920.0, 1080.0);
    const AIR: Rect = Rect::new(0.0, 0.0, 1280.0, 832.0);
    const MAC: &[Rect] = &[AIR, LG];
    const PC: &[Rect] = &[Rect::new(0.0, 0.0, 1920.0, 1080.0)];

    #[test]
    fn crosses_only_at_a_free_edge_pushing_outward() {
        let at_right = Point::new(1279.0, 416.0);
        assert_eq!(
            crossing(Side::Right, MAC, at_right, 5.0, 0.0),
            Some((AIR, 0.5))
        );
        assert_eq!(crossing(Side::Right, MAC, at_right, -5.0, 0.0), None);
        assert_eq!(crossing(Side::Right, MAC, at_right, 0.0, 5.0), None);
        assert_eq!(
            crossing(Side::Right, MAC, Point::new(1200.0, 416.0), 5.0, 0.0),
            None
        );
        // The LG's right edge is free too.
        assert_eq!(
            crossing(Side::Right, MAC, Point::new(1599.0, -540.0), 1.0, 0.0),
            Some((LG, 0.5))
        );
    }

    #[test]
    fn edges_shared_between_mac_displays_never_cross() {
        // Top of the Air touches the LG.
        assert_eq!(
            crossing(Side::Top, MAC, Point::new(640.0, 0.0), 0.0, -3.0),
            None
        );
        // Top of the LG is free.
        assert_eq!(
            crossing(Side::Top, MAC, Point::new(640.0, -1080.0), 0.0, -3.0),
            Some((LG, 0.5))
        );
        // Bottom of the Air is free.
        assert!(crossing(Side::Bottom, MAC, Point::new(0.0, 831.0), 0.0, 1.0).is_some());
    }

    #[test]
    fn enters_on_the_facing_display_edge() {
        let pcs = [
            Rect::new(-1920.0, 0.0, 1920.0, 1080.0),
            Rect::new(0.0, 0.0, 2560.0, 1440.0),
        ];
        assert_eq!(
            entry_point(Side::Right, &pcs, 0.5),
            Some(Point::new(-1920.0, 540.0))
        );
        assert_eq!(
            entry_point(Side::Left, &pcs, 0.0),
            Some(Point::new(2559.0, 0.0))
        );
        assert_eq!(
            entry_point(Side::Bottom, PC, 1.0),
            Some(Point::new(1919.0, 0.0))
        );
        assert_eq!(
            entry_point(Side::Top, PC, 0.25),
            Some(Point::new(480.0, 1079.0))
        );
        assert_eq!(entry_point(Side::Right, &[], 0.5), None);
    }

    #[test]
    fn moves_and_clamps_inside_the_secondary() {
        let start = Point::new(100.0, 100.0);
        assert_eq!(
            move_on_secondary(Side::Right, PC, start, 10.0, -5.0),
            Step::Stay(Point::new(110.0, 95.0))
        );
        // Far edge and top/bottom clamp instead of leaving.
        assert_eq!(
            move_on_secondary(Side::Right, PC, Point::new(1915.0, 5.0), 50.0, -50.0),
            Step::Stay(Point::new(1919.0, 0.0))
        );
    }

    #[test]
    fn leaves_through_the_edge_facing_the_mac() {
        assert_eq!(
            move_on_secondary(Side::Right, PC, Point::new(2.0, 270.0), -5.0, 0.0),
            Step::Leave(0.25)
        );
        assert_eq!(
            move_on_secondary(Side::Bottom, PC, Point::new(960.0, 1.0), 0.0, -4.0),
            Step::Leave(0.5)
        );
        assert_eq!(
            move_on_secondary(Side::Left, PC, Point::new(1918.0, 0.0), 3.0, 0.0),
            Step::Leave(0.0)
        );
    }

    #[test]
    fn walks_between_secondary_displays_without_leaving() {
        let pcs = [
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            Rect::new(-1920.0, 0.0, 1920.0, 1080.0),
        ];
        // Mac on the left of the PC: the PC's left monitor faces it.
        assert_eq!(
            move_on_secondary(Side::Right, &pcs, Point::new(1.0, 500.0), -10.0, 0.0),
            Step::Stay(Point::new(-9.0, 500.0))
        );
        assert_eq!(
            move_on_secondary(Side::Right, &pcs, Point::new(-1919.0, 500.0), -10.0, 0.0),
            Step::Leave(500.0 / 1080.0)
        );
    }

    #[test]
    fn returns_just_inside_the_display_it_left() {
        assert_eq!(
            return_point(Side::Right, &AIR, 0.5),
            Point::new(1278.0, 416.0)
        );
        assert_eq!(
            return_point(Side::Top, &LG, 0.0),
            Point::new(-320.0, -1079.0)
        );
    }
}

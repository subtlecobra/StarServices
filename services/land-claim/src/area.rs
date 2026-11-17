//! Whether a plot lies in the area a client asks about. The area is one point, two opposite corners, or
//! a polygon. Coordinates are treated as a plane, so an area across the antimeridian is not matched.

#[derive(Clone, Copy)]
pub struct Point {
    pub latitude: f64,
    pub longitude: f64,
}

pub struct Rectangle {
    pub bottom_left: Point,
    pub top_right: Point,
}

impl Rectangle {
    fn contains(&self, point: Point) -> bool {
        (self.bottom_left.latitude..=self.top_right.latitude).contains(&point.latitude)
            && (self.bottom_left.longitude..=self.top_right.longitude).contains(&point.longitude)
    }

    fn corners(&self) -> [Point; 4] {
        let (bottom, left) = (self.bottom_left.latitude, self.bottom_left.longitude);
        let (top, right) = (self.top_right.latitude, self.top_right.longitude);
        [
            Point { latitude: bottom, longitude: left },
            Point { latitude: bottom, longitude: right },
            Point { latitude: top, longitude: right },
            Point { latitude: top, longitude: left },
        ]
    }

    fn overlaps(&self, other: &Rectangle) -> bool {
        self.bottom_left.latitude <= other.top_right.latitude
            && other.bottom_left.latitude <= self.top_right.latitude
            && self.bottom_left.longitude <= other.top_right.longitude
            && other.bottom_left.longitude <= self.top_right.longitude
    }
}

fn edges(points: &[Point]) -> impl Iterator<Item = (Point, Point)> + '_ {
    points.iter().copied().zip(points.iter().copied().cycle().skip(1))
}

/// Ray casting along the latitude axis.
fn polygon_contains(polygon: &[Point], point: Point) -> bool {
    edges(polygon)
        .filter(|(a, b)| {
            (a.latitude > point.latitude) != (b.latitude > point.latitude)
                && point.longitude
                    < a.longitude
                        + (point.latitude - a.latitude) / (b.latitude - a.latitude) * (b.longitude - a.longitude)
        })
        .count()
        % 2
        == 1
}

fn turn(a: Point, b: Point, c: Point) -> f64 {
    (b.longitude - a.longitude) * (c.latitude - a.latitude) - (b.latitude - a.latitude) * (c.longitude - a.longitude)
}

fn segments_cross(a: (Point, Point), b: (Point, Point)) -> bool {
    turn(a.0, a.1, b.0) * turn(a.0, a.1, b.1) < 0.0 && turn(b.0, b.1, a.0) * turn(b.0, b.1, a.1) < 0.0
}

pub fn overlaps(area: &[Point], plot: &Rectangle) -> bool {
    match area {
        [] => true,
        [point] => plot.contains(*point),
        [a, b] => {
            let bottom_left = Point { latitude: a.latitude.min(b.latitude), longitude: a.longitude.min(b.longitude) };
            let top_right = Point { latitude: a.latitude.max(b.latitude), longitude: a.longitude.max(b.longitude) };
            plot.overlaps(&Rectangle { bottom_left, top_right })
        }
        polygon => {
            let corners = plot.corners();
            polygon.iter().any(|point| plot.contains(*point))
                || corners.iter().any(|corner| polygon_contains(polygon, *corner))
                || edges(polygon).any(|edge| edges(&corners).any(|side| segments_cross(edge, side)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(latitude: f64, longitude: f64) -> Point {
        Point { latitude, longitude }
    }

    fn plot() -> Rectangle {
        Rectangle { bottom_left: point(10.0, 10.0), top_right: point(20.0, 20.0) }
    }

    #[test]
    fn a_point_must_be_inside() {
        assert!(overlaps(&[point(15.0, 15.0)], &plot()));
        assert!(overlaps(&[point(10.0, 20.0)], &plot()));
        assert!(!overlaps(&[point(25.0, 15.0)], &plot()));
    }

    #[test]
    fn two_points_are_corners_in_any_order() {
        assert!(overlaps(&[point(25.0, 25.0), point(15.0, 15.0)], &plot()));
        assert!(!overlaps(&[point(25.0, 25.0), point(21.0, 21.0)], &plot()));
    }

    #[test]
    fn a_polygon_inside_around_or_across_the_plot_overlaps() {
        let inside = [point(12.0, 12.0), point(12.0, 14.0), point(14.0, 13.0)];
        assert!(overlaps(&inside, &plot()));
        let around = [point(0.0, 0.0), point(0.0, 40.0), point(40.0, 40.0), point(40.0, 0.0)];
        assert!(overlaps(&around, &plot()));
        let across = [point(15.0, 0.0), point(15.0, 40.0), point(16.0, 40.0), point(16.0, 0.0)];
        assert!(overlaps(&across, &plot()));
    }

    #[test]
    fn a_polygon_beside_the_plot_does_not() {
        let beside = [point(30.0, 30.0), point(30.0, 40.0), point(40.0, 35.0)];
        assert!(!overlaps(&beside, &plot()));
        let notch = [point(0.0, 0.0), point(0.0, 30.0), point(9.0, 30.0), point(9.0, 0.0)];
        assert!(!overlaps(&notch, &plot()));
    }
}

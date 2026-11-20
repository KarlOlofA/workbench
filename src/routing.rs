//! Orthogonal link routing, modelled on how Floorplan draws its maps with ELK's orthogonal router.
//! A link leaves its source on the right side and enters its target on the left side, turning only
//! at right angles. All links into one node share a vertical trunk just left of it, so they stack
//! into one path. On its way across, a link passes every column of nodes between its ends through
//! a gap between two nodes (or above or below the column), stepping up or down in the free space
//! between columns. A backward link, which closes a cycle, does the same travelling leftwards.

use std::collections::HashMap;

use crate::graph::{NODE_H, NODE_W};

pub type Point = (f64, f64);

/// How far a link runs out of its source before its first turn.
const EXIT: f64 = 14.0;
/// How far left of its target a link's trunk runs.
const TRUNK: f64 = 18.0;
/// The space between neighbouring trunks or runs, so they never sit on top of each other.
const LANE: f64 = 7.0;
/// Trunks that would overlap move left by one lane each, up to this many lanes.
const MAX_LANES: usize = 4;
/// How far a run keeps from the nodes it passes. Two of these fit in the gap between rows.
const CLEARANCE: f64 = 9.0;
/// Ports keep this far from a node's top and bottom corners.
const PORT_MARGIN: f64 = 12.0;
/// The radius of the rounded corners at each turn.
pub const CORNER: f64 = 10.0;

/// Nodes whose x ranges overlap. A link passes a column through a gap between two of its nodes,
/// or above or below them.
struct Column {
    x0: f64,
    x1: f64,
    /// The tops of its nodes, sorted.
    tops: Vec<f64>,
    /// The y and target of every link already passing through, sorted by y, so links into other
    /// targets keep a lane apart.
    runs: Vec<(f64, usize)>,
}

impl Column {
    /// Whether `y` keeps clear of the column's nodes. The tops are sorted, so this is a binary search.
    fn off_nodes(&self, y: f64) -> bool {
        let below = self.tops.partition_point(|t| t + NODE_H + CLEARANCE - 1.0 <= y);
        self.tops.get(below).is_none_or(|t| y <= t - CLEARANCE + 1.0)
    }

    /// Whether no link into another target already passes within a lane of `y`.
    fn lane_free(&self, y: f64, target: usize) -> bool {
        let near = LANE - 0.5;
        let first = self.runs.partition_point(|r| r.0 <= y - near);
        self.runs[first..].iter().take_while(|r| r.0 < y + near).all(|r| r.1 == target)
    }
}

/// One polyline per edge, from the source's right side to the target's left side, or `None` when
/// an end has no position.
pub fn route(edges: &[(String, String)], positions: &HashMap<String, [f64; 2]>) -> Vec<Option<Vec<Point>>> {
    let ids: Vec<&String> = positions.keys().collect();
    let index: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
    let pos: Vec<[f64; 2]> = ids.iter().map(|id| positions[*id]).collect();
    let ends: Vec<Option<(usize, usize)>> =
        edges.iter().map(|(a, b)| Some((*index.get(a.as_str())?, *index.get(b.as_str())?))).collect();
    let (out_y, in_y) = ports(&ends, &pos);
    let trunk_x = trunks(&ends, &pos, &out_y, &in_y);
    let mut columns = columns(&pos);

    ends.iter()
        .enumerate()
        .map(|(e, end)| {
            let (a, b) = (*end)?;
            let (x1, tx, ty) = (pos[a][0] + NODE_W, trunk_x[b], in_y[e]);
            let rightwards = tx >= x1 + EXIT;
            // The columns between the two ends, in the order the link meets them.
            let passed: Vec<usize> = if rightwards {
                (0..columns.len()).filter(|&c| columns[c].x0 >= x1 && columns[c].x0 < tx).collect()
            } else {
                (0..columns.len()).rev().filter(|&c| columns[c].x0 < x1 && columns[c].x1 > tx).collect()
            };
            let mut y = out_y[e];
            let mut pts = vec![(x1, y)];
            for ci in passed {
                let c = &columns[ci];
                if !(c.off_nodes(y) && c.lane_free(y, b)) {
                    let from = y;
                    let cost = |y: f64| (y - from).abs() + (y - ty).abs();
                    // The lanes just above and below the three nodes either side of the link.
                    let at = c.tops.partition_point(|t| *t < from);
                    let near: Vec<f64> = c.tops[at.saturating_sub(3)..(at + 3).min(c.tops.len())]
                        .iter()
                        .flat_map(|t| {
                            (0..MAX_LANES).flat_map(move |k| {
                                let off = CLEARANCE + k as f64 * LANE;
                                [t - off, t + NODE_H + off]
                            })
                        })
                        .collect();
                    let best = |ok: &dyn Fn(f64) -> bool| near.iter().copied().filter(|&y| ok(y)).min_by(|p, q| cost(*p).total_cmp(&cost(*q)));
                    // A free lane nearby; else a nearby lane shared with links into other targets,
                    // which reads better than a long way round the whole column.
                    y = best(&|y| c.off_nodes(y) && c.lane_free(y, b))
                        .or_else(|| best(&|y| c.off_nodes(y)))
                        .unwrap_or(c.tops[0] - CLEARANCE);
                    // Step in the free space before the column: left of it going right, right of it going left.
                    let jog_x = if rightwards { c.x0 - TRUNK - MAX_LANES as f64 * LANE } else { c.x1 + EXIT + LANE };
                    pts.extend([(jog_x, from), (jog_x, y)]);
                }
                let runs = &mut columns[ci].runs;
                runs.insert(runs.partition_point(|r| r.0 < y), (y, b));
            }
            pts.extend([(tx, y), (tx, ty), (pos[b][0], ty)]);
            Some(pts)
        })
        .collect()
}

/// The y of each edge's exit port on its source and entry port on its target. A node's ports are
/// spread evenly down its side, ordered by where the other end sits, so links don't cross at the node.
fn ports(ends: &[Option<(usize, usize)>], pos: &[[f64; 2]]) -> (Vec<f64>, Vec<f64>) {
    let mid = |i: usize| pos[i][1] + NODE_H / 2.0;
    let mut outgoing: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut incoming: HashMap<usize, Vec<usize>> = HashMap::new();
    for (e, end) in ends.iter().enumerate() {
        if let Some((a, b)) = *end {
            outgoing.entry(a).or_default().push(e);
            incoming.entry(b).or_default().push(e);
        }
    }
    let mut out_y = vec![0.0; ends.len()];
    let mut in_y = vec![0.0; ends.len()];
    let spread = |node: usize, mut list: Vec<usize>, other: &dyn Fn(usize) -> usize, ys: &mut Vec<f64>| {
        list.sort_by(|&e, &f| mid(other(e)).total_cmp(&mid(other(f))));
        let span = NODE_H - 2.0 * PORT_MARGIN;
        for (k, e) in list.iter().enumerate() {
            ys[*e] = pos[node][1] + PORT_MARGIN + span * (k + 1) as f64 / (list.len() + 1) as f64;
        }
    };
    let target = |e: usize| ends[e].unwrap().1;
    let source = |e: usize| ends[e].unwrap().0;
    for (node, list) in outgoing {
        spread(node, list, &target, &mut out_y);
    }
    for (node, list) in incoming {
        spread(node, list, &source, &mut in_y);
    }
    (out_y, in_y)
}

/// The x of each target's trunk. Targets in the same column whose trunks would overlap get
/// separate lanes, so a trunk only ever carries links into one node.
fn trunks(ends: &[Option<(usize, usize)>], pos: &[[f64; 2]], out_y: &[f64], in_y: &[f64]) -> Vec<f64> {
    let mut span: HashMap<usize, (f64, f64)> = HashMap::new();
    for (e, end) in ends.iter().enumerate() {
        let Some((_, b)) = *end else { continue };
        let s = span.entry(b).or_insert((f64::MAX, f64::MIN));
        *s = (s.0.min(out_y[e]).min(in_y[e]), s.1.max(out_y[e]).max(in_y[e]));
    }
    let mut columns: HashMap<i64, Vec<usize>> = HashMap::new();
    for &b in span.keys() {
        columns.entry(pos[b][0].round() as i64).or_default().push(b);
    }
    let mut x: Vec<f64> = pos.iter().map(|p| p[0] - TRUNK).collect();
    for (_, mut targets) in columns {
        targets.sort_by(|a, b| span[a].0.total_cmp(&span[b].0));
        // The lowest point each lane is used down to.
        let mut lane_end: Vec<f64> = Vec::new();
        for b in targets {
            let (top, bottom) = span[&b];
            let lane = match lane_end.iter().position(|&end| end + LANE < top) {
                Some(l) => l,
                None => {
                    lane_end.push(f64::MIN);
                    lane_end.len() - 1
                }
            };
            lane_end[lane] = bottom;
            x[b] -= (lane % MAX_LANES) as f64 * LANE;
        }
    }
    x
}

/// Groups nodes whose x ranges overlap into columns, left to right.
fn columns(pos: &[[f64; 2]]) -> Vec<Column> {
    let mut by_x: Vec<&[f64; 2]> = pos.iter().collect();
    by_x.sort_by(|p, q| p[0].total_cmp(&q[0]));
    let mut out: Vec<Column> = Vec::new();
    for p in by_x {
        match out.last_mut() {
            Some(c) if p[0] < c.x1 => {
                c.x1 = c.x1.max(p[0] + NODE_W);
                c.tops.push(p[1]);
            }
            _ => out.push(Column { x0: p[0], x1: p[0] + NODE_W, tops: vec![p[1]], runs: Vec::new() }),
        }
    }
    for c in &mut out {
        c.tops.sort_by(f64::total_cmp);
    }
    out
}

/// An SVG path through `pts` with each turn rounded off by up to `radius`.
pub fn svg_path(pts: &[Point], radius: f64) -> String {
    let Some(&(x, y)) = pts.first() else { return String::new() };
    let mut d = format!("M{x:.1},{y:.1}");
    for w in pts.windows(3) {
        let (a, b, c) = (w[0], w[1], w[2]);
        let (d1, d2) = ((b.0 - a.0).hypot(b.1 - a.1), (c.0 - b.0).hypot(c.1 - b.1));
        let r = radius.min(d1 / 2.0).min(d2 / 2.0);
        if r <= 0.0 {
            d.push_str(&format!(" L{:.1},{:.1}", b.0, b.1));
            continue;
        }
        let p1 = (b.0 + (a.0 - b.0) * r / d1, b.1 + (a.1 - b.1) * r / d1);
        let p2 = (b.0 + (c.0 - b.0) * r / d2, b.1 + (c.1 - b.1) * r / d2);
        d.push_str(&format!(" L{:.1},{:.1} Q{:.1},{:.1} {:.1},{:.1}", p1.0, p1.1, b.0, b.1, p2.0, p2.1));
    }
    if let Some(&(x, y)) = pts.last().filter(|_| pts.len() > 1) {
        d.push_str(&format!(" L{x:.1},{y:.1}"));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(list: &[(&str, f64, f64)]) -> HashMap<String, [f64; 2]> {
        list.iter().map(|(id, x, y)| (id.to_string(), [*x, *y])).collect()
    }

    fn edge(a: &str, b: &str) -> (String, String) {
        (a.into(), b.into())
    }

    fn orthogonal(pts: &[Point]) -> bool {
        pts.windows(2).all(|w| w[0].0 == w[1].0 || w[0].1 == w[1].1)
    }

    #[test]
    fn a_forward_link_runs_straight_across_and_turns_on_the_trunk() {
        let pos = at(&[("a", 0.0, 0.0), ("b", 260.0, 100.0)]);
        let r = route(&[edge("a", "b")], &pos)[0].clone().unwrap();
        assert_eq!(r.len(), 4);
        assert!(orthogonal(&r));
        assert_eq!(r[0], (NODE_W, NODE_H / 2.0));
        assert_eq!(*r.last().unwrap(), (260.0, 100.0 + NODE_H / 2.0));
    }

    #[test]
    fn links_into_one_node_share_its_trunk() {
        let pos = at(&[("a", 0.0, 0.0), ("b", 0.0, 200.0), ("c", 260.0, 100.0)]);
        let r = route(&[edge("a", "c"), edge("b", "c")], &pos);
        let (r0, r1) = (r[0].clone().unwrap(), r[1].clone().unwrap());
        assert_eq!(r0[1].0, r1[1].0);
        // The upper caller enters through the upper port.
        assert!(r0.last().unwrap().1 < r1.last().unwrap().1);
    }

    /// Whether any straight piece of `pts` passes through a node box (touching the side it enters or
    /// leaves by is fine).
    fn hits_a_node(pts: &[Point], pos: &HashMap<String, [f64; 2]>) -> bool {
        pts.windows(2).any(|w| {
            let (x0, x1) = (w[0].0.min(w[1].0), w[0].0.max(w[1].0));
            let (y0, y1) = (w[0].1.min(w[1].1), w[0].1.max(w[1].1));
            pos.values().any(|p| x0 < p[0] + NODE_W - 0.5 && x1 > p[0] + 0.5 && y0 < p[1] + NODE_H && y1 > p[1])
        })
    }

    #[test]
    fn a_backward_link_goes_round_the_nodes_instead_of_through_them() {
        let pos = at(&[("a", 0.0, 0.0), ("b", 260.0, 0.0)]);
        let r = route(&[edge("b", "a")], &pos)[0].clone().unwrap();
        assert!(orthogonal(&r));
        assert!(!hits_a_node(&r, &pos), "{r:?}");
        assert_eq!(r.last().unwrap().0, 0.0);
    }

    #[test]
    fn a_link_past_a_column_threads_the_gap_between_its_nodes() {
        let pos = at(&[("a", 0.0, 42.0), ("m1", 260.0, 0.0), ("m2", 260.0, 84.0), ("b", 520.0, 42.0)]);
        let r = route(&[edge("a", "b")], &pos)[0].clone().unwrap();
        assert!(orthogonal(&r));
        assert!(!hits_a_node(&r, &pos), "{r:?}");
        let through = r.iter().find(|p| p.0 > 260.0 && p.0 < 260.0 + NODE_W).map(|p| p.1);
        assert!(through.is_none(), "turns inside the column: {r:?}");
        assert!(r.iter().any(|p| p.1 > NODE_H && p.1 < 84.0), "does not use the gap: {r:?}");
    }

    #[test]
    fn runs_into_different_targets_through_one_gap_do_not_overlap() {
        let pos = at(&[("a", 0.0, 0.0), ("b", 0.0, 84.0), ("x", 260.0, 0.0), ("y", 260.0, 84.0)]);
        let r = route(&[edge("x", "a"), edge("y", "b")], &pos);
        let (r0, r1) = (r[0].clone().unwrap(), r[1].clone().unwrap());
        assert!(!hits_a_node(&r0, &pos) && !hits_a_node(&r1, &pos));
        let level = |r: &[Point]| r[r.len() - 3].1;
        assert!((level(&r0) - level(&r1)).abs() >= LANE - 0.5, "{r0:?} {r1:?}");
    }

    #[test]
    fn rounded_path_starts_and_ends_on_the_points() {
        let d = svg_path(&[(0.0, 0.0), (20.0, 0.0), (20.0, 40.0), (40.0, 40.0)], CORNER);
        assert!(d.starts_with("M0.0,0.0"));
        assert!(d.ends_with("L40.0,40.0"));
        assert_eq!(d.matches('Q').count(), 2);
    }
}

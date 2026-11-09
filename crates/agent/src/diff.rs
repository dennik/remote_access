//! Finds the regions of a BGRA frame that changed since the previous one.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

const TILE: usize = 128;

/// Above this share of changed tiles, one JPEG for the frame costs less than many.
const WHOLE_FRAME_PERCENT: usize = 60;

/// Returns the changed regions of `cur`. A `prev` of another size means everything changed.
pub fn dirty_rects(prev: &[u8], cur: &[u8], width: u32, height: u32) -> Vec<Rect> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let whole = Rect { x: 0, y: 0, w: width, h: height };
    if prev.len() != cur.len() {
        return vec![whole];
    }

    let cols = w.div_ceil(TILE);
    let rows = h.div_ceil(TILE);
    let stride = w * 4;
    let mut changed_count = 0usize;

    // Runs of changed tiles in the row above, as (start col, end col, top row),
    // so identical runs grow downward into one rectangle.
    let mut open: Vec<(usize, usize, usize)> = Vec::new();
    let mut out = Vec::new();
    let mut to_rect = |(c0, c1, r0): (usize, usize, usize), r1: usize| {
        let x = c0 * TILE;
        let y = r0 * TILE;
        out.push(Rect {
            x: x as u32,
            y: y as u32,
            w: ((c1 * TILE).min(w) - x) as u32,
            h: ((r1 * TILE).min(h) - y) as u32,
        });
    };

    for row in 0..rows {
        let y0 = row * TILE;
        let y1 = (y0 + TILE).min(h);
        let mut changed = vec![false; cols];
        for y in y0..y1 {
            let a = &prev[y * stride..(y + 1) * stride];
            let b = &cur[y * stride..(y + 1) * stride];
            if a == b {
                continue;
            }
            for (col, flag) in changed.iter_mut().enumerate() {
                if !*flag {
                    let x0 = col * TILE * 4;
                    let x1 = ((col + 1) * TILE).min(w) * 4;
                    *flag = a[x0..x1] != b[x0..x1];
                }
            }
        }
        changed_count += changed.iter().filter(|c| **c).count();

        let mut runs = Vec::new();
        let mut col = 0;
        while col < cols {
            if changed[col] {
                let start = col;
                while col < cols && changed[col] {
                    col += 1;
                }
                runs.push((start, col));
            } else {
                col += 1;
            }
        }

        let mut next_open = Vec::with_capacity(runs.len());
        for run in open.drain(..) {
            if runs.contains(&(run.0, run.1)) {
                next_open.push(run);
            } else {
                to_rect(run, row);
            }
        }
        for (c0, c1) in runs {
            if !next_open.iter().any(|o| o.0 == c0 && o.1 == c1) {
                next_open.push((c0, c1, row));
            }
        }
        open = next_open;
    }
    for run in open {
        to_rect(run, rows);
    }

    if changed_count * 100 >= cols * rows * WHOLE_FRAME_PERCENT {
        return vec![whole];
    }
    out
}

/// Copies one rectangle out of a tightly packed BGRA frame.
pub fn crop(frame: &[u8], frame_width: u32, r: Rect) -> Vec<u8> {
    let stride = frame_width as usize * 4;
    let row_len = r.w as usize * 4;
    let mut out = Vec::with_capacity(row_len * r.h as usize);
    for y in r.y as usize..(r.y + r.h) as usize {
        let start = y * stride + r.x as usize * 4;
        out.extend_from_slice(&frame[start..start + row_len]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 1024;
    const H: u32 = 768;

    fn blank() -> Vec<u8> {
        vec![0u8; (W * H * 4) as usize]
    }

    fn set_pixel(f: &mut [u8], x: u32, y: u32) {
        f[((y * W + x) * 4) as usize] = 255;
    }

    fn covers(rects: &[Rect], x: u32, y: u32) -> bool {
        rects.iter().any(|r| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
    }

    #[test]
    fn identical_frames_have_no_rects() {
        assert!(dirty_rects(&blank(), &blank(), W, H).is_empty());
    }

    #[test]
    fn size_change_is_a_whole_frame() {
        assert_eq!(dirty_rects(&[], &blank(), W, H), vec![Rect { x: 0, y: 0, w: W, h: H }]);
    }

    #[test]
    fn single_pixel_gives_one_tile() {
        let mut cur = blank();
        set_pixel(&mut cur, 300, 400);
        assert_eq!(dirty_rects(&blank(), &cur, W, H), vec![Rect { x: 256, y: 384, w: 128, h: 128 }]);
    }

    #[test]
    fn every_changed_pixel_is_covered_including_edges() {
        let mut cur = blank();
        let points = [(0, 0), (W - 1, 0), (0, H - 1), (W - 1, H - 1), (500, 300), (501, 430)];
        for &(x, y) in &points {
            set_pixel(&mut cur, x, y);
        }
        let rects = dirty_rects(&blank(), &cur, W, H);
        for &(x, y) in &points {
            assert!(covers(&rects, x, y), "({x},{y}) missing from {rects:?}");
        }
        for r in &rects {
            assert!(r.x + r.w <= W && r.y + r.h <= H, "{r:?} leaves the frame");
        }
    }

    #[test]
    fn vertically_adjacent_tiles_merge() {
        let mut cur = blank();
        set_pixel(&mut cur, 10, 10);
        set_pixel(&mut cur, 10, 140);
        assert_eq!(dirty_rects(&blank(), &cur, W, H), vec![Rect { x: 0, y: 0, w: 128, h: 256 }]);
    }

    #[test]
    fn mostly_changed_frame_collapses_to_whole() {
        let cur = vec![1u8; (W * H * 4) as usize];
        assert_eq!(dirty_rects(&blank(), &cur, W, H), vec![Rect { x: 0, y: 0, w: W, h: H }]);
    }

    #[test]
    fn crop_extracts_the_right_pixels() {
        let mut cur = blank();
        set_pixel(&mut cur, 5, 6);
        let out = crop(&cur, W, Rect { x: 5, y: 6, w: 2, h: 2 });
        assert_eq!(out.len(), 16);
        assert_eq!(out[0], 255);
        assert_eq!(out[4], 0);
    }
}

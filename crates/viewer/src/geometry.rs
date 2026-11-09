//! Placing the remote image in a window and mapping pointer positions onto it.

/// Where the remote image sits inside the client area, keeping its aspect ratio.
pub fn fit_rect(client_w: i32, client_h: i32, image_w: i32, image_h: i32) -> (i32, i32, i32, i32) {
    if client_w <= 0 || client_h <= 0 || image_w <= 0 || image_h <= 0 {
        return (0, 0, 0, 0);
    }
    let (cw, ch, iw, ih) = (client_w as i64, client_h as i64, image_w as i64, image_h as i64);
    let (w, h) = if cw * ih <= ch * iw { (cw, cw * ih / iw) } else { (ch * iw / ih, ch) };
    (((cw - w) / 2) as i32, ((ch - h) / 2) as i32, w as i32, h as i32)
}

/// Converts a client-area point to the 0..=65535 range the agent expects.
pub fn to_remote(px: i32, py: i32, rect: (i32, i32, i32, i32)) -> Option<(u16, u16)> {
    let (x, y, w, h) = rect;
    if w <= 0 || h <= 0 {
        return None;
    }
    let scale = |value: i32, origin: i32, extent: i32| -> u16 {
        let offset = (value - origin).clamp(0, (extent - 1).max(0)) as i64;
        (offset * 65535 / ((extent - 1).max(1)) as i64) as u16
    };
    Some((scale(px, x, w), scale(py, y, h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_ratio_and_centres() {
        assert_eq!(fit_rect(1920, 1080, 1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(fit_rect(1000, 1000, 1920, 1080), (0, 219, 1000, 562));
        assert_eq!(fit_rect(1000, 500, 1000, 1000), (250, 0, 500, 500));
        assert_eq!(fit_rect(0, 500, 1000, 1000), (0, 0, 0, 0));
    }

    #[test]
    fn remote_coordinates_reach_both_edges_and_clamp() {
        let rect = (100, 0, 800, 600);
        assert_eq!(to_remote(100, 0, rect), Some((0, 0)));
        assert_eq!(to_remote(899, 599, rect), Some((65535, 65535)));
        assert_eq!(to_remote(5, -3, rect), Some((0, 0)), "letterbox clamps to the edge");
        assert_eq!(to_remote(5000, 5000, rect), Some((65535, 65535)));
        assert_eq!(to_remote(0, 0, (0, 0, 0, 0)), None);
    }
}

//! The remote screen as a BGRA buffer, updated from decoded JPEG tiles.

use anyhow::{anyhow, bail, Result};
use rayon::prelude::*;
use rd_common::proto::Tile;

#[derive(Default)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

fn decode(tile: &Tile) -> Result<Vec<u8>> {
    let mut decoder = jpeg_decoder::Decoder::new(tile.jpeg.as_slice());
    let rgb = decoder.decode()?;
    let info = decoder.info().ok_or_else(|| anyhow!("JPEG has no header"))?;
    if info.width as u32 != tile.width || info.height as u32 != tile.height {
        bail!("tile size doesn't match its JPEG");
    }
    if info.pixel_format != jpeg_decoder::PixelFormat::RGB24 {
        bail!("unexpected JPEG pixel format");
    }
    Ok(rgb)
}

impl Framebuffer {
    /// Decodes without touching the buffer, so painting isn't held up by decoding.
    pub fn decode_tiles(tiles: &[Tile]) -> Result<Vec<Vec<u8>>> {
        tiles.par_iter().map(decode).collect()
    }

    pub fn apply(&mut self, width: u32, height: u32, tiles: &[Tile], decoded: &[Vec<u8>]) -> Result<()> {
        if width != self.width || height != self.height {
            self.width = width;
            self.height = height;
            self.pixels = vec![0; width as usize * height as usize * 4];
        }
        let stride = width as usize * 4;
        for (tile, rgb) in tiles.iter().zip(decoded) {
            // Guards against a malformed message writing outside the buffer.
            if tile.x.saturating_add(tile.width) > width || tile.y.saturating_add(tile.height) > height {
                bail!("tile lies outside the frame");
            }
            let row_pixels = tile.width as usize;
            for row in 0..tile.height as usize {
                let start = (tile.y as usize + row) * stride + tile.x as usize * 4;
                let dst = &mut self.pixels[start..start + row_pixels * 4];
                let src = &rgb[row * row_pixels * 3..(row + 1) * row_pixels * 3];
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(3)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = 255;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(x: u32, y: u32, width: u32, height: u32) -> Tile {
        Tile { x, y, width, height, jpeg: Vec::new() }
    }

    #[test]
    fn converts_rgb_to_bgra_at_the_tile_position() {
        let mut fb = Framebuffer::default();
        let rgb = vec![10, 20, 30];
        fb.apply(4, 4, &[tile(2, 3, 1, 1)], &[rgb]).unwrap();
        let at = (3 * 4 + 2) * 4;
        assert_eq!(&fb.pixels[at..at + 4], &[30, 20, 10, 255]);
        assert_eq!(fb.pixels[0], 0);
    }

    #[test]
    fn rejects_tiles_outside_the_frame() {
        let mut fb = Framebuffer::default();
        assert!(fb.apply(4, 4, &[tile(3, 0, 2, 1)], &[vec![0; 6]]).is_err());
        assert!(fb.apply(4, 4, &[tile(u32::MAX, 0, 2, 1)], &[vec![0; 6]]).is_err());
    }

    #[test]
    fn rejects_a_jpeg_of_the_wrong_size() {
        assert!(Framebuffer::decode_tiles(&[tile(0, 0, 1, 1)]).is_err());
    }
}

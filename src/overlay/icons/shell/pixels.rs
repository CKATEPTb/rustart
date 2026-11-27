// The icon bitmaps use premultiplied BGRA. Filter coverage and color
// together so transparent samples do not darken antialiased edges.

type Pixel = [u8; 4];

#[derive(Clone, Copy)]
struct Span {
    start: f32,
    end: f32,
}

impl Span {
    fn at(index: usize, from: usize, to: usize) -> Self {
        let scale = from as f32 / to as f32;
        if from >= to {
            Self {
                start: index as f32 * scale,
                end: ((index + 1) as f32 * scale).min(from as f32),
            }
        } else {
            let start = ((index as f32 + 0.5) * scale - 0.5).clamp(0.0, (from - 1) as f32);
            Self {
                start,
                end: start + 1.0,
            }
        }
    }

    fn weights(self) -> impl Iterator<Item = (usize, f32)> {
        let total = self.end - self.start;
        (self.start.floor() as usize..self.end.ceil() as usize).map(move |cell| {
            let weight = self.end.min(cell as f32 + 1.0) - self.start.max(cell as f32);
            (cell, weight / total)
        })
    }
}

pub(super) fn resize(input: &[Pixel], width: usize, height: usize, size: usize) -> Vec<Pixel> {
    let edge = width.max(height);
    let fit = |side: usize| ((size * side + edge / 2) / edge).max(1);
    let (out_width, out_height) = (fit(width), fit(height));
    let left = (size - out_width) / 2;
    let top = (size - out_height) / 2;
    let columns = (0..out_width)
        .map(|x| Span::at(x, width, out_width))
        .collect::<Vec<_>>();
    let mut output = vec![[0; 4]; size * size];

    for y in 0..out_height {
        let row = Span::at(y, height, out_height);
        for (x, &column) in columns.iter().enumerate() {
            let mut sum = [0.0; 4];
            for (sy, wy) in row.weights() {
                for (sx, wx) in column.weights() {
                    let pixel = input[sy * width + sx];
                    let weight = wy * wx;
                    for (value, sample) in sum.iter_mut().zip(pixel) {
                        *value += f32::from(sample.min(pixel[3])) * weight;
                    }
                }
            }
            output[(top + y) * size + left + x] = sum.map(|value| value.round() as u8);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_edges_keep_color_while_coverage_decreases() {
        let red = [0, 0, 255, 255];
        let transparent_with_rgb = [255, 255, 255, 0];
        let source = [red, transparent_with_rgb, red, transparent_with_rgb];
        assert_eq!(resize(&source, 2, 2, 1), [[0, 0, 128, 128]]);
    }

    #[test]
    fn upscale_interpolates_a_gradient_and_preserves_its_endpoints() {
        let dark = [0, 0, 0, 255];
        let bright = [0, 0, 255, 255];
        let source = [dark, bright, dark, bright];
        let result = resize(&source, 2, 2, 3);
        assert_eq!(result[3..6], [dark, [0, 0, 128, 255], bright]);
    }
}

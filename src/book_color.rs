// SPDX-License-Identifier: AGPL-3.0-only
//! Whole-book canvas color interpolation.
//!
//! This module deliberately knows nothing about themes, CSS, or the optional
//! eye-comfort palette. It only turns `(page position, RGB)` samples into one
//! color, so the reader can consume that color in whichever way it likes:
//! derive the UI theme from it, feed it to the palette, or use it only for
//! the outline tint.
//!
//! The sampling policy itself is continuous: the target number of samples is
//! a real number that grows smoothly with the book length, and the color is
//! evaluated over a soft window around the middle of the book instead of at
//! one hard point. Callers blend adjacent integer sample counts by the
//! fractional part, so a re-layout that changes the page count does not make
//! the palette jump.

/// Smallest target when a book is long enough to spread its samples.
const MIN_SAMPLES: f64 = 4.0;
/// Largest target; more tiny previews stop improving the book-wide average.
const MAX_SAMPLES: f64 = 10.0;
/// Page count at which the target has covered roughly 63% of its range.
const GROWTH_SCALE: f64 = 120.0;
/// Points used to average the interpolated color curve around the middle.
const WINDOW_STEPS: usize = 9;
/// Half-width of that evaluation window in normalized book position.
const WINDOW_HALF_WIDTH: f64 = 0.25;

/// Target number of sampled pages as a continuous value. The caller samples
/// `floor` and `ceil` and mixes by the fraction, which keeps the result stable
/// when the page count changes.
pub fn sample_budget(page_count: usize) -> f64 {
    if page_count == 0 {
        return 0.0;
    }
    let count = page_count as f64;
    (MIN_SAMPLES + (MAX_SAMPLES - MIN_SAMPLES) * (1.0 - (-count / GROWTH_SCALE).exp())).min(count)
}

/// Page indices and normalized positions spread evenly across the book.
/// Sampling bin centers keeps the cover and the last page from dominating.
pub fn sample_pages(page_count: usize, samples: usize) -> Vec<(usize, f64)> {
    if page_count == 0 || samples == 0 {
        return Vec::new();
    }
    let samples = samples.min(page_count);
    let mut points = Vec::with_capacity(samples);
    let mut last = None;
    for slot in 0..samples {
        let fraction = (slot as f64 + 0.5) / samples as f64;
        let page = ((fraction * page_count as f64) as usize).min(page_count - 1);
        if last == Some(page) {
            continue;
        }
        let position = if page_count > 1 {
            page as f64 / (page_count - 1) as f64
        } else {
            0.5
        };
        points.push((page, position));
        last = Some(page);
    }
    points
}

/// Evaluate the Lagrange interpolant through `samples` at `at`. The
/// barycentric form avoids expanding the polynomial; channels are interpolated
/// independently and clamped back to the 8-bit range.
pub fn interpolate(samples: &[(f64, u32)], at: f64) -> u32 {
    match samples {
        [] => 0x00ff_ffff,
        [(_, color)] => *color,
        _ => {
            let mut weights = Vec::with_capacity(samples.len());
            for (index, (x, _)) in samples.iter().enumerate() {
                let mut denominator = 1.0;
                for (other, (other_x, _)) in samples.iter().enumerate() {
                    if index != other {
                        denominator *= x - other_x;
                    }
                }
                if denominator.abs() < 1.0e-12 {
                    return samples[0].1;
                }
                weights.push(1.0 / denominator);
            }
            let mut terms = Vec::with_capacity(samples.len());
            let mut sum = 0.0;
            for (weight, (x, color)) in weights.iter().zip(samples) {
                let distance = at - x;
                if distance.abs() < 1.0e-12 {
                    return *color;
                }
                let term = weight / distance;
                terms.push(term);
                sum += term;
            }
            if sum.abs() < 1.0e-12 {
                return samples[0].1;
            }
            let mut channels = [0.0f64; 3];
            for (term, (_, color)) in terms.iter().zip(samples) {
                let weight = term / sum;
                channels[0] += weight * f64::from((color >> 16) & 0xff);
                channels[1] += weight * f64::from((color >> 8) & 0xff);
                channels[2] += weight * f64::from(color & 0xff);
            }
            pack(channels)
        }
    }
}

/// Average the interpolated curve over a soft window around the middle of the
/// book. A single evaluation point makes the result step when the sample grid
/// changes; the window smooths both the sample count and the evaluation
/// position.
pub fn soft_color(samples: &[(f64, u32)]) -> u32 {
    match samples {
        [] => 0x00ff_ffff,
        [(_, color)] => *color,
        _ => {
            let mut channels = [0.0f64; 3];
            let mut total = 0.0;
            for step in 0..WINDOW_STEPS {
                let offset = if WINDOW_STEPS > 1 {
                    step as f64 / (WINDOW_STEPS - 1) as f64 * 2.0 - 1.0
                } else {
                    0.0
                };
                let weight = (-(offset * offset) / 2.0).exp();
                let color = interpolate(samples, 0.5 + offset * WINDOW_HALF_WIDTH);
                channels[0] += weight * f64::from((color >> 16) & 0xff);
                channels[1] += weight * f64::from((color >> 8) & 0xff);
                channels[2] += weight * f64::from(color & 0xff);
                total += weight;
            }
            if total <= 0.0 {
                return samples[0].1;
            }
            pack(channels.map(|channel| channel / total))
        }
    }
}

/// Blend two packed colors. Used to move continuously between adjacent integer
/// sample counts.
pub fn mix(from: u32, to: u32, amount: f64) -> u32 {
    let amount = amount.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let a = f64::from((from >> shift) & 0xff);
        let b = f64::from((to >> shift) & 0xff);
        (a + (b - a) * amount).round().clamp(0.0, 255.0) as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn pack(channels: [f64; 3]) -> u32 {
    let channel = |value: f64| value.round().clamp(0.0, 255.0) as u32;
    (channel(channels[0]) << 16) | (channel(channels[1]) << 8) | channel(channels[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_budget_grows_smoothly_and_stays_bounded() {
        assert_eq!(sample_budget(0), 0.0);
        assert_eq!(sample_budget(1), 1.0);
        let mut previous = 0.0;
        for count in [2, 5, 20, 100, 400, 2_000] {
            let budget = sample_budget(count);
            assert!(
                budget > previous,
                "budget must grow with the page count: {count}"
            );
            assert!(budget <= MAX_SAMPLES);
            previous = budget;
        }
        assert!(sample_budget(2_000) > 9.0);
    }

    #[test]
    fn sample_pages_span_the_book_without_duplicates() {
        assert_eq!(sample_pages(0, 4), Vec::new());
        assert_eq!(sample_pages(1, 4), vec![(0, 0.5)]);

        let pages = sample_pages(200, 8);
        assert_eq!(pages.len(), 8);
        assert!(pages[0].0 < 20);
        assert!(pages[pages.len() - 1].0 > 180);
        assert!(pages.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(pages.windows(2).all(|pair| pair[0].1 < pair[1].1));
    }

    #[test]
    fn interpolation_reproduces_a_linear_channel_ramp() {
        // Red rises linearly from 0 to 200. Lagrange interpolation is exact
        // for linear data, so the middle of the book is 100.
        let samples = [
            (0.0, 0x0000_0000),
            (1.0 / 3.0, 0x0032_0000),
            (2.0 / 3.0, 0x0096_0000),
            (1.0, 0x00c8_0000),
        ];
        assert_eq!(interpolate(&samples, 0.5), 0x0064_0000);
        assert_eq!(soft_color(&samples), 0x0064_0000);
    }

    #[test]
    fn interpolation_keeps_a_uniform_book_unchanged() {
        let color = 0x004a_5b6c;
        let samples = [(0.1, color), (0.35, color), (0.65, color), (0.9, color)];
        assert_eq!(interpolate(&samples, 0.5), color);
        assert_eq!(soft_color(&samples), color);
        assert_eq!(interpolate(&[], 0.5), 0x00ff_ffff);
        assert_eq!(soft_color(&[(0.5, color)]), color);
    }

    #[test]
    fn mix_moves_between_adjacent_sample_counts() {
        assert_eq!(mix(0x0010_2030, 0x0030_4050, 0.0), 0x0010_2030);
        assert_eq!(mix(0x0010_2030, 0x0030_4050, 1.0), 0x0030_4050);
        assert_eq!(mix(0x0010_2030, 0x0030_4050, 0.5), 0x0020_3040);
    }
}

//! Lens bow + paper flatten from 1-4 hand-drawn lines.
//!
//! Each line has three handles (start, middle, end) placed along a paper edge that should be
//! straight. A one-parameter radial model removes the bow; with four lines the corrected edges
//! are intersected into a quad and warped to a flat rectangle.
//!
//! Model (distorted <- undistorted): p_d = c + (p_u - c) * (1 + k * r_u^2), r normalised to the
//! half diagonal. Rendering only needs this forward direction, so it stays closed-form.

use crate::imageops::{par_rows, RgbImage};

pub const MAX_LINES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Aspect {
    Auto,
    Iso,
    Letter,
    Wide16x9,
    Tv4x3,
    Custom(f32),
}

impl Aspect {
    pub fn label(&self) -> String {
        match self {
            Aspect::Auto => "Auto (from lines)".into(),
            Aspect::Iso => "A-series paper (1.414)".into(),
            Aspect::Letter => "US Letter (1.294)".into(),
            Aspect::Wide16x9 => "16:9 field".into(),
            Aspect::Tv4x3 => "4:3 field".into(),
            Aspect::Custom(r) => format!("Custom ({r:.3})"),
        }
    }
    /// long side / short side
    pub fn ratio(&self) -> Option<f64> {
        match self {
            Aspect::Auto => None,
            Aspect::Iso => Some(std::f64::consts::SQRT_2),
            Aspect::Letter => Some(11.0 / 8.5),
            Aspect::Wide16x9 => Some(16.0 / 9.0),
            Aspect::Tv4x3 => Some(4.0 / 3.0),
            Aspect::Custom(r) => Some((*r as f64).max(1.0)),
        }
    }
    pub fn to_str(&self) -> String {
        match self {
            Aspect::Auto => "auto".into(),
            Aspect::Iso => "iso".into(),
            Aspect::Letter => "letter".into(),
            Aspect::Wide16x9 => "16x9".into(),
            Aspect::Tv4x3 => "4x3".into(),
            Aspect::Custom(r) => format!("custom:{r}"),
        }
    }
    pub fn parse(s: &str) -> Aspect {
        match s {
            "iso" => Aspect::Iso,
            "letter" => Aspect::Letter,
            "16x9" => Aspect::Wide16x9,
            "4x3" => Aspect::Tv4x3,
            _ => s
                .strip_prefix("custom:")
                .and_then(|v| v.parse().ok())
                .map(Aspect::Custom)
                .unwrap_or(Aspect::Auto),
        }
    }
}

/// A line: start, middle, end handles in normalised image coordinates (0..1).
pub type Line = [[f32; 2]; 3];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub lines: Vec<Line>,
    pub lens: bool,
    pub flatten: bool,
    pub aspect: Aspect,
    /// Manual fine tune added to the fitted k.
    pub k_adjust: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self { lines: Vec::new(), lens: true, flatten: true, aspect: Aspect::Auto, k_adjust: 0.0 }
    }
}

impl Settings {
    /// True when there is anything to correct.
    pub fn active(&self) -> bool {
        (self.lens && (!self.lines.is_empty() || self.k_adjust != 0.0))
            || (self.flatten && self.lines.len() == MAX_LINES)
    }
}

#[derive(Clone, Debug)]
pub struct Solved {
    pub k: f64,
    cx: f64,
    cy: f64,
    inv_r2: f64,
    h: Option<[f64; 9]>,
    pub out_w: usize,
    pub out_h: usize,
    pub status: String,
}

impl Solved {
    pub fn is_identity(&self) -> bool {
        self.k == 0.0 && self.h.is_none()
    }

    #[inline]
    fn distort(&self, x: f64, y: f64) -> (f64, f64) {
        let dx = x - self.cx;
        let dy = y - self.cy;
        let s = 1.0 + self.k * (dx * dx + dy * dy) * self.inv_r2;
        (self.cx + dx * s, self.cy + dy * s)
    }

    /// Output pixel centre -> source pixel position (continuous, pixel centres at +0.5).
    #[inline]
    pub fn map(&self, u: f64, v: f64) -> (f64, f64) {
        let (x, y) = match &self.h {
            Some(h) => apply_h(h, u, v),
            None => (u, v),
        };
        self.distort(x, y)
    }
}

fn undistort_pt(k: f64, cx: f64, cy: f64, inv_r2: f64, x: f64, y: f64) -> (f64, f64) {
    let (dx, dy) = (x - cx, y - cy);
    let (mut ux, mut uy) = (dx, dy);
    for _ in 0..30 {
        let s = 1.0 + k * (ux * ux + uy * uy) * inv_r2;
        if s.abs() < 1e-6 {
            break;
        }
        ux = dx / s;
        uy = dy / s;
    }
    (cx + ux, cy + uy)
}

fn bow_cost(k: f64, lines: &[[(f64, f64); 3]], cx: f64, cy: f64, inv_r2: f64) -> f64 {
    let mut cost = 0.0;
    for l in lines {
        let p: Vec<(f64, f64)> = l.iter().map(|&(x, y)| undistort_pt(k, cx, cy, inv_r2, x, y)).collect();
        let (ax, ay) = (p[2].0 - p[0].0, p[2].1 - p[0].1);
        let len = (ax * ax + ay * ay).sqrt().max(1e-9);
        let d = ((p[1].0 - p[0].0) * ay - (p[1].1 - p[0].1) * ax) / len;
        cost += d * d;
    }
    cost
}

const K_RANGE: f64 = 0.6;

fn fit_k(lines: &[[(f64, f64); 3]], cx: f64, cy: f64, inv_r2: f64) -> f64 {
    let f = |k: f64| bow_cost(k, lines, cx, cy, inv_r2);
    let steps = 240;
    let mut best = (0.0, f(0.0));
    for i in 0..=steps {
        let k = -K_RANGE + 2.0 * K_RANGE * i as f64 / steps as f64;
        let c = f(k);
        if c < best.1 - 1e-12 {
            best = (k, c);
        }
    }
    // golden section refine around the coarse minimum
    let step = 2.0 * K_RANGE / steps as f64;
    let (mut a, mut b) = (best.0 - step, best.0 + step);
    let g = 0.618_033_988_75;
    for _ in 0..40 {
        let c = b - g * (b - a);
        let d = a + g * (b - a);
        if f(c) < f(d) {
            b = d;
        } else {
            a = c;
        }
    }
    let k = 0.5 * (a + b);
    if f(k) <= best.1 {
        k
    } else {
        best.0
    }
}

/// Infinite line through points: (point, unit direction), total least squares.
fn fit_line(pts: &[(f64, f64)]) -> ((f64, f64), (f64, f64)) {
    let n = pts.len() as f64;
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for &(x, y) in pts {
        sxx += (x - mx) * (x - mx);
        sxy += (x - mx) * (y - my);
        syy += (y - my) * (y - my);
    }
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    ((mx, my), (theta.cos(), theta.sin()))
}

fn intersect(a: ((f64, f64), (f64, f64)), b: ((f64, f64), (f64, f64))) -> Option<(f64, f64)> {
    let ((px, py), (dx, dy)) = a;
    let ((qx, qy), (ex, ey)) = b;
    let den = dx * ey - dy * ex;
    if den.abs() < 1e-6 {
        return None;
    }
    let t = ((qx - px) * ey - (qy - py) * ex) / den;
    Some((px + t * dx, py + t * dy))
}

fn apply_h(h: &[f64; 9], u: f64, v: f64) -> (f64, f64) {
    let w = h[6] * u + h[7] * v + 1.0;
    ((h[0] * u + h[1] * v + h[2]) / w, (h[3] * u + h[4] * v + h[5]) / w)
}

/// Homography mapping src[i] -> dst[i].
fn homography(src: &[(f64, f64); 4], dst: &[(f64, f64); 4]) -> Option<[f64; 9]> {
    let mut a = [[0.0f64; 9]; 8];
    for i in 0..4 {
        let (u, v) = src[i];
        let (x, y) = dst[i];
        a[2 * i] = [u, v, 1.0, 0.0, 0.0, 0.0, -u * x, -v * x, x];
        a[2 * i + 1] = [0.0, 0.0, 0.0, u, v, 1.0, -u * y, -v * y, y];
    }
    for col in 0..8 {
        let piv = (col..8).max_by(|&r1, &r2| a[r1][col].abs().total_cmp(&a[r2][col].abs()))?;
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        for r in 0..8 {
            if r != col {
                let f = a[r][col] / a[col][col];
                for c in col..9 {
                    a[r][c] -= f * a[col][c];
                }
            }
        }
    }
    let mut h = [0.0; 9];
    for i in 0..8 {
        h[i] = a[i][8] / a[i][i];
    }
    h[8] = 1.0;
    Some(h)
}

/// Solve the correction for a source image of w x h pixels.
pub fn solve(s: &Settings, w: usize, h: usize) -> Solved {
    let (wf, hf) = (w as f64, h as f64);
    let cx = wf / 2.0;
    let cy = hf / 2.0;
    let inv_r2 = 4.0 / (wf * wf + hf * hf);
    let px_lines: Vec<[(f64, f64); 3]> = s
        .lines
        .iter()
        .map(|l| l.map(|p| (p[0] as f64 * wf, p[1] as f64 * hf)))
        .collect();

    let mut status = Vec::new();
    let mut k = 0.0;
    if s.lens {
        if !px_lines.is_empty() {
            k = fit_k(&px_lines, cx, cy, inv_r2);
        }
        k += s.k_adjust as f64;
        k = k.clamp(-0.9, 0.9);
        if k.abs() < 5e-5 {
            k = 0.0;
        }
        if k != 0.0 {
            status.push(format!("Lens k = {k:+.4}"));
        }
    }

    let mut solved = Solved { k, cx, cy, inv_r2, h: None, out_w: w, out_h: h, status: String::new() };

    if s.flatten {
        if px_lines.len() == MAX_LINES {
            match flatten(&px_lines, k, cx, cy, inv_r2, wf, hf, s.aspect) {
                Ok((hm, ow, oh)) => {
                    solved.h = Some(hm);
                    solved.out_w = ow;
                    solved.out_h = oh;
                    status.push(format!("Flattened to {ow} x {oh}"));
                }
                Err(e) => status.push(format!("Flatten off: {e}")),
            }
        } else {
            status.push(format!("Flatten needs 4 lines ({} drawn)", px_lines.len()));
        }
    }
    if status.is_empty() {
        status.push("No correction".into());
    }
    solved.status = status.join(" | ");
    solved
}

#[allow(clippy::too_many_arguments)]
fn flatten(
    lines: &[[(f64, f64); 3]],
    k: f64,
    cx: f64,
    cy: f64,
    inv_r2: f64,
    wf: f64,
    hf: f64,
    aspect: Aspect,
) -> Result<([f64; 9], usize, usize), String> {
    let fitted: Vec<_> = lines
        .iter()
        .map(|l| {
            let p: Vec<(f64, f64)> = l.iter().map(|&(x, y)| undistort_pt(k, cx, cy, inv_r2, x, y)).collect();
            let c = ((p[0].0 + p[1].0 + p[2].0) / 3.0, (p[0].1 + p[1].1 + p[2].1) / 3.0);
            (fit_line(&p), c)
        })
        .collect();
    // order the four edges around their common centre
    let gx = fitted.iter().map(|f| f.1 .0).sum::<f64>() / 4.0;
    let gy = fitted.iter().map(|f| f.1 .1).sum::<f64>() / 4.0;
    let mut order: Vec<usize> = (0..4).collect();
    order.sort_by(|&a, &b| {
        let ta = (fitted[a].1 .1 - gy).atan2(fitted[a].1 .0 - gx);
        let tb = (fitted[b].1 .1 - gy).atan2(fitted[b].1 .0 - gx);
        ta.total_cmp(&tb)
    });
    let mut corners = Vec::with_capacity(4);
    for i in 0..4 {
        let a = fitted[order[i]].0;
        let b = fitted[order[(i + 1) % 4]].0;
        corners.push(intersect(a, b).ok_or("two lines are parallel")?);
    }
    // sort corners clockwise (screen space) and start at top-left
    let qx = corners.iter().map(|c| c.0).sum::<f64>() / 4.0;
    let qy = corners.iter().map(|c| c.1).sum::<f64>() / 4.0;
    corners.sort_by(|a, b| (a.1 - qy).atan2(a.0 - qx).total_cmp(&(b.1 - qy).atan2(b.0 - qx)));
    let tl = (0..4).min_by(|&a, &b| (corners[a].0 + corners[a].1).total_cmp(&(corners[b].0 + corners[b].1))).unwrap();
    corners.rotate_left(tl);
    for c in &corners {
        if c.0 < -0.5 * wf || c.0 > 1.5 * wf || c.1 < -0.5 * hf || c.1 > 1.5 * hf {
            return Err("corners land far outside the image".into());
        }
    }
    // convexity
    let mut sign = 0.0;
    for i in 0..4 {
        let a = corners[i];
        let b = corners[(i + 1) % 4];
        let c = corners[(i + 2) % 4];
        let cr = (b.0 - a.0) * (c.1 - b.1) - (b.1 - a.1) * (c.0 - b.0);
        if sign == 0.0 {
            sign = cr.signum();
        } else if cr.signum() != sign || cr.abs() < 1e-6 {
            return Err("lines do not form a four-sided shape".into());
        }
    }
    let d = |a: (f64, f64), b: (f64, f64)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
    let (tl, tr, br, bl) = (corners[0], corners[1], corners[2], corners[3]);
    let wid = d(tl, tr).max(d(bl, br));
    let hei = d(tl, bl).max(d(tr, br));
    if wid < 8.0 || hei < 8.0 {
        return Err("paper area too small".into());
    }
    let (mut ow, mut oh) = match aspect.ratio() {
        None => (wid, hei),
        Some(r) => {
            if wid >= hei {
                let w = wid.max(hei * r);
                (w, w / r)
            } else {
                let h = hei.max(wid * r);
                (h / r, h)
            }
        }
    };
    // keep output within 2x the source pixel count
    let cap = 2.0 * wf * hf;
    if ow * oh > cap {
        let s = (cap / (ow * oh)).sqrt();
        ow *= s;
        oh *= s;
    }
    let (ow, oh) = (ow.round().max(16.0) as usize, oh.round().max(16.0) as usize);
    let rect = [(0.0, 0.0), (ow as f64, 0.0), (ow as f64, oh as f64), (0.0, oh as f64)];
    let hm = homography(&rect, &[tl, tr, br, bl]).ok_or("could not solve the warp")?;
    Ok((hm, ow, oh))
}

#[inline]
fn sample(src: &RgbImage, x: f64, y: f64) -> [u8; 3] {
    // x, y are continuous coords with pixel centres at +0.5
    let fx = x - 0.5;
    let fy = y - 0.5;
    if fx < -0.5 || fy < -0.5 || fx > src.w as f64 - 0.5 || fy > src.h as f64 - 0.5 {
        return [0, 0, 0];
    }
    let fx = fx.clamp(0.0, (src.w - 1) as f64);
    let fy = fy.clamp(0.0, (src.h - 1) as f64);
    let x0 = fx.floor() as usize;
    let y0 = fy.floor() as usize;
    let x1 = (x0 + 1).min(src.w - 1);
    let y1 = (y0 + 1).min(src.h - 1);
    let ax = (fx - x0 as f64) as f32;
    let ay = (fy - y0 as f64) as f32;
    let px = |xx: usize, yy: usize, c: usize| src.data[(yy * src.w + xx) * 3 + c] as f32;
    let mut out = [0u8; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let top = px(x0, y0, c) * (1.0 - ax) + px(x1, y0, c) * ax;
        let bot = px(x0, y1, c) * (1.0 - ax) + px(x1, y1, c) * ax;
        *o = (top * (1.0 - ay) + bot * ay + 0.5) as u8;
    }
    out
}

/// Precomputed lookup from preview output pixels to normalised source coordinates.
pub struct Lut {
    pub w: usize,
    pub h: usize,
    map: Vec<[f32; 2]>,
}

/// Build a LUT whose output fits within max_dim (solved for full-res w x h).
pub fn build_lut(s: &Solved, src_w: usize, src_h: usize, max_dim: usize) -> Lut {
    let scale = (s.out_w.max(s.out_h) as f64 / max_dim as f64).max(1.0);
    let w = ((s.out_w as f64 / scale).round() as usize).max(1);
    let h = ((s.out_h as f64 / scale).round() as usize).max(1);
    let sx = s.out_w as f64 / w as f64;
    let sy = s.out_h as f64 / h as f64;
    let mut map = vec![[0f32; 2]; w * h];
    par_rows(&mut map, w, |y, row| {
        for (x, m) in row.iter_mut().enumerate() {
            let (px, py) = s.map((x as f64 + 0.5) * sx, (y as f64 + 0.5) * sy);
            *m = [(px / src_w as f64) as f32, (py / src_h as f64) as f32];
        }
    });
    Lut { w, h, map }
}

/// Apply a LUT to an image of any resolution with the same aspect as the solved source.
pub fn apply_lut(src: &RgbImage, lut: &Lut) -> RgbImage {
    let mut out = RgbImage::new(lut.w, lut.h);
    let (sw, sh) = (src.w as f64, src.h as f64);
    par_rows(&mut out.data, lut.w * 3, |y, row| {
        for x in 0..lut.w {
            let [nx, ny] = lut.map[y * lut.w + x];
            let p = sample(src, nx as f64 * sw, ny as f64 * sh);
            row[x * 3..x * 3 + 3].copy_from_slice(&p);
        }
    });
    out
}

/// Full resolution render for export.
pub fn render_full(src: &RgbImage, settings: &Settings) -> RgbImage {
    if !settings.active() {
        return src.clone();
    }
    let s = solve(settings, src.w, src.h);
    if s.is_identity() {
        return src.clone();
    }
    let mut out = RgbImage::new(s.out_w, s.out_h);
    par_rows(&mut out.data, s.out_w * 3, |y, row| {
        for x in 0..s.out_w {
            let (px, py) = s.map(x as f64 + 0.5, y as f64 + 0.5);
            row[x * 3..x * 3 + 3].copy_from_slice(&sample(src, px, py));
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distort_with(k: f64, w: f64, h: f64, x: f64, y: f64) -> (f64, f64) {
        let (cx, cy) = (w / 2.0, h / 2.0);
        let inv_r2 = 4.0 / (w * w + h * h);
        let (dx, dy) = (x - cx, y - cy);
        let s = 1.0 + k * (dx * dx + dy * dy) * inv_r2;
        (cx + dx * s, cy + dy * s)
    }

    /// Lines that are straight in the world, bent by a known k, should give k back.
    #[test]
    fn recovers_k() {
        let (w, h) = (1920.0, 1080.0);
        for &k_true in &[-0.12, -0.05, 0.0, 0.04, 0.1] {
            let world = [
                [(200.0, 150.0), (960.0, 150.0), (1720.0, 150.0)],
                [(200.0, 930.0), (960.0, 930.0), (1720.0, 930.0)],
                [(200.0, 150.0), (200.0, 540.0), (200.0, 930.0)],
            ];
            let lines: Vec<Line> = world
                .iter()
                .map(|l| {
                    l.map(|(x, y)| {
                        let (dx, dy) = distort_with(k_true, w, h, x, y);
                        [(dx / w) as f32, (dy / h) as f32]
                    })
                })
                .collect();
            let s = Settings { lines, flatten: false, ..Default::default() };
            let sol = solve(&s, w as usize, h as usize);
            assert!((sol.k - k_true).abs() < 0.003, "k_true {k_true} got {}", sol.k);
        }
    }

    /// Four edges of a keystoned sheet should flatten to the requested aspect.
    #[test]
    fn flattens_quad() {
        let (w, h) = (1600usize, 1200usize);
        let tl = (300.0, 200.0);
        let tr = (1300.0, 260.0);
        let br = (1250.0, 1000.0);
        let bl = (350.0, 1050.0);
        let mk = |a: (f64, f64), b: (f64, f64)| -> Line {
            let m = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            [a, m, b].map(|p| [(p.0 / w as f64) as f32, (p.1 / h as f64) as f32])
        };
        // deliberately unordered
        let lines = vec![mk(bl, tl), mk(tl, tr), mk(br, bl), mk(tr, br)];
        let s = Settings { lines, aspect: Aspect::Iso, ..Default::default() };
        let sol = solve(&s, w, h);
        assert!(sol.h.is_some(), "{}", sol.status);
        let r = sol.out_w as f64 / sol.out_h as f64;
        assert!((r - std::f64::consts::SQRT_2).abs() < 0.01, "ratio {r}");
        let (x, y) = sol.map(0.0, 0.0);
        assert!((x - tl.0).abs() < 0.5 && (y - tl.1).abs() < 0.5, "tl maps to {x},{y}");
        let (x, y) = sol.map(sol.out_w as f64, sol.out_h as f64);
        assert!((x - br.0).abs() < 0.5 && (y - br.1).abs() < 0.5, "br maps to {x},{y}");
    }

    #[test]
    fn three_lines_no_flatten() {
        let s = Settings {
            lines: vec![[[0.1, 0.1], [0.5, 0.1], [0.9, 0.1]]; 3],
            ..Default::default()
        };
        let sol = solve(&s, 640, 480);
        assert!(sol.h.is_none());
        assert_eq!((sol.out_w, sol.out_h), (640, 480));
    }

    #[test]
    fn aspect_roundtrip() {
        for a in [Aspect::Auto, Aspect::Iso, Aspect::Letter, Aspect::Wide16x9, Aspect::Tv4x3, Aspect::Custom(1.5)] {
            assert_eq!(Aspect::parse(&a.to_str()), a);
        }
    }
}

//! Criterion micro-benchmark for `to_gaussian_splats`: the old multi-pass
//! cell reduction (up to 9 `.iter().sum()` passes per voxel) vs. the new
//! 2-pass fused accumulation now used in production.
//!
//! This crate is a binary (no `lib.rs`), so the bench cannot import the
//! production symbol directly. Both variants are reproduced here verbatim and
//! driven over identical data; the `new`/`old` shapes match the code in
//! `src/pointcloud.rs` exactly, so the measured speed-up reflects the real
//! change. A `parity` assertion in the harness guards that the two variants
//! produce bit-identical output before timing them.
//!
//! Run: `cargo bench -p wifi-densepose-pointcloud`

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

#[derive(Clone)]
struct ColorPoint {
    x: f32,
    y: f32,
    z: f32,
    r: u8,
    g: u8,
    b: u8,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Splat {
    center: [f32; 3],
    color: [f32; 3],
    opacity: f32,
    scale: [f32; 3],
}

const VOXEL: f32 = 0.08;

fn voxelize(points: &[ColorPoint]) -> std::collections::HashMap<(i32, i32, i32), Vec<&ColorPoint>> {
    let mut cells: std::collections::HashMap<(i32, i32, i32), Vec<&ColorPoint>> =
        std::collections::HashMap::new();
    for p in points {
        let key = (
            (p.x / VOXEL).floor() as i32,
            (p.y / VOXEL).floor() as i32,
            (p.z / VOXEL).floor() as i32,
        );
        cells.entry(key).or_default().push(p);
    }
    cells
}

/// OLD: nine separate `.iter()` passes per cell.
fn splats_old(points: &[ColorPoint]) -> Vec<Splat> {
    let cells = voxelize(points);
    cells
        .values()
        .map(|pts| {
            let n = pts.len() as f32;
            let cx = pts.iter().map(|p| p.x).sum::<f32>() / n;
            let cy = pts.iter().map(|p| p.y).sum::<f32>() / n;
            let cz = pts.iter().map(|p| p.z).sum::<f32>() / n;
            let cr = pts.iter().map(|p| p.r as f32).sum::<f32>() / n / 255.0;
            let cg = pts.iter().map(|p| p.g as f32).sum::<f32>() / n / 255.0;
            let cb = pts.iter().map(|p| p.b as f32).sum::<f32>() / n / 255.0;
            let sx = pts.iter().map(|p| (p.x - cx).abs()).sum::<f32>() / n + 0.01;
            let sy = pts.iter().map(|p| (p.y - cy).abs()).sum::<f32>() / n + 0.01;
            let sz = pts.iter().map(|p| (p.z - cz).abs()).sum::<f32>() / n + 0.01;
            Splat {
                center: [cx, cy, cz],
                color: [cr, cg, cb],
                opacity: (n / 10.0).min(1.0),
                scale: [sx, sy, sz],
            }
        })
        .collect()
}

/// NEW: two fused accumulation passes per cell (production version).
fn splats_new(points: &[ColorPoint]) -> Vec<Splat> {
    let cells = voxelize(points);
    cells
        .values()
        .map(|pts| {
            let n = pts.len() as f32;
            let (mut sum_x, mut sum_y, mut sum_z) = (0.0f32, 0.0f32, 0.0f32);
            let (mut sum_r, mut sum_g, mut sum_b) = (0.0f32, 0.0f32, 0.0f32);
            for p in pts {
                sum_x += p.x;
                sum_y += p.y;
                sum_z += p.z;
                sum_r += p.r as f32;
                sum_g += p.g as f32;
                sum_b += p.b as f32;
            }
            let cx = sum_x / n;
            let cy = sum_y / n;
            let cz = sum_z / n;
            let cr = sum_r / n / 255.0;
            let cg = sum_g / n / 255.0;
            let cb = sum_b / n / 255.0;
            let (mut dev_x, mut dev_y, mut dev_z) = (0.0f32, 0.0f32, 0.0f32);
            for p in pts {
                dev_x += (p.x - cx).abs();
                dev_y += (p.y - cy).abs();
                dev_z += (p.z - cz).abs();
            }
            Splat {
                center: [cx, cy, cz],
                color: [cr, cg, cb],
                opacity: (n / 10.0).min(1.0),
                scale: [dev_x / n + 0.01, dev_y / n + 0.01, dev_z / n + 0.01],
            }
        })
        .collect()
}

/// Deterministic synthetic cloud (no RNG — fully reproducible).
///
/// Points are spread over a room volume that grows with `n` so that the number
/// of occupied voxels scales with the point count (≈ 8 points per voxel on
/// average), matching a real dense cloud where the optimization's per-cell
/// reduction dominates. This avoids the degenerate "all points in one tiny
/// cube" layout, which made the measurement noise-bound.
fn make_cloud(n: usize) -> Vec<ColorPoint> {
    // Side length of the voxel grid (in cells) so total cells ≈ n / 8.
    let cells_per_side = (((n / 8).max(1) as f64).cbrt().ceil() as usize).max(1);
    let extent = cells_per_side as f32 * VOXEL; // metres
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32;
        // Three incommensurate strides walk the whole volume, depositing
        // several points per cell deterministically.
        v.push(ColorPoint {
            x: (t * 0.011) % extent,
            y: (t * 0.017) % extent,
            z: (t * 0.023) % extent,
            r: (i % 256) as u8,
            g: ((i / 2) % 256) as u8,
            b: ((i / 3) % 256) as u8,
        });
    }
    v
}

fn bench_splats(c: &mut Criterion) {
    let mut group = c.benchmark_group("to_gaussian_splats");
    for &n in &[1_000usize, 10_000, 50_000] {
        let cloud = make_cloud(n);

        // Parity guard: old and new must agree bit-for-bit before we time them.
        let a = splats_old(&cloud);
        let b = splats_new(&cloud);
        assert_eq!(a.len(), b.len(), "cell count differs at n={n}");
        // Sort by center to compare set-equality (HashMap order is arbitrary).
        let mut sa = a.clone();
        let mut sb = b.clone();
        let key = |s: &Splat| (s.center[0].to_bits(), s.center[1].to_bits(), s.center[2].to_bits());
        sa.sort_by_key(key);
        sb.sort_by_key(key);
        assert_eq!(sa, sb, "old/new splat output diverged at n={n}");

        group.bench_with_input(BenchmarkId::new("old_9pass", n), &cloud, |bch, cl| {
            bch.iter(|| splats_old(black_box(cl)))
        });
        group.bench_with_input(BenchmarkId::new("new_2pass", n), &cloud, |bch, cl| {
            bch.iter(|| splats_new(black_box(cl)))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_splats);
criterion_main!(benches);

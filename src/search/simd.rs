//! SIMD-accelerated vector math primitives.
//!
//! Uses the `wide` crate for portable SIMD on stable Rust.
//! Compiles to AVX2 on x86-64 and NEON on aarch64.

use wide::f32x8;

// ---------------------------------------------------------------------------
// Dot Product
// ---------------------------------------------------------------------------

/// Compute the dot product of two f32 slices using SIMD.
///
/// Both slices must have the same length. **Panics** if lengths differ.
///
/// Uses `f32x8` (256-bit) with a two-accumulator FMA unrolling
/// strategy for instruction-level parallelism.
#[inline]
pub fn dot_product_simd(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "dot_product_simd: length mismatch");

    let len = a.len();
    let chunks = len / 8;
    let remainder = len % 8;

    // Two accumulators exploit instruction-level parallelism.
    let mut acc0 = f32x8::ZERO;
    let mut acc1 = f32x8::ZERO;

    // Process 16 elements (2 x f32x8) per iteration.
    let a_chunks = a[..chunks * 8].chunks_exact(16);
    let b_chunks = b[..chunks * 8].chunks_exact(16);
    let a_remainder_from_unrolled = a_chunks.remainder();
    let b_remainder_from_unrolled = b_chunks.remainder();

    for (a_pair, b_pair) in a_chunks.zip(b_chunks) {
        let a0 = f32x8::new(a_pair[..8].try_into().unwrap());
        let b0 = f32x8::new(b_pair[..8].try_into().unwrap());
        let a1 = f32x8::new(a_pair[8..16].try_into().unwrap());
        let b1 = f32x8::new(b_pair[8..16].try_into().unwrap());

        acc0 = a0.mul_add(b0, acc0);
        acc1 = a1.mul_add(b1, acc1);
    }

    // Handle the leftover f32x8 chunk (when chunks is odd).
    if a_remainder_from_unrolled.len() >= 8 {
        let a0 = f32x8::new(a_remainder_from_unrolled[..8].try_into().unwrap());
        let b0 = f32x8::new(b_remainder_from_unrolled[..8].try_into().unwrap());
        acc0 = a0.mul_add(b0, acc0);
    }

    // Merge the two accumulators.
    let combined = acc0 + acc1;

    // Horizontal sum: reduce 8 lanes to a single f32.
    let arr: [f32; 8] = combined.to_array();
    let mut sum = arr[0] + arr[1] + arr[2] + arr[3] + arr[4] + arr[5] + arr[6] + arr[7];

    // Handle tail elements (0-7 remaining, scalar fallback).
    let tail_start = len - remainder;
    for i in tail_start..len {
        sum += a[i] * b[i];
    }

    sum
}

// ---------------------------------------------------------------------------
// L2 Normalization
// ---------------------------------------------------------------------------

/// L2-normalize a vector in place so that `||v|| = 1.0`.
///
/// If the vector is all zeros (or near-zero), it is left unchanged.
#[inline]
pub fn normalize_l2(v: &mut [f32]) {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    if norm_sq < f32::EPSILON {
        return; // Zero vector — cannot normalize
    }
    let inv_norm = 1.0 / norm_sq.sqrt();
    for x in v.iter_mut() {
        *x *= inv_norm;
    }
}

/// Check if a vector is already approximately L2-normalized.
///
/// Returns `true` if `| ||v||^2 - 1.0 | < epsilon`.
#[inline]
pub fn is_normalized(vector: &[f32], epsilon: f32) -> bool {
    let norm_sq: f32 = vector.iter().map(|x| x * x).sum();
    (norm_sq - 1.0).abs() < epsilon
}

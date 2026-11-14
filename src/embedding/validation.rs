//! Shared vector boundary for callers and providers. Profile compatibility is
//! a separate check; equal dimensions alone do not establish provenance.

/// Validate before changing the vector, then normalize with f64 accumulation.
/// The algorithm matches the existing explicit-query boundary (PR28): finite
/// f32 extremes and subnormals must not overflow/underflow the squared norm.
pub fn normalize_vector(vector: &mut [f32], dimensions: usize) -> Result<(), String> {
    if !(1..=8192).contains(&dimensions) || vector.len() != dimensions {
        return Err(format!(
            "embedding has {} dimensions, namespace requires {dimensions} (1–8192)",
            vector.len()
        ));
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err("embedding elements must be finite".into());
    }
    let norm_squared: f64 = vector.iter().map(|&value| f64::from(value).powi(2)).sum();
    if !norm_squared.is_finite() || norm_squared <= 0.0 {
        return Err("embedding must have a finite positive norm".into());
    }
    let norm = norm_squared.sqrt();
    for value in vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(())
}

//! Vector storage helpers for the Recall tier: Matryoshka truncation and
//! symmetric int8 quantisation (ADR "personal memory — three tiers by trust,
//! one index").
//!
//! A 256-d int8 vector costs 256 B + 6 B header at rest instead of 6 KB for
//! a 1536-d f32 one. Vectors are L2-normalised before quantisation so the
//! per-vector scale is the only float that survives; cosine similarity on the
//! dequantised vector is within ~1e-2 of the f32 original, which is far
//! below the score gaps that matter for ranking.

use serde::{Deserialize, Serialize};

/// Keep the first `out` components and re-normalise (Matryoshka models are
/// trained so prefixes stay meaningful). Vectors already shorter than `out`
/// pass through unchanged.
pub fn mrl_truncate(v: &[f32], out: usize) -> Vec<f32> {
    let head = if out >= v.len() { v } else { &v[..out] };
    let norm = head.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm < f32::EPSILON {
        return head.to_vec();
    }
    head.iter().map(|x| x / norm).collect()
}

/// An int8 vector with its dequantisation scale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuantizedVector {
    pub scale: f32,
    pub data: Vec<i8>,
}

impl QuantizedVector {
    /// Quantise symmetrically around zero with a per-vector absmax scale.
    pub fn from_f32(v: &[f32]) -> Self {
        let absmax = v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        if absmax < f32::EPSILON {
            return Self {
                scale: 0.0,
                data: vec![0; v.len()],
            };
        }
        let scale = absmax / 127.0;
        let data = v
            .iter()
            .map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8)
            .collect();
        Self { scale, data }
    }

    pub fn to_f32(&self) -> Vec<f32> {
        self.data.iter().map(|&q| q as f32 * self.scale).collect()
    }

    pub fn dimension(&self) -> usize {
        self.data.len()
    }

    /// Compact on-disk form: `[dim: u16 LE][scale: f32 LE][int8 × dim]`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6 + self.data.len());
        out.extend_from_slice(&(self.data.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.scale.to_le_bytes());
        out.extend(self.data.iter().map(|&q| q as u8));
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 6 {
            return None;
        }
        let dim = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        let scale = f32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        let data = &bytes[6..];
        if data.len() != dim {
            return None;
        }
        Some(Self {
            scale,
            data: data.iter().map(|&b| b as i8).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb)
    }

    #[test]
    fn should_keep_prefix_unit_length_when_truncating() {
        let v: Vec<f32> = (0..8).map(|i| (i as f32 + 1.0) * 0.1).collect();
        let t = mrl_truncate(&v, 4);
        assert_eq!(t.len(), 4);
        let norm: f32 = t.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert_eq!(
            mrl_truncate(&v, 16).len(),
            8,
            "shorter vectors pass through"
        );
    }

    #[test]
    fn should_round_trip_within_quantisation_error() {
        let v = mrl_truncate(&[0.3, -0.7, 0.05, 0.9, -0.2, 0.0, 0.11, -0.44], 8);
        let q = QuantizedVector::from_f32(&v);
        let back = q.to_f32();
        assert!(cosine(&v, &back) > 0.999, "cosine {}", cosine(&v, &back));
        let bytes = q.to_bytes();
        assert_eq!(bytes.len(), 6 + 8);
        assert_eq!(QuantizedVector::from_bytes(&bytes).unwrap(), q);
        assert!(QuantizedVector::from_bytes(&bytes[..9]).is_none());
    }

    #[test]
    fn should_encode_zero_vector_without_nan() {
        let q = QuantizedVector::from_f32(&[0.0, 0.0, 0.0]);
        assert_eq!(q.scale, 0.0);
        assert!(q.to_f32().iter().all(|x| *x == 0.0));
    }
}

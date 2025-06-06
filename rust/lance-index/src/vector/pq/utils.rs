// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

use arrow_array::{cast::AsArray, types::ArrowPrimitiveType, Array, FixedSizeListArray};
use lance_core::{Error, Result};
use snafu::location;

/// Divide a 2D vector in [`T::Array`] to `m` sub-vectors.
///
/// For example, for a `[1024x1M]` matrix, when `n = 8`, this function divides
/// the matrix into  `[128x1M; 8]` vector of matrix.
pub(super) fn divide_to_subvectors<T: ArrowPrimitiveType>(
    fsl: &FixedSizeListArray,
    m: usize,
) -> Result<Vec<Vec<T::Native>>> {
    let dim = fsl.value_length() as usize;
    if dim % m != 0 {
        return Err(Error::invalid_input(
            format!(
                "num_sub_vectors must divide vector dimension {}, but got {}",
                dim, m
            ),
            location!(),
        ));
    };

    let sub_vector_length = dim / m;
    let capacity = fsl.len() * sub_vector_length;
    let mut subarrays = vec![Vec::with_capacity(capacity); m];

    // TODO: very intensive memory copy involved!!! But this is on the write path.
    // Optimize for memory copy later.
    fsl.values()
        .as_primitive::<T>()
        .values()
        .chunks(dim)
        .for_each(|vec| {
            for i in 0..m {
                subarrays[i]
                    .extend_from_slice(&vec[i * sub_vector_length..(i + 1) * sub_vector_length]);
            }
        });
    Ok(subarrays)
}

/// Number of PQ centroids, for the corresponding number of PQ bits.
///
// TODO: pub(crate)
pub fn num_centroids(num_bits: impl Into<u32>) -> usize {
    2_usize.pow(num_bits.into())
}

#[inline]
pub fn get_sub_vector_centroids<const NUM_BITS: u32, T>(
    codebook: &[T],
    dimension: usize,
    num_sub_vectors: usize,
    sub_vector_idx: usize,
) -> &[T] {
    debug_assert!(
        sub_vector_idx < num_sub_vectors,
        "sub_vector idx: {}, num_sub_vectors: {}",
        sub_vector_idx,
        num_sub_vectors
    );

    let num_centroids: usize = 2_usize.pow(NUM_BITS);
    let sub_vector_width = dimension / num_sub_vectors;
    &codebook[sub_vector_idx * num_centroids * sub_vector_width
        ..(sub_vector_idx + 1) * num_centroids * sub_vector_width]
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{types::Float32Type, FixedSizeListArray, Float32Array};
    use lance_arrow::FixedSizeListArrayExt;

    #[test]
    fn test_divide_to_subvectors() {
        let values = Float32Array::from_iter((0..320).map(|v| v as f32));
        // A [10, 32] array.
        let mat = FixedSizeListArray::try_new_from_values(values, 32).unwrap();
        let sub_vectors = divide_to_subvectors::<Float32Type>(&mat, 4).unwrap();
        assert_eq!(sub_vectors.len(), 4);
        assert_eq!(sub_vectors[0].len(), 10 * 8);

        assert_eq!(
            sub_vectors[0],
            (0..10)
                .flat_map(|i| (0..8).map(move |c| 32.0 * i as f32 + c as f32))
                .collect::<Vec<_>>()
        );
    }
}

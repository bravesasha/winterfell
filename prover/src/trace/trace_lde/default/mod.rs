// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

use super::{
    ColMatrix, ElementHasher, EvaluationFrame, FieldElement, Hasher, Queries, StarkDomain,
    TraceInfo, TraceLayout, TraceLde, TracePolyTable,
};
use crate::{RowMatrix, DEFAULT_SEGMENT_WIDTH};
use crypto::MerkleTree;
use tracing::info_span;
use utils::collections::*;

#[cfg(test)]
mod tests;

// TRACE LOW DEGREE EXTENSION
// ================================================================================================
/// Contains all segments of the extended execution trace, the commitments to these segments, the
/// LDE blowup factor, and the [TraceInfo].
///
/// Segments are stored in two groups:
/// - Main segment: this is the first trace segment generated by the prover. Values in this segment
///   will always be elements in the base field (even when an extension field is used).
/// - Auxiliary segments: a list of 0 or more segments for traces generated after the prover
///   commits to the first trace segment. Currently, at most 1 auxiliary segment is possible.
pub struct DefaultTraceLde<E: FieldElement, H: ElementHasher<BaseField = E::BaseField>> {
    // low-degree extension of the main segment of the trace
    main_segment_lde: RowMatrix<E::BaseField>,
    // commitment to the main segment of the trace
    main_segment_tree: MerkleTree<H>,
    // low-degree extensions of the auxiliary segments of the trace
    aux_segment_ldes: Vec<RowMatrix<E>>,
    // commitment to the auxiliary segments of the trace
    aux_segment_trees: Vec<MerkleTree<H>>,
    blowup: usize,
    trace_info: TraceInfo,
}

impl<E: FieldElement, H: ElementHasher<BaseField = E::BaseField>> DefaultTraceLde<E, H> {
    /// Takes the main trace segment columns as input, interpolates them into polynomials in
    /// coefficient form, evaluates the polynomials over the LDE domain, commits to the
    /// polynomial evaluations, and creates a new [DefaultTraceLde] with the LDE of the main trace
    /// segment and the commitment.
    ///
    /// Returns a tuple containing a [TracePolyTable] with the trace polynomials for the main trace
    /// segment and the new [DefaultTraceLde].
    pub fn new(
        trace_info: &TraceInfo,
        main_trace: &ColMatrix<E::BaseField>,
        domain: &StarkDomain<E::BaseField>,
    ) -> (Self, TracePolyTable<E>) {
        // extend the main execution trace and build a Merkle tree from the extended trace
        let (main_segment_lde, main_segment_tree, main_segment_polys) =
            build_trace_commitment::<E, E::BaseField, H>(main_trace, domain);

        let trace_poly_table = TracePolyTable::new(main_segment_polys);
        let trace_lde = DefaultTraceLde {
            main_segment_lde,
            main_segment_tree,
            aux_segment_ldes: Vec::new(),
            aux_segment_trees: Vec::new(),
            blowup: domain.trace_to_lde_blowup(),
            trace_info: trace_info.clone(),
        };

        (trace_lde, trace_poly_table)
    }

    // TEST HELPERS
    // --------------------------------------------------------------------------------------------

    /// Returns number of columns in the main segment of the execution trace.
    #[cfg(test)]
    pub fn main_segment_width(&self) -> usize {
        self.main_segment_lde.num_cols()
    }

    /// Returns a reference to [Matrix] representing the main trace segment.
    #[cfg(test)]
    pub fn get_main_segment(&self) -> &RowMatrix<E::BaseField> {
        &self.main_segment_lde
    }

    /// Returns the entire trace for the column at the specified index.
    #[cfg(test)]
    pub fn get_main_segment_column(&self, col_idx: usize) -> Vec<E::BaseField> {
        (0..self.main_segment_lde.num_rows())
            .map(|row_idx| self.main_segment_lde.get(col_idx, row_idx))
            .collect()
    }
}

impl<E, H> TraceLde<E> for DefaultTraceLde<E, H>
where
    E: FieldElement,
    H: ElementHasher<BaseField = E::BaseField>,
{
    type HashFn = H;

    /// Returns the commitment to the low-degree extension of the main trace segment.
    fn get_main_trace_commitment(&self) -> <Self::HashFn as Hasher>::Digest {
        let root_hash = self.main_segment_tree.root();
        *root_hash
    }

    /// Takes auxiliary trace segment columns as input, interpolates them into polynomials in
    /// coefficient form, evaluates the polynomials over the LDE domain, and commits to the
    /// polynomial evaluations.
    ///
    /// Returns a tuple containing the column polynomials in coefficient from and the commitment
    /// to the polynomial evaluations over the LDE domain.
    ///
    /// # Panics
    ///
    /// This function will panic if any of the following are true:
    /// - the number of rows in the provided `aux_trace` does not match the main trace.
    /// - this segment would exceed the number of segments specified by the trace layout.
    fn add_aux_segment(
        &mut self,
        aux_trace: &ColMatrix<E>,
        domain: &StarkDomain<E::BaseField>,
    ) -> (ColMatrix<E>, <Self::HashFn as Hasher>::Digest) {
        // extend the auxiliary trace segment and build a Merkle tree from the extended trace
        let (aux_segment_lde, aux_segment_tree, aux_segment_polys) =
            build_trace_commitment::<E, E, H>(aux_trace, domain);

        // check errors
        assert!(
            self.aux_segment_ldes.len() < self.trace_info.layout().num_aux_segments(),
            "the specified number of auxiliary segments has already been added"
        );
        assert_eq!(
            self.main_segment_lde.num_rows(),
            aux_segment_lde.num_rows(),
            "the number of rows in the auxiliary segment must be the same as in the main segment"
        );

        // save the lde and commitment
        self.aux_segment_ldes.push(aux_segment_lde);
        let root_hash = *aux_segment_tree.root();
        self.aux_segment_trees.push(aux_segment_tree);

        (aux_segment_polys, root_hash)
    }

    /// Reads current and next rows from the main trace segment into the specified frame.
    fn read_main_trace_frame_into(
        &self,
        lde_step: usize,
        frame: &mut EvaluationFrame<E::BaseField>,
    ) {
        // at the end of the trace, next state wraps around and we read the first step again
        let next_lde_step = (lde_step + self.blowup()) % self.trace_len();

        // copy main trace segment values into the frame
        frame.current_mut().copy_from_slice(self.main_segment_lde.row(lde_step));
        frame.next_mut().copy_from_slice(self.main_segment_lde.row(next_lde_step));
    }

    /// Reads current and next rows from the auxiliary trace segment into the specified frame.
    ///
    /// # Panics
    /// This currently assumes that there is exactly one auxiliary trace segment, and will panic
    /// otherwise.
    fn read_aux_trace_frame_into(&self, lde_step: usize, frame: &mut EvaluationFrame<E>) {
        // at the end of the trace, next state wraps around and we read the first step again
        let next_lde_step = (lde_step + self.blowup()) % self.trace_len();

        // copy auxiliary trace segment values into the frame
        let segment = &self.aux_segment_ldes[0];
        frame.current_mut().copy_from_slice(segment.row(lde_step));
        frame.next_mut().copy_from_slice(segment.row(next_lde_step));
    }

    /// Returns trace table rows at the specified positions along with Merkle authentication paths
    /// from the commitment root to these rows.
    fn query(&self, positions: &[usize]) -> Vec<Queries> {
        // build queries for the main trace segment
        let mut result = vec![build_segment_queries(
            &self.main_segment_lde,
            &self.main_segment_tree,
            positions,
        )];

        // build queries for auxiliary trace segments
        for (i, segment_tree) in self.aux_segment_trees.iter().enumerate() {
            let segment_lde = &self.aux_segment_ldes[i];
            result.push(build_segment_queries(segment_lde, segment_tree, positions));
        }

        result
    }

    /// Returns the number of rows in the execution trace.
    fn trace_len(&self) -> usize {
        self.main_segment_lde.num_rows()
    }

    /// Returns blowup factor which was used to extend original execution trace into trace LDE.
    fn blowup(&self) -> usize {
        self.blowup
    }

    /// Returns the trace layout of the execution trace.
    fn trace_layout(&self) -> &TraceLayout {
        self.trace_info.layout()
    }
}

// HELPER FUNCTIONS
// ================================================================================================

/// Computes a low-degree extension (LDE) of the provided execution trace over the specified
/// domain and builds a commitment to the extended trace.
///
/// The extension is performed by interpolating each column of the execution trace into a
/// polynomial of degree = trace_length - 1, and then evaluating the polynomial over the LDE
/// domain.
///
/// The trace commitment is computed by hashing each row of the extended execution trace, then
/// building a Merkle tree from the resulting hashes.
fn build_trace_commitment<E, F, H>(
    trace: &ColMatrix<F>,
    domain: &StarkDomain<E::BaseField>,
) -> (RowMatrix<F>, MerkleTree<H>, ColMatrix<F>)
where
    E: FieldElement,
    F: FieldElement<BaseField = E::BaseField>,
    H: ElementHasher<BaseField = E::BaseField>,
{
    // extend the execution trace
    let (trace_lde, trace_polys) = {
        let span = info_span!(
            "extend_execution_trace",
            num_cols = trace.num_cols(),
            blowup = domain.trace_to_lde_blowup()
        )
        .entered();
        let trace_polys = trace.interpolate_columns();
        let trace_lde =
            RowMatrix::evaluate_polys_over::<DEFAULT_SEGMENT_WIDTH>(&trace_polys, domain);
        drop(span);

        (trace_lde, trace_polys)
    };
    assert_eq!(trace_lde.num_cols(), trace.num_cols());
    assert_eq!(trace_polys.num_rows(), trace.num_rows());
    assert_eq!(trace_lde.num_rows(), domain.lde_domain_size());

    // build trace commitment
    let tree_depth = trace_lde.num_rows().ilog2() as usize;
    let trace_tree = info_span!("compute_execution_trace_commitment", tree_depth)
        .in_scope(|| trace_lde.commit_to_rows());
    assert_eq!(trace_tree.depth(), tree_depth);

    (trace_lde, trace_tree, trace_polys)
}

fn build_segment_queries<E, H>(
    segment_lde: &RowMatrix<E>,
    segment_tree: &MerkleTree<H>,
    positions: &[usize],
) -> Queries
where
    E: FieldElement,
    H: ElementHasher<BaseField = E::BaseField>,
{
    // for each position, get the corresponding row from the trace segment LDE and put all these
    // rows into a single vector
    let trace_states =
        positions.iter().map(|&pos| segment_lde.row(pos).to_vec()).collect::<Vec<_>>();

    // build Merkle authentication paths to the leaves specified by positions
    let trace_proof = segment_tree
        .prove_batch(positions)
        .expect("failed to generate a Merkle proof for trace queries");

    Queries::new(trace_proof, trace_states)
}

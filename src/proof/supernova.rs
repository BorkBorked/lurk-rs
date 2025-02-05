use abomonation::Abomonation;
use ff::PrimeField;
use nova::{
    supernova::{
        self,
        error::SuperNovaError,
        snark::{CompressedSNARK, ProverKey, VerifierKey},
        AuxParams, CircuitDigests, NonUniformCircuit, RecursiveSNARK,
        StepCircuit as SuperStepCircuit, TrivialSecondaryCircuit,
    },
    traits::{
        snark::{BatchedRelaxedR1CSSNARKTrait, RelaxedR1CSSNARKTrait},
        Engine,
    },
};
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use std::{
    marker::PhantomData,
    ops::Index,
    sync::{Arc, Mutex},
};
use tracing::info;

use crate::{
    config::lurk_config,
    coprocessor::Coprocessor,
    error::ProofError,
    eval::lang::Lang,
    field::LurkField,
    lem::store::Store,
    proof::{
        nova::{CurveCycleEquipped, NovaCircuitShape, E1, E2},
        Prover, RecursiveSNARKTrait,
    },
};

use super::{nova::C1LEM, FoldingMode};

/// Type alias for a Trivial Test Circuit with G2 scalar field elements.
pub type C2<F> = TrivialSecondaryCircuit<<E2<F> as Engine>::Scalar>;

/// Type alias for SuperNova Aux Parameters with the curve cycle types defined above.
pub type SuperNovaAuxParams<F> = AuxParams<E1<F>, E2<F>>;

/// Type alias for SuperNova Public Parameters with the curve cycle types defined above.
pub type SuperNovaPublicParams<F, C1> = supernova::PublicParams<E1<F>, E2<F>, C1, C2<F>>;

/// A struct that contains public parameters for the SuperNova proving system.
pub struct PublicParams<F: CurveCycleEquipped, SC: SuperStepCircuit<F>>
where
    // technical bounds that would disappear once associated_type_bounds stabilizes
    <<E1<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
    <<E2<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
{
    /// Public params for SuperNova.
    pub pp: SuperNovaPublicParams<F, SC>,
    /// Prover key for SuperNova
    pub pk: ProverKey<E1<F>, E2<F>, SC, C2<F>, SS1<F>, SS2<F>>,
    /// Verifier key for SuperNova
    pub vk: VerifierKey<E1<F>, E2<F>, SC, C2<F>, SS1<F>, SS2<F>>,
}

impl<F: CurveCycleEquipped, SC: SuperStepCircuit<F>> Index<usize> for PublicParams<F, SC>
where
    // technical bounds that would disappear once associated_type_bounds stabilizes
    <<E1<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
    <<E2<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
{
    type Output = NovaCircuitShape<F>;

    fn index(&self, index: usize) -> &Self::Output {
        &self.pp[index]
    }
}

impl<F: CurveCycleEquipped, SC: SuperStepCircuit<F>> PublicParams<F, SC>
where
    // technical bounds that would disappear once associated_type_bounds stabilizes
    <<E1<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
    <<E2<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
{
    /// return the digest
    pub fn digest(&self) -> F {
        self.pp.digest()
    }
}

/// Type alias for the Evaluation Engine using G1 group elements.
pub type EE1<F> = <F as CurveCycleEquipped>::EE1;
/// Type alias for the Evaluation Engine using G2 group elements.
pub type EE2<F> = <F as CurveCycleEquipped>::EE2;

/// Type alias for the Relaxed R1CS Spartan SNARK using G1 group elements, EE1.
// NOTE: this is not a SNARK that uses computational commitments,
// that SNARK would be found at nova::spartan::ppsnark::RelaxedR1CSSNARK,
pub type SS1<F> = nova::spartan::batched::BatchedRelaxedR1CSSNARK<E1<F>, EE1<F>>;
/// Type alias for the Relaxed R1CS Spartan SNARK using G2 group elements, EE2.
// NOTE: this is not a SNARK that uses computational commitments,
// that SNARK would be found at nova::spartan::ppsnark::RelaxedR1CSSNARK,
pub type SS2<F> = nova::spartan::snark::RelaxedR1CSSNARK<E2<F>, EE2<F>>;

/// Generates the running claim params for the SuperNova proving system.
pub fn public_params<'a, F: CurveCycleEquipped, C: Coprocessor<F> + 'a>(
    rc: usize,
    lang: Arc<Lang<F, C>>,
) -> PublicParams<F, C1LEM<'a, F, C>>
where
    <<E1<F> as Engine>::Scalar as ff::PrimeField>::Repr: Abomonation,
    <<E2<F> as Engine>::Scalar as ff::PrimeField>::Repr: Abomonation,
{
    let folding_config = Arc::new(FoldingConfig::new_nivc(lang, rc));
    let non_uniform_circuit = C1LEM::<'a, F, C>::blank(folding_config, 0);

    // grab hints for the compressed SNARK variants we will use this with
    let commitment_size_hint1 = <SS1<F> as BatchedRelaxedR1CSSNARKTrait<E1<F>>>::ck_floor();
    let commitment_size_hint2 = <SS2<F> as RelaxedR1CSSNARKTrait<E2<F>>>::ck_floor();

    let pp = SuperNovaPublicParams::<F, C1LEM<'a, F, C>>::setup(
        &non_uniform_circuit,
        &*commitment_size_hint1,
        &*commitment_size_hint2,
    );
    let (pk, vk) = CompressedSNARK::setup(&pp).unwrap();
    PublicParams { pp, pk, vk }
}

/// An enum representing the two types of proofs that can be generated and verified.
#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
pub enum Proof<'a, F: CurveCycleEquipped, C: Coprocessor<F>>
where
    <<E1<F> as Engine>::Scalar as ff::PrimeField>::Repr: Abomonation,
    <<E2<F> as Engine>::Scalar as ff::PrimeField>::Repr: Abomonation,
{
    /// A proof for the intermediate steps of a recursive computation
    Recursive(Box<RecursiveSNARK<E1<F>, E2<F>>>),
    /// A proof for the final step of a recursive computation
    Compressed(
        Box<CompressedSNARK<E1<F>, E2<F>, C1LEM<'a, F, C>, C2<F>, SS1<F>, SS2<F>>>,
        PhantomData<&'a C>,
    ),
}

/// A struct for the Nova prover that operates on field elements of type `F`.
#[derive(Debug)]
pub struct SuperNovaProver<'a, F: CurveCycleEquipped, C: Coprocessor<F> + 'a> {
    /// The number of small-step reductions performed in each recursive step of
    /// the primary Lurk circuit.
    reduction_count: usize,
    lang: Arc<Lang<F, C>>,
    folding_mode: FoldingMode,
    _phantom: PhantomData<&'a ()>,
}

impl<'a, F: CurveCycleEquipped, C: Coprocessor<F> + 'a> SuperNovaProver<'a, F, C> {
    /// Create a new SuperNovaProver with a reduction count and a `Lang`
    #[inline]
    pub fn new(reduction_count: usize, lang: Arc<Lang<F, C>>) -> Self {
        Self {
            reduction_count,
            lang,
            folding_mode: FoldingMode::NIVC,
            _phantom: PhantomData,
        }
    }
}

impl<'a, F: CurveCycleEquipped, C: Coprocessor<F>> RecursiveSNARKTrait<'a, F, C> for Proof<'a, F, C>
where
    <<E1<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
    <<E2<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
{
    type PublicParams = PublicParams<F, C1LEM<'a, F, C>>;

    type ErrorType = SuperNovaError;

    #[tracing::instrument(skip_all, name = "supernova::prove_recursively")]
    fn prove_recursively(
        pp: &PublicParams<F, C1LEM<'a, F, C>>,
        z0: &[F],
        steps: Vec<C1LEM<'a, F, C>>,
        store: &'a Store<F>,
        _reduction_count: usize,
        _lang: Arc<Lang<F, C>>,
    ) -> Result<Self, ProofError> {
        let mut recursive_snark_option: Option<RecursiveSNARK<E1<F>, E2<F>>> = None;

        let z0_primary = z0;
        let z0_secondary = Self::z0_secondary();

        let mut prove_step = |i: usize, step: &C1LEM<'a, F, C>| {
            info!("prove_recursively, step {i}");

            let secondary_circuit = step.secondary_circuit();

            let mut recursive_snark = recursive_snark_option.clone().unwrap_or_else(|| {
                info!("RecursiveSnark::new {i}");
                RecursiveSNARK::new(
                    &pp.pp,
                    step,
                    step,
                    &secondary_circuit,
                    z0_primary,
                    &z0_secondary,
                )
                .unwrap()
            });

            info!("prove_step {i}");

            recursive_snark
                .prove_step(&pp.pp, step, &secondary_circuit)
                .unwrap();

            recursive_snark_option = Some(recursive_snark);
        };

        if lurk_config(None, None)
            .perf
            .parallelism
            .recursive_steps
            .is_parallel()
        {
            let cc = steps
                .into_iter()
                .map(|mf| (mf.program_counter() == 0, Mutex::new(mf)))
                .collect::<Vec<_>>();

            crossbeam::thread::scope(|s| {
                s.spawn(|_| {
                    // Skip the very first circuit's witness, so `prove_step` can begin immediately.
                    // That circuit's witness will not be cached and will just be computed on-demand.

                    // There are many MultiFrames with PC = 0, each with several inner frames and heavy internal
                    // paralellism for witness generation. So we do it like on Nova's pipeline.
                    cc.iter()
                        .skip(1)
                        .filter(|(is_zero_pc, _)| *is_zero_pc)
                        .for_each(|(_, mf)| {
                            mf.lock()
                                .unwrap()
                                .cache_witness(store)
                                .expect("witness caching failed");
                        });

                    // There shouldn't be as many MultiFrames with PC != 0 and they only have one inner frame, each with
                    // poor internal parallelism for witness generation, so we can generate their witnesses in parallel.
                    // This is mimicking the behavior we had in the Nova pipeline before #941 so...
                    // TODO: once we have robust benchmarking for NIVC, we should test whether merging this loop with
                    // the non-parallel one above (and getting rid of the filters) is better
                    cc.par_iter()
                        .skip(1)
                        .filter(|(is_zero_pc, _)| !*is_zero_pc)
                        .for_each(|(_, mf)| {
                            mf.lock()
                                .unwrap()
                                .cache_witness(store)
                                .expect("witness caching failed");
                        });
                });

                for (i, (_, step)) in cc.iter().enumerate() {
                    prove_step(i, &step.lock().unwrap());
                }
            })
            .unwrap()
        } else {
            for (i, step) in steps.iter().enumerate() {
                prove_step(i, step);
            }
        }

        // This probably should be made unnecessary.
        Ok(Self::Recursive(Box::new(
            recursive_snark_option.expect("RecursiveSNARK missing"),
        )))
    }

    fn compress(self, pp: &PublicParams<F, C1LEM<'a, F, C>>) -> Result<Self, ProofError> {
        match &self {
            Self::Recursive(recursive_snark) => Ok(Self::Compressed(
                Box::new(CompressedSNARK::<_, _, _, _, SS1<F>, SS2<F>>::prove(
                    &pp.pp,
                    &pp.pk,
                    recursive_snark,
                )?),
                PhantomData,
            )),
            Self::Compressed(..) => Ok(self),
        }
    }

    fn verify(&self, pp: &Self::PublicParams, z0: &[F], zi: &[F]) -> Result<bool, Self::ErrorType> {
        let (z0_primary, zi_primary) = (z0, zi);
        let z0_secondary = Self::z0_secondary();
        let zi_secondary = &z0_secondary;

        let (zi_primary_verified, zi_secondary_verified) = match self {
            Self::Recursive(p) => p.verify(&pp.pp, z0_primary, &z0_secondary)?,
            Self::Compressed(p, _) => p.verify(&pp.pp, &pp.vk, z0_primary, &z0_secondary)?,
        };

        Ok(zi_primary == zi_primary_verified && zi_secondary == &zi_secondary_verified)
    }
}

impl<'a, F: CurveCycleEquipped, C: Coprocessor<F>> Prover<'a, F, C> for SuperNovaProver<'a, F, C>
where
    <<E1<F> as Engine>::Scalar as ff::PrimeField>::Repr: Abomonation,
    <<E2<F> as Engine>::Scalar as ff::PrimeField>::Repr: Abomonation,
{
    type PublicParams = PublicParams<F, C1LEM<'a, F, C>>;
    type RecursiveSnark = Proof<'a, F, C>;

    #[inline]
    fn reduction_count(&self) -> usize {
        self.reduction_count
    }

    #[inline]
    fn lang(&self) -> &Arc<Lang<F, C>> {
        &self.lang
    }

    #[inline]
    fn folding_mode(&self) -> &FoldingMode {
        &self.folding_mode
    }
}

#[derive(Clone, Debug)]
/// Folding configuration specifies the `Lang`, the reduction count and the
/// folding mode for a proving setup.
///
/// NOTE: This is somewhat trivial now, but will likely become more elaborate as
/// NIVC configuration becomes more flexible.
pub enum FoldingConfig<F: LurkField, C: Coprocessor<F>> {
    // TODO: maybe (lang, reduction_count) should be a common struct.
    /// IVC: a single circuit implementing the `Lang`'s reduction will be used
    /// for every folding step
    IVC(Arc<Lang<F, C>>, usize),
    /// NIVC: each folding step will use one of a fixed set of circuits which
    /// together implement the `Lang`'s reduction.
    NIVC(Arc<Lang<F, C>>, usize),
}

impl<F: LurkField, C: Coprocessor<F>> FoldingConfig<F, C> {
    /// Create a new IVC config for `lang`.
    #[inline]
    pub fn new_ivc(lang: Arc<Lang<F, C>>, reduction_count: usize) -> Self {
        Self::IVC(lang, reduction_count)
    }

    /// Create a new NIVC config for `lang`.
    #[inline]
    pub fn new_nivc(lang: Arc<Lang<F, C>>, reduction_count: usize) -> Self {
        Self::NIVC(lang, reduction_count)
    }

    /// Return the total number of NIVC circuits potentially required when folding
    /// programs described by this `FoldingConfig`.
    pub fn num_circuits(&self) -> usize {
        match self {
            Self::IVC(..) => 1,
            Self::NIVC(lang, _) => 1 + lang.coprocessor_count(),
        }
    }

    /// Return a reference to the contained `Lang`.
    pub fn lang(&self) -> &Arc<Lang<F, C>> {
        match self {
            Self::IVC(lang, _) | Self::NIVC(lang, _) => lang,
        }
    }
    /// Return contained reduction count.
    pub fn reduction_count(&self) -> usize {
        match self {
            Self::IVC(_, rc) | Self::NIVC(_, rc) => *rc,
        }
    }
}

/// Computes a cache key of a supernova primary circuit. The point is that if a
/// circuit changes in any way but has the same `rc`/`Lang`, then we still want
/// the public params to stay in sync with the changes.
///
/// Note: For now, we use ad-hoc circuit cache keys.
/// See: [crate::public_parameters::instance]
pub fn circuit_cache_key<'a, F: CurveCycleEquipped, C: Coprocessor<F> + 'a>(
    rc: usize,
    lang: Arc<Lang<F, C>>,
    circuit_index: usize,
) -> F
where
    <<E1<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
    <<E2<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
{
    let folding_config = Arc::new(FoldingConfig::new_nivc(lang, 2));
    let circuit = C1LEM::<'a, F, C>::blank(folding_config, 0);
    let num_circuits = circuit.num_circuits();
    let circuit = circuit.primary_circuit(circuit_index);
    F::from(rc as u64) * supernova::circuit_digest::<F::E1, F::E2, _>(&circuit, num_circuits)
}

/// Collects all the cache keys of supernova instance. We need all of them to compute
/// a cache key for the digest of the [PublicParams] of the supernova instance.
pub fn circuit_cache_keys<'a, F: CurveCycleEquipped, C: Coprocessor<F> + 'a>(
    rc: usize,
    lang: &Arc<Lang<F, C>>,
) -> CircuitDigests<E1<F>>
where
    <<E1<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
    <<E2<F> as Engine>::Scalar as PrimeField>::Repr: Abomonation,
{
    let num_circuits = lang.coprocessor_count() + 1;
    let digests = (0..num_circuits)
        .map(|circuit_index| circuit_cache_key::<F, C>(rc, lang.clone(), circuit_index))
        .collect();
    CircuitDigests::new(digests)
}

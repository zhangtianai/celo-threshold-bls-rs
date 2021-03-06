use super::{
    board::BoardPublisher,
    primitives::{
        phases::{Phase0, Phase1, Phase2, Phase3},
        types::{BundledJustification, BundledResponses, BundledShares, DKGOutput},
        DKGError,
    },
};

use rand::RngCore;
use thiserror::Error;
use threshold_bls::group::Curve;

#[derive(Debug, Error)]
/// Error thrown while running the DKG or while publishing to the board
pub enum NodeError {
    /// Node could not publish to the board
    #[error("Could not publish to board")]
    PublisherError,
    /// There was an internal error in the DKG
    #[error("DKG Error: {0}")]
    DKGError(#[from] DKGError),
}

/// Phase2 can either be successful or require going to Phase 3.
#[derive(Clone, Debug)]
pub enum Phase2Result<C: Curve, P: Phase3<C>> {
    /// The final DKG output
    Output(DKGOutput<C>),
    /// Indicates that Phase 2 failed and that the protocol must proceed to Phase 3
    GoToPhase3(P),
}

type NodeResult<T> = std::result::Result<T, NodeError>;

/// A DKG Phase.
pub trait DKGPhase<C: Curve, B: BoardPublisher<C>, T> {
    /// The next DKG Phase
    type Next;

    /// Executes this DKG Phase and publishes the required result to the board.
    /// The `arg` is specific to each phase.
    fn run(self, board: &mut B, arg: T) -> NodeResult<Self::Next>;
}

impl<C, B, R, P> DKGPhase<C, B, &mut R> for P
where
    C: Curve,
    B: BoardPublisher<C>,
    R: RngCore,
    P: Phase0<C>,
{
    type Next = P::Next;

    fn run(self, board: &mut B, rng: &mut R) -> NodeResult<Self::Next> {
        let (next, shares) = self.encrypt_shares(rng)?;
        if let Some(sh) = shares {
            board
                .publish_shares(sh)
                .map_err(|_| NodeError::PublisherError)?;
        }

        Ok(next)
    }
}

impl<C, B, P> DKGPhase<C, B, &[BundledShares<C>]> for P
where
    C: Curve,
    B: BoardPublisher<C>,
    P: Phase1<C>,
{
    type Next = P::Next;

    fn run(self, board: &mut B, shares: &[BundledShares<C>]) -> NodeResult<Self::Next> {
        let (next, bundle) = self.process_shares(shares, false)?;

        if let Some(bundle) = bundle {
            board
                .publish_responses(bundle)
                .map_err(|_| NodeError::PublisherError)?;
        }

        Ok(next)
    }
}

impl<C, B, P> DKGPhase<C, B, &[BundledResponses]> for P
where
    C: Curve,
    B: BoardPublisher<C>,
    P: Phase2<C>,
{
    type Next = Phase2Result<C, P::Next>;

    fn run(self, board: &mut B, responses: &[BundledResponses]) -> NodeResult<Self::Next> {
        match self.process_responses(responses) {
            Ok(output) => Ok(Phase2Result::Output(output)),
            Err(next) => {
                match next {
                    Ok((next, justifications)) => {
                        // publish justifications if you have some
                        // Nodes may just see that justifications are needed but they
                        // don't have to create any, since no  complaint have been filed
                        // against their deal.
                        if let Some(justifications) = justifications {
                            board
                                .publish_justifications(justifications)
                                .map_err(|_| NodeError::PublisherError)?;
                        }

                        Ok(Phase2Result::GoToPhase3(next))
                    }
                    Err(e) => Err(NodeError::DKGError(e)),
                }
            }
        }
    }
}

impl<C, B, P> DKGPhase<C, B, &[BundledJustification<C>]> for P
where
    C: Curve,
    B: BoardPublisher<C>,
    P: Phase3<C>,
{
    type Next = DKGOutput<C>;

    fn run(self, _: &mut B, responses: &[BundledJustification<C>]) -> NodeResult<Self::Next> {
        Ok(self.process_justifications(responses)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        primitives::{
            group::{Group, Node},
            joint_feldman,
        },
        test_helpers::InMemoryBoard,
    };

    use threshold_bls::{
        curve::bls12381::{self, PairingCurve as BLS12_381},
        curve::zexe::{self as bls12_377, PairingCurve as BLS12_377},
        poly::Idx,
        sig::{BlindThresholdScheme, G1Scheme, G2Scheme, Scheme, SignatureScheme, ThresholdScheme},
    };
    // helper to simulate a phase0 where a participant does not publish their
    // shares to the board
    fn bad_phase0<C: Curve, R: RngCore, P: Phase0<C>>(phase0: P, rng: &mut R) -> P::Next {
        let (next, _) = phase0.encrypt_shares(rng).unwrap();
        next
    }

    #[test]
    fn dkg_sign_e2e() {
        let (t, n) = (3, 5);
        dkg_sign_e2e_curve::<bls12381::Curve, G1Scheme<BLS12_381>>(n, t);
        dkg_sign_e2e_curve::<bls12381::G2Curve, G2Scheme<BLS12_381>>(n, t);

        dkg_sign_e2e_curve::<bls12_377::G1Curve, G1Scheme<BLS12_377>>(n, t);
        dkg_sign_e2e_curve::<bls12_377::G2Curve, G2Scheme<BLS12_377>>(n, t);
    }

    fn dkg_sign_e2e_curve<C, S>(n: usize, t: usize)
    where
        C: Curve,
        // We need to bind the Curve's Point and Scalars to the Scheme
        S: Scheme<Public = <C as Curve>::Point, Private = <C as Curve>::Scalar>
            + BlindThresholdScheme
            + ThresholdScheme
            + SignatureScheme,
    {
        let msg = rand::random::<[u8; 32]>().to_vec();

        // executes the DKG state machine and ensures that the keys are generated correctly
        let outputs = run_dkg::<C, S>(n, t);

        // blinds the message
        let (token, blinded_msg) = S::blind_msg(&msg[..], &mut rand::thread_rng());

        // generates a partial sig with each share from the dkg
        let partial_sigs = outputs
            .iter()
            .map(|output| S::sign_blind_partial(&output.share, &blinded_msg[..]).unwrap())
            .collect::<Vec<_>>();

        // aggregates them
        let blinded_sig = S::aggregate(t, &partial_sigs).unwrap();

        // the user unblinds it
        let unblinded_sig = S::unblind_sig(&token, &blinded_sig).unwrap();

        // get the public key (we have already checked that all outputs' pubkeys are the same)
        let pubkey = outputs[0].public.public_key();

        // verify the threshold signature
        S::verify(&pubkey, &msg, &unblinded_sig).unwrap();
    }

    fn run_dkg<C, S>(n: usize, t: usize) -> Vec<DKGOutput<C>>
    where
        C: Curve,
        // We need to bind the Curve's Point and Scalars to the Scheme
        S: Scheme<Public = <C as Curve>::Point, Private = <C as Curve>::Scalar>,
    {
        let rng = &mut rand::thread_rng();

        let (mut board, phase0s) = setup::<C, S, _>(n, t, rng);

        // Phase 1: Publishes shares
        let phase1s = phase0s
            .into_iter()
            .map(|phase0| phase0.run(&mut board, rng).unwrap())
            .collect::<Vec<_>>();

        // Get the shares from the board
        let shares = board.shares.clone();

        // Phase2
        let phase2s = phase1s
            .into_iter()
            .map(|phase1| phase1.run(&mut board, &shares).unwrap())
            .collect::<Vec<_>>();

        // Get the responses from the board
        let responses = board.responses.clone();

        let results = phase2s
            .into_iter()
            .map(|phase2| phase2.run(&mut board, &responses).unwrap())
            .collect::<Vec<_>>();

        // The distributed public key must be the same
        let outputs = results
            .into_iter()
            .map(|res| match res {
                Phase2Result::Output(out) => out,
                Phase2Result::GoToPhase3(_) => unreachable!("should not get here"),
            })
            .collect::<Vec<_>>();
        assert!(is_all_same(outputs.iter().map(|output| &output.public)));

        outputs
    }

    #[test]
    fn not_enough_validator_shares() {
        let (t, n) = (6, 10);
        let bad = t + 1;
        let honest = n - bad;

        let rng = &mut rand::thread_rng();
        let (mut board, phase0s) = setup::<bls12_377::G1Curve, G1Scheme<BLS12_377>, _>(n, t, rng);

        let phase1s = phase0s
            .into_iter()
            .enumerate()
            .map(|(i, phase0)| {
                if i < bad {
                    bad_phase0(phase0, rng)
                } else {
                    phase0.run(&mut board, rng).unwrap()
                }
            })
            .collect::<Vec<_>>();

        // Get the shares from the board
        let shares = board.shares.clone();

        // Phase2 fails (only the good ones try to run it)
        let errs = phase1s
            .into_iter()
            .map(|phase1| phase1.run(&mut board, &shares).unwrap_err())
            .map(|err| match err {
                NodeError::DKGError(err) => err,
                _ => panic!("should get dkg error"),
            })
            .collect::<Vec<_>>();

        // bad contributors who try to contribute in P2 without contributing in P1
        // will get `honest`
        for err in &errs[..bad] {
            match err {
                DKGError::NotEnoughValidShares(got, required) => {
                    assert_eq!(*got, honest);
                    assert_eq!(*required, t);
                }
                _ => panic!("should not get here"),
            };
        }

        // the honest participants should have received `honest - 1` shares
        // (which were not enough)
        for err in &errs[bad..] {
            match err {
                DKGError::NotEnoughValidShares(got, required) => {
                    assert_eq!(*got, honest - 1);
                    assert_eq!(*required, t);
                }
                _ => panic!("should not get here"),
            };
        }
    }

    #[test]
    fn dkg_phase3() {
        let (t, n) = (5, 8);
        let bad = 2; // >0 people not broadcasting in the start force us to go to phase 3

        let rng = &mut rand::thread_rng();
        let (mut board, phase0s) = setup::<bls12_377::G1Curve, G1Scheme<BLS12_377>, _>(n, t, rng);

        let phase1s = phase0s
            .into_iter()
            .enumerate()
            .map(|(i, phase0)| {
                // some participants decide not to broadcast their share.
                // this forces us to go to phase 3
                if i < bad {
                    bad_phase0(phase0, rng)
                } else {
                    phase0.run(&mut board, rng).unwrap()
                }
            })
            .collect::<Vec<_>>();

        // Get the shares from the board
        let shares = board.shares.clone();

        // Phase2 runs but not enough were published
        let phase2s = phase1s
            .into_iter()
            .map(|phase1| phase1.run(&mut board, &shares).unwrap())
            .collect::<Vec<_>>();

        // Get the responses from the board
        let responses = board.responses.clone();

        let results = phase2s
            .into_iter()
            .map(|phase2| phase2.run(&mut board, &responses).unwrap())
            .collect::<Vec<_>>();

        let phase3s = results
            .into_iter()
            .map(|res| match res {
                Phase2Result::GoToPhase3(p3) => p3,
                _ => unreachable!("should not get here"),
            })
            .collect::<Vec<_>>();

        let justifications = board.justifs.clone();

        let outputs = phase3s
            .into_iter()
            .map(|phase3| phase3.run(&mut board, &justifications).unwrap())
            .collect::<Vec<_>>();

        // everyone knows who qualified correctly and who did not
        assert!(is_all_same(outputs.iter().map(|output| &output.qual)));

        // excluding the first people did not publish, the rest are the same
        assert!(is_all_same(
            outputs[bad..].iter().map(|output| &output.public)
        ));

        // the first people must have a different public key from the others
        let pubkey = &outputs[bad].public;
        for output in &outputs[..bad] {
            assert_ne!(&output.public, pubkey);
        }
    }

    fn setup<C, S, R: rand::RngCore>(
        n: usize,
        t: usize,
        rng: &mut R,
    ) -> (InMemoryBoard<C>, Vec<joint_feldman::DKG<C>>)
    where
        C: Curve,
        // We need to bind the Curve's Point and Scalars to the Scheme
        S: Scheme<Public = <C as Curve>::Point, Private = <C as Curve>::Scalar>,
    {
        // generate a keypair per participant
        let keypairs = (0..n).map(|_| S::keypair(rng)).collect::<Vec<_>>();

        let nodes = keypairs
            .iter()
            .enumerate()
            .map(|(i, (_, public))| Node::<C>::new(i as Idx, public.clone()))
            .collect::<Vec<_>>();

        // This is setup phase during which publickeys and indexes must be exchanged
        // across participants
        let group = Group::new(nodes, t).unwrap();

        // Create the Phase 0 for each participant
        let phase0s = keypairs
            .iter()
            .map(|(private, _)| joint_feldman::DKG::new(private.clone(), group.clone()).unwrap())
            .collect::<Vec<_>>();

        // Create the board
        let board = InMemoryBoard::<C>::new();

        (board, phase0s)
    }

    fn is_all_same<T: PartialEq>(mut arr: impl Iterator<Item = T>) -> bool {
        let first = arr.next().unwrap();
        arr.all(|item| item == first)
    }
}

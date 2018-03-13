#![allow(non_snake_case)]

use std::iter;
use std::borrow::Borrow;

use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::ristretto;
use curve25519_dalek::scalar::Scalar;

// XXX upstream into dalek
use scalar;

use proof_transcript::ProofTranscript;

use util;

use range_proof::inner_product;
use range_proof::make_generators;

use sha2::Sha256;

#[derive(Clone, Debug)]
pub struct Proof {
    L_vec: Vec<RistrettoPoint>,
    R_vec: Vec<RistrettoPoint>,
    a: Scalar,
    b: Scalar,
}

impl Proof {
    /// Create an inner-product proof.
    ///
    /// The proof is created with respect to the bases G, Hprime,
    /// where Hprime[i] = H[i] * Hprime_factors[i].
    ///
    /// The `verifier` is passed in as a parameter so that the
    /// challenges depend on the *entire* transcript (including parent
    /// protocols).
    pub fn create<I>(
        verifier: &mut ProofTranscript,
        Q: &RistrettoPoint,
        Hprime_factors: I,
        mut G_vec: Vec<RistrettoPoint>,
        mut H_vec: Vec<RistrettoPoint>,
        mut a_vec: Vec<Scalar>,
        mut b_vec: Vec<Scalar>,
    ) -> Proof
    where
        I: IntoIterator,
        I::Item: Borrow<Scalar>,
    {
        // Create slices G, H, a, b backed by their respective
        // vectors.  This lets us reslice as we compress the lengths
        // of the vectors in the main loop below.
        let mut G = &mut G_vec[..];
        let mut H = &mut H_vec[..];
        let mut a = &mut a_vec[..];
        let mut b = &mut b_vec[..];

        let mut n = G.len();

        // All of the input vectors must have the same length.
        assert_eq!(G.len(), n);
        assert_eq!(H.len(), n);
        assert_eq!(a.len(), n);
        assert_eq!(b.len(), n);

        // XXX save these scalar mults by unrolling them into the
        // first iteration of the loop below
        for (H_i, h_i) in H.iter_mut().zip(Hprime_factors.into_iter()) {
            *H_i = (&*H_i) * h_i.borrow();
        }

        let lg_n = n.next_power_of_two().trailing_zeros() as usize;
        let mut L_vec = Vec::with_capacity(lg_n);
        let mut R_vec = Vec::with_capacity(lg_n);

        while n != 1 {
            n = n / 2;
            let (a_L, a_R) = a.split_at_mut(n);
            let (b_L, b_R) = b.split_at_mut(n);
            let (G_L, G_R) = G.split_at_mut(n);
            let (H_L, H_R) = H.split_at_mut(n);

            let c_L = inner_product(&a_L, &b_R);
            let c_R = inner_product(&a_R, &b_L);

            let L = ristretto::vartime::multiscalar_mult(
                a_L.iter().chain(b_R.iter()).chain(iter::once(&c_L)),
                G_R.iter().chain(H_L.iter()).chain(iter::once(Q)),
            );

            let R = ristretto::vartime::multiscalar_mult(
                a_R.iter().chain(b_L.iter()).chain(iter::once(&c_R)),
                G_L.iter().chain(H_R.iter()).chain(iter::once(Q)),
            );

            L_vec.push(L);
            R_vec.push(R);

            verifier.commit(L.compress().as_bytes());
            verifier.commit(R.compress().as_bytes());

            let x = verifier.challenge_scalar();
            let x_inv = x.invert();

            for i in 0..n {
                a_L[i] = a_L[i] * x + x_inv * a_R[i];
                b_L[i] = b_L[i] * x_inv + x * b_R[i];
                G_L[i] = ristretto::vartime::multiscalar_mult(&[x_inv, x], &[G_L[i], G_R[i]]);
                H_L[i] = ristretto::vartime::multiscalar_mult(&[x, x_inv], &[H_L[i], H_R[i]]);
            }

            a = a_L;
            b = b_L;
            G = G_L;
            H = H_L;
        }

        return Proof {
            L_vec: L_vec,
            R_vec: R_vec,
            a: a[0],
            b: b[0],
        };
    }

    pub fn verify<I>(
        &self,
        verifier: &mut ProofTranscript,
        Hprime_factors: I,
        P: &RistrettoPoint,
        Q: &RistrettoPoint,
        G_vec: &Vec<RistrettoPoint>,
        H_vec: &Vec<RistrettoPoint>,
    ) -> Result<(), ()>
    where
        I: IntoIterator,
        I::Item: Borrow<Scalar>,
    {
        // XXX prover should commit to n
        let lg_n = self.L_vec.len();
        let n = 1 << lg_n;

        // XXX figure out how ser/deser works for Proofs
        // maybe avoid this compression
        let mut challenges = Vec::with_capacity(lg_n);
        for (L, R) in self.L_vec.iter().zip(self.R_vec.iter()) {
            verifier.commit(L.compress().as_bytes());
            verifier.commit(R.compress().as_bytes());

            challenges.push(verifier.challenge_scalar());
        }

        let mut inv_challenges = challenges.clone();
        let allinv = scalar::batch_invert(&mut inv_challenges);

        for x in challenges.iter_mut() {
            *x = &*x * &*x; // wtf
        }
        let challenges_sq = challenges;

        // j-th bit of i
        let bit = |i, j| 1 & (i >> j);

        let mut s = Vec::with_capacity(n);
        for i in 0..n {
            let mut s_i = allinv;
            // XXX remove this loop via the bit twiddling mentioned in the paper
            for j in 0..lg_n {
                if bit(i, j) == 1 {
                    // The challenges are stored in "creation order" as [x_k,...,x_1]
                    s_i *= challenges_sq[(lg_n - 1) - j];
                }
            }
            s.push(s_i);
        }
        let s = s;

        let a_times_s = s.iter().map(|s_i| self.a * s_i);

        // 1/s[i] is s[!i], and !i runs from n-1 to 0 as i runs from 0 to n-1
        let inv_s = s.iter().rev();

        let h_times_b_div_s = Hprime_factors
            .into_iter()
            .zip(inv_s)
            .map(|(h_i, s_i_inv)| (self.b * s_i_inv) * h_i.borrow());

        let neg_x_sq = challenges_sq.iter().map(|x| -x);

        let neg_x_inv_sq = inv_challenges.iter().map(|x_inv| -(x_inv * x_inv));

        let expect_P = ristretto::vartime::multiscalar_mult(
            iter::once(self.a * self.b)
                .chain(a_times_s)
                .chain(h_times_b_div_s)
                .chain(neg_x_sq)
                .chain(neg_x_inv_sq),
            iter::once(Q)
                .chain(G_vec.iter())
                .chain(H_vec.iter())
                .chain(self.L_vec.iter())
                .chain(self.R_vec.iter()),
        );

        if expect_P == *P {
            Ok(())
        } else {
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rand::OsRng;

    fn test_helper_create(n: usize) {
        let mut rng = OsRng::new().unwrap();

        // XXX fix up generators
        let B = &RistrettoPoint::hash_from_bytes::<Sha256>("hello".as_bytes());
        let B_blinding = &RistrettoPoint::hash_from_bytes::<Sha256>("there".as_bytes());
        let G = make_generators(B, n);
        let H = make_generators(B_blinding, n);

        // Q would be determined upstream in the protocol, so we pick a random one.
        let Q = RistrettoPoint::hash_from_bytes::<Sha256>(b"test point");

        // a and b are the vectors for which we want to prove c = <a,b>
        let a: Vec<_> = (0..n).map(|_| Scalar::random(&mut rng)).collect();
        let b: Vec<_> = (0..n).map(|_| Scalar::random(&mut rng)).collect();
        let c = inner_product(&a, &b);

        // y_inv is (the inverse of) a random challenge
        let y_inv = Scalar::random(&mut rng);

        // P would be determined upstream, but we need a correct P to check the proof.
        //
        // To generate P = <a,G> + <b,H'> + <a,b> Q, compute
        //             P = <a,G> + <b',H> + <a,b> Q,
        // where b' = b \circ y^(-n)
        let b_prime = b.iter().zip(util::exp_iter(y_inv)).map(|(bi, yi)| bi * yi);
        // a.iter() has Item=&Scalar, need Item=Scalar to chain with b_prime
        let a_prime = a.iter().cloned();

        let P = ristretto::vartime::multiscalar_mult(
            a_prime.chain(b_prime).chain(iter::once(c)),
            G.iter().chain(H.iter()).chain(iter::once(&Q)),
        );

        let mut verifier = ProofTranscript::new(b"innerproducttest");
        let proof = Proof::create(
            &mut verifier,
            &Q,
            util::exp_iter(y_inv),
            G.clone(),
            H.clone(),
            a.clone(),
            b.clone(),
        );

        let mut verifier = ProofTranscript::new(b"innerproducttest");
        assert!(
            proof
                .verify(&mut verifier, util::exp_iter(y_inv), &P, &Q, &G, &H)
                .is_ok()
        );
    }

    #[test]
    fn make_ipp_1() {
        test_helper_create(1);
    }

    #[test]
    fn make_ipp_2() {
        test_helper_create(2);
    }

    #[test]
    fn make_ipp_4() {
        test_helper_create(4);
    }

    #[test]
    fn make_ipp_32() {
        test_helper_create(32);
    }

    #[test]
    fn make_ipp_64() {
        test_helper_create(64);
    }
}

#[cfg(test)]
mod bench {

    use super::*;
    use test::Bencher;

    fn bench_helper_create(n: usize, b: &mut Bencher) {
        let mut verifier = ProofTranscript::new(b"innerproducttest");
        let G = &RistrettoPoint::hash_from_bytes::<Sha256>("hello".as_bytes());
        let H = &RistrettoPoint::hash_from_bytes::<Sha256>("there".as_bytes());
        let G_vec = make_generators(G, n);
        let H_vec = make_generators(H, n);
        let Q = RistrettoPoint::hash_from_bytes::<Sha256>("more".as_bytes());
        let P = RistrettoPoint::hash_from_bytes::<Sha256>("points".as_bytes());
        let a_vec = vec![Scalar::from_u64(1); n];
        let b_vec = vec![Scalar::from_u64(2); n];
        let ones = vec![Scalar::from_u64(1); n];

        b.iter(|| {
            Proof::create(
                &mut verifier,
                &Q,
                &ones,
                G_vec.clone(),
                H_vec.clone(),
                a_vec.clone(),
                b_vec.clone(),
            )
        });
    }

    fn bench_helper_verify(n: usize, b: &mut Bencher) {
        let mut verifier = ProofTranscript::new(b"innerproducttest");
        let G = &RistrettoPoint::hash_from_bytes::<Sha256>("hello".as_bytes());
        let H = &RistrettoPoint::hash_from_bytes::<Sha256>("there".as_bytes());
        let G_vec = make_generators(G, n);
        let H_vec = make_generators(H, n);

        let a_vec = vec![Scalar::from_u64(1); n];
        let b_vec = vec![Scalar::from_u64(2); n];

        let Q = RistrettoPoint::hash_from_bytes::<Sha256>(b"test point");
        let c = inner_product(&a_vec, &b_vec);

        let P = ristretto::vartime::multiscalar_mult(
            a_vec.iter().chain(b_vec.iter()).chain(iter::once(&c)),
            G_vec.iter().chain(H_vec.iter()).chain(iter::once(&Q)),
        );

        let ones = vec![Scalar::from_u64(1); n];

        let proof = Proof::create(
            &mut verifier,
            &Q,
            &ones,
            G_vec.clone(),
            H_vec.clone(),
            a_vec.clone(),
            b_vec.clone(),
        );

        let mut verifier = ProofTranscript::new(b"innerproducttest");
        b.iter(|| proof.verify(&mut verifier, &ones, &P, &Q, &G_vec, &H_vec));
    }

    #[bench]
    fn create_n_eq_64(b: &mut Bencher) {
        bench_helper_create(64, b);
    }

    #[bench]
    fn create_n_eq_32(b: &mut Bencher) {
        bench_helper_create(32, b);
    }

    #[bench]
    fn create_n_eq_16(b: &mut Bencher) {
        bench_helper_create(16, b);
    }

    #[bench]
    fn verify_n_eq_64(b: &mut Bencher) {
        bench_helper_verify(64, b);
    }

    #[bench]
    fn verify_n_eq_32(b: &mut Bencher) {
        bench_helper_verify(32, b);
    }

    #[bench]
    fn verify_n_eq_16(b: &mut Bencher) {
        bench_helper_verify(16, b);
    }
}

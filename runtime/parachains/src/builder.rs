use crate::paras_inherent::{self};
use crate::{configuration, inclusion, initializer, paras, scheduler, session_info, shared};
use bitvec::{order::Lsb0 as BitOrderLsb0, vec::BitVec};
use frame_benchmarking::{account, benchmarks, impl_benchmark_test_suite};
use frame_system::RawOrigin;
use primitives::v1::{
	byzantine_threshold, collator_signature_payload, AvailabilityBitfield, CandidateCommitments,
	CandidateDescriptor, CandidateHash, CollatorId, CommittedCandidateReceipt, CompactStatement,
	CoreIndex, CoreOccupied, DisputeStatement, DisputeStatementSet, GroupIndex, HeadData,
	Id as ParaId, InvalidDisputeStatementKind, PersistedValidationData, SessionIndex,
	SigningContext, UncheckedSigned, ValidDisputeStatementKind, ValidatorId, ValidatorIndex,
	ValidityAttestation, ValidationCode, BackedCandidate, InherentData as ParachainsInherentData,
};
use sp_core::H256;
use sp_runtime::{
	generic::Digest,
	traits::{One, Zero},
	RuntimeAppPublic,
};
use sp_runtime::traits::Header as HeaderT;
use sp_std::{collections::btree_set::BTreeSet, convert::TryInto};
use frame_support::pallet_prelude::*;

const LOG_TARGET: &str = "runtime::paras-runtime-test-builder";

fn byte32_slice_from(n: u32) -> [u8; 32] {
	let mut slice = [0u8; 32];
	slice[31] = (n % (1 << 8)) as u8;
	slice[30] = ((n >> 8) % (1 << 8)) as u8;
	slice[29] = ((n >> 16) % (1 << 8)) as u8;
	slice[28] = ((n >> 24) % (1 << 8)) as u8;

	slice
}

// Brainstorming worst case aspects:
//
// - there are many fresh disputes, where the disputes have just been initiated.
// - create a new `DisputeState` with blank bitfields.
// - make sure spam slots is maxed out without being cleared
// - force one side to have a super majority, so we enable slashing <-- TODO
//
/// Paras inherent `enter` benchmark scenario builder.
pub(crate) struct BenchBuilder<T: paras_inherent::Config> {
	validators: Option<Vec<ValidatorId>>,
	block_number: T::BlockNumber,
	session: SessionIndex,
	_phantom: sp_std::marker::PhantomData<T>,
}

/// Paras inherent `enter` benchmark scenario.
pub(crate) struct Bench<T: paras_inherent::Config> {
	pub(crate) data: ParachainsInherentData<T::Header>,
}

impl<T: paras_inherent::Config> BenchBuilder<T> {
	pub(crate) fn new() -> Self {
		BenchBuilder {
			validators: None,
			block_number: Zero::zero(),
			session: SessionIndex::from(0u32),
			_phantom: sp_std::marker::PhantomData::<T>,
		}
	}

	/// Mock header
	fn header(block_number: T::BlockNumber) -> T::Header {
		T::Header::new(
			block_number,       // block_number,
			Default::default(), // extrinsics_root,
			Default::default(), // storage_root,
			Default::default(), // parent_hash,
			Default::default(), // digest,
		)
	}

	fn relay_parent_number(&self) -> u32 {
		(self.block_number - One::one())
			.try_into()
			.map_err(|_| ())
			.expect("self.block_number is u32")
	}

	fn create_indexes(&self, seed: u32) -> (ParaId, CoreIndex, GroupIndex) {
		let para_id = ParaId::from(seed);
		let core_idx = CoreIndex(seed);
		let group_idx =
			scheduler::Pallet::<T>::group_assigned_to_core(core_idx, self.block_number).unwrap();

		(para_id, core_idx, group_idx)
	}

	fn candidate_availability_mock(
		group_idx: GroupIndex,
		core_idx: CoreIndex,
		candidate_hash: CandidateHash,
		availability_votes: BitVec<BitOrderLsb0, u8>,
	) -> inclusion::CandidatePendingAvailability<T::Hash, T::BlockNumber> {
		inclusion::CandidatePendingAvailability::<T::Hash, T::BlockNumber>::new(
			core_idx,
			candidate_hash,
			Default::default(),
			availability_votes,
			Default::default(),
			Zero::zero(),
			One::one(),
			group_idx,
		)
	}

	fn add_availability(
		para_id: ParaId,
		core_idx: CoreIndex,
		group_idx: GroupIndex,
		availability_votes: BitVec<BitOrderLsb0, u8>,
		candidate_hash: CandidateHash,
	) {
		let candidate_availability = Self::candidate_availability_mock(
			group_idx,
			core_idx,
			candidate_hash,
			availability_votes,
		);
		// TODO notes: commitments does not include any data that would lead to heavy code
		// paths in `enact_candidate`. But enact_candidates does return a weight so maybe
		// that should be used. (Relevant for when bitfields indicate a candidate is available)
		let commitments = CandidateCommitments::<u32>::default();
		inclusion::PendingAvailability::<T>::insert(para_id, candidate_availability);
		inclusion::PendingAvailabilityCommitments::<T>::insert(&para_id, commitments);
	}

	fn availability_bitvec(concluding: &BTreeSet<u32>) -> AvailabilityBitfield {
		let mut bitfields = bitvec::bitvec![bitvec::order::Lsb0, u8; 0; 0];
		for i in 0..Self::cores() {
			// the first `availability` cores are marked as available
			if concluding.contains(&(i as u32)) {
				bitfields.push(true);
			} else {
				bitfields.push(false)
			}
		}

		bitfields.into()
	}

	fn run_to_block(to: u32) {
		let to = to.into();
		while frame_system::Pallet::<T>::block_number() < to {
			let b = frame_system::Pallet::<T>::block_number();
			initializer::Pallet::<T>::on_finalize(b);

			let b = b + One::one();
			frame_system::Pallet::<T>::set_block_number(b);
			initializer::Pallet::<T>::on_initialize(b);
		}
	}

	/// Insert para ids into `paras::Parachains`.
	fn setup_para_ids(cores: u32) {
		// make sure parachains exist prior to session change.
		for i in 0..cores {
			let para_id = ParaId::from(i as u32);

			paras::Pallet::<T>::schedule_para_initialize(
				para_id,
				paras::ParaGenesisArgs {
					genesis_head: Default::default(),
					validation_code: Default::default(),
					parachain: true,
				},
			)
			.unwrap();
		}
	}

	/// Generate validator key pairs and account ids.
	fn generate_validator_pairs(validator_count: u32) -> Vec<(T::AccountId, ValidatorId)> {
		(0..validator_count)
			.map(|i| {
				let public = ValidatorId::generate_pair(None);

				// this account is not actually used anywhere, just necessary to fulfill expected type
				// `validators` param of `test_trigger_on_new_session`.
				let account: T::AccountId = account("validator", i, i);
				(account, public)
			})
			.collect()
	}

	fn signing_context(&self) -> SigningContext<T::Hash> {
		SigningContext {
			parent_hash: Self::header(self.block_number.clone()).hash(),
			session_index: self.session.clone(),
		}
	}

	fn max_validators() -> u32 {
		let config_max = configuration::Pallet::<T>::config().max_validators.unwrap_or(200);
		config_max
	}

	fn max_validators_per_core() -> u32 {
		configuration::Pallet::<T>::config().max_validators_per_core.unwrap_or(5)
	}

	pub(crate) fn cores() -> u32 {
		Self::max_validators() / Self::max_validators_per_core()
	}

	fn max_statements() -> u32 {
		Self::max_validators()
	}

	/// Byzantine statement spam threshold.
	fn statement_spam_thresh() -> u32 {
		byzantine_threshold(Self::max_statements() as usize) as u32
	}

	fn validator_availability_votes_yes() -> BitVec<bitvec::order::Lsb0, u8> {
		// every validator confirms availability.
		bitvec::bitvec![bitvec::order::Lsb0, u8; 1; Self::max_validators() as usize]
	}

	/// Setup session 1 and create `self.validators_map` and `self.validators`.
	fn setup_session(
		mut self,
		target_session: SessionIndex,
		validators: Vec<(T::AccountId, ValidatorId)>,
	) -> Self {
		let mut block = 1;
		for session in 0..=target_session {
			// initialize session 1.
			if block == 0 {
				initializer::Pallet::<T>::test_trigger_on_new_session(
					true, // indicate the validator set has changed because there are no validators in the system yet
					session, // session index
					Vec::new().into_iter(), // There are currently no validators
					Some(validators.iter().map(|(a, v)| (a, v.clone())).collect::<Vec<_>>())
						.map(|v| v.into_iter()), // validators
				);
			} else {
				// initialize session 2.
				initializer::Pallet::<T>::test_trigger_on_new_session(
					false,                                          // indicate the validator set has changed
					session,                                        // session index
					validators.iter().map(|(a, v)| (a, v.clone())), // We don't want to change the validator set
					None, // queued - when this is None validators are considered queued
				);
			}
			block += 1;
			Self::run_to_block(block);
		}

		let block_number = <T as frame_system::Config>::BlockNumber::from(block);
		let header = Self::header(block_number.clone());

		frame_system::Pallet::<T>::initialize(
			&header.number(),
			&header.hash(),
			&Digest::<T::Hash> { logs: Vec::new() },
			Default::default(),
		);

		// confirm setup at session change.
		// assert_eq!(scheduler::AvailabilityCores::<T>::get().len(), Self::cores() as usize);
		assert_eq!(scheduler::ValidatorGroups::<T>::get().len(), Self::cores() as usize);
		assert_eq!(<shared::Pallet<T>>::session_index(), target_session);

		log::info!(target: LOG_TARGET, "b");

		// get validators from session info. We need to refetch them since they have been shuffled.
		let validators_shuffled: Vec<_> = session_info::Pallet::<T>::session_info(target_session)
			.unwrap()
			.validators
			.clone()
			.into_iter()
			.enumerate()
			.map(|(val_idx, public)| {
				// TODO we don't actually need to map here anymore, can just to a for loop to
				// sanity check things.
				{
					// sanity check that the validator keys line up as expected.
					let active_val_keys = shared::Pallet::<T>::active_validator_keys();
					let public_check = active_val_keys.get(val_idx).unwrap();
					assert_eq!(public, *public_check);
				}

				public
			})
			.collect();

		self.validators = Some(validators_shuffled);
		self.block_number = block_number;
		self.session = target_session;
		assert_eq!(paras::Pallet::<T>::parachains().len(), Self::cores() as usize);

		self
	}

	/// Marks `concluding_cores` as fully available.
	fn create_availability_bitfields(
		&self,
		concluding_cores: BTreeSet<u32>,
	) -> Vec<UncheckedSigned<AvailabilityBitfield>> {
		let validators =
			self.validators.as_ref().expect("must have some validators prior to calling");

		let availability_bitvec = Self::availability_bitvec(&concluding_cores);

		let bitfields: Vec<UncheckedSigned<AvailabilityBitfield>> = validators
			.iter()
			.enumerate()
			.map(|(i, public)| {
				let unchecked_signed = UncheckedSigned::<AvailabilityBitfield>::benchmark_sign(
					public,
					availability_bitvec.clone(),
					&self.signing_context(),
					ValidatorIndex(i as u32),
				);

				unchecked_signed
			})
			.collect();

		for seed in concluding_cores.iter() {
			// make sure the candidates that are concluding by becoming available are marked as
			// pending availability.
			let (para_id, core_idx, group_idx) = self.create_indexes(seed.clone());
			Self::add_availability(
				para_id,
				core_idx,
				group_idx,
				Self::validator_availability_votes_yes(),
				CandidateHash(H256::from(byte32_slice_from(seed.clone()))),
			);

			scheduler::AvailabilityCores::<T>::mutate(|cores| {
				cores[*seed as usize] = Some(CoreOccupied::Parachain)
			});
		}

		bitfields
	}

	/// Create backed candidates for `cores_with_backed_candidates`.
	// TODO currently you need these cores to be scheduled within paras inherent, which requires marking
	// the available bitfields as fully available.
	fn create_backed_candidates(
		&self,
		cores_with_backed_candidates: BTreeSet<u32>,
	) -> Vec<BackedCandidate<T::Hash>> {
		let validators =
			self.validators.as_ref().expect("must have some validators prior to calling");
		let config = configuration::Pallet::<T>::config();

		cores_with_backed_candidates
			.iter()
			.map(|seed| {
				let (para_id, _core_idx, group_idx) = self.create_indexes(seed.clone());

				// generate a pair and add it to the keystore.
				let collator_public = CollatorId::generate_pair(None);
				let header = Self::header(self.block_number.clone());
				let relay_parent = header.hash();
				let head_data: HeadData = Default::default();
				let persisted_validation_data_hash = PersistedValidationData::<H256> {
					parent_head: head_data.clone(),
					relay_parent_number: self.relay_parent_number(),
					relay_parent_storage_root: Default::default(),
					max_pov_size: config.max_pov_size,
				}
				.hash();

				let pov_hash = Default::default();
				// note that we use the default `ValidationCode` when setting it in `setup_para_ids`.
				let validation_code_hash = ValidationCode::default().hash();
				let payload = collator_signature_payload(
					&relay_parent,
					&para_id,
					&persisted_validation_data_hash,
					&pov_hash,
					&validation_code_hash,
				);
				let signature = collator_public.sign(&payload).unwrap();

				// set the head data so it can be used while validating the signatures on the candidate
				// receipt.
				paras::Pallet::<T>::heads_insert(&para_id, head_data.clone());

				let mut past_code_meta = paras::ParaPastCodeMeta::<T::BlockNumber>::default();
				past_code_meta.note_replacement(0u32.into(), 0u32.into());

				let group_validators = scheduler::Pallet::<T>::group_validators(group_idx).unwrap();

				let candidate = CommittedCandidateReceipt::<T::Hash> {
					descriptor: CandidateDescriptor::<T::Hash> {
						para_id,
						relay_parent,
						collator: collator_public,
						persisted_validation_data_hash,
						pov_hash,
						erasure_root: Default::default(),
						signature,
						para_head: head_data.hash(),
						validation_code_hash,
					},
					commitments: CandidateCommitments::<u32> {
						upward_messages: Vec::new(),
						horizontal_messages: Vec::new(),
						new_validation_code: None,
						head_data, // HeadData
						processed_downward_messages: 0,
						hrmp_watermark: self.relay_parent_number(),
					},
				};

				let candidate_hash = candidate.hash();

				let validity_votes: Vec<_> = group_validators
					.iter()
					.map(|val_idx| {
						let public = validators.get(val_idx.0 as usize).unwrap();
						let sig = UncheckedSigned::<CompactStatement>::benchmark_sign(
							public,
							CompactStatement::Valid(candidate_hash.clone()),
							&self.signing_context(),
							*val_idx,
						)
						.benchmark_signature();

						ValidityAttestation::Explicit(sig.clone())
					})
					.collect();

				BackedCandidate::<T::Hash> {
					candidate,
					validity_votes,
					validator_indices: bitvec::bitvec![bitvec::order::Lsb0, u8; 1; group_validators.len()],
				}
			})
			.collect()
	}

	fn create_disputes_with_some_spam(&self, start: u32, last: u32) -> Vec<DisputeStatementSet> {
		let validators =
			self.validators.as_ref().expect("must have some validators prior to calling");
		let config = configuration::Pallet::<T>::config();

		let mut spam_count = 0;
		(start..last)
			.map(|seed| {
				// fill corresponding storage items for inclusion that will be `taken` when `collect_disputed`
				// is called.
				let (para_id, core_idx, group_idx) = self.create_indexes(seed);
				let candidate_hash = CandidateHash(H256::from(byte32_slice_from(seed)));

				Self::add_availability(
					para_id,
					core_idx,
					group_idx,
					Self::validator_availability_votes_yes(), // TODO
					candidate_hash,
				);

				// create the set of statements to dispute the above candidate hash.
				let statement_range = if spam_count < config.dispute_max_spam_slots {
					// if we have not hit the spam dispute statement limit, only make up to the byzantine
					// threshold number of statements.

					// TODO: we could max the amount of spam even more by  taking 3 1/3 chunks of
					// validator set and having them each attest to different statements. Right now we
					// just use 1 1/3 chunk.
					0..Self::statement_spam_thresh()
				} else {
					// otherwise, make the maximum number of statements, which is over the byzantine
					// threshold and thus these statements will not be counted as potential spam.
					0..Self::max_statements()
				};
				log::info!(target: LOG_TARGET, "g");

				let statements = statement_range
					.map(|validator_index| {
						let validator_public = &validators.get(validator_index as usize).unwrap();

						// we need dispute statements on each side.
						let dispute_statement = if validator_index % 2 == 0 {
							DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit)
						} else {
							DisputeStatement::Valid(ValidDisputeStatementKind::Explicit)
						};
						let data = dispute_statement.payload_data(candidate_hash.clone(), 2);
						let statement_sig = validator_public.sign(&data).unwrap();

						(dispute_statement, ValidatorIndex(validator_index), statement_sig)
					})
					.collect();

				if spam_count < config.dispute_max_spam_slots {
					spam_count += 1;
				}

				// return dispute statements with metadata.
				DisputeStatementSet {
					candidate_hash: candidate_hash.clone(),
					session: 2,
					statements,
				}
			})
			.collect()
	}

	pub(crate) fn build(self, backed_and_concluding: u32, disputed: u32) -> Bench<T> {
		// make sure relevant storage is cleared. TODO this is just to get the asserts to work when
		// running tests because it seems the storage is not cleared in between.
		inclusion::PendingAvailabilityCommitments::<T>::remove_all(None);
		inclusion::PendingAvailability::<T>::remove_all(None);

		// Setup para_ids traverses each core,
		// creates a ParaId for that CoreIndex,
		// inserts ParaLifeCycle::Onboarding for that ParaId,
		// inserts the upcoming paras genesis
		// subsequently inserts the ParaId into the ActionsQueue
		// Note that there is an n+2 session delay for these actions to take effect
		// We are currently in Session 0, so these changes will take effect in Session 2
		Self::setup_para_ids(Self::cores());

		// As there are not validator public keys in the system yet, we must generate them here
		let validator_ids = Self::generate_validator_pairs(Self::max_validators());
		// Setup for session 1 and 2 along with the proper run_to_block logic

		let target_session = SessionIndex::from(2u32);
		let builder = self.setup_session(target_session, validator_ids);

		let concluding_cores: BTreeSet<_> = (0..backed_and_concluding).into_iter().collect();
		let concluding_0: BTreeSet<_> = (0..0).into_iter().collect();

		let bitfields = builder.create_availability_bitfields(concluding_cores.clone());
		let backed_candidates = builder.create_backed_candidates(concluding_cores);

		let last_disputed = backed_and_concluding + disputed;
		assert!(last_disputed <= Self::cores());
		let disputes = builder.create_disputes_with_some_spam(backed_and_concluding, last_disputed);

		// spam slots are empty prior.
		// TODO
		// assert_eq!(disputes::Pallet::<T>::spam_slots(&builder.current_session), None);
		assert_eq!(
			inclusion::PendingAvailabilityCommitments::<T>::iter().count(),
			(disputed + backed_and_concluding) as usize
		);
		assert_eq!(
			inclusion::PendingAvailability::<T>::iter().count(),
			(disputed + backed_and_concluding) as usize
		);

		Bench::<T> {
			data: ParachainsInherentData {
				bitfields,
				backed_candidates,
				disputes, // Vec<DisputeStatementSet>
				parent_header: Self::header(builder.block_number.clone()),
			},
		}
	}
}

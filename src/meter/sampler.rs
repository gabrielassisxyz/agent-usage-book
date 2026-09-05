//! The sampling batch orchestrator (aub-eun.3, PLAN.md sections 14.3, 27, 44).
//!
//! One batch takes the configured accounts through four stages, each a
//! distinct step in this module so the pipeline reads the way the design
//! states it:
//!
//! 1. **Due determination**: for each account, ensure its identity row, read
//!    its evidence snapshot, and evaluate the due decision against it.
//! 2. **Lease acquisition**: for each due account, take the per-account
//!    sampling lease, re-validate the due decision while holding it (another
//!    invocation may have committed a fresh attempt between stage 1's read
//!    and this lease), record the policy snapshot, and commit the attempt
//!    start. An account that loses any of these is reported and never
//!    sampled; the attempt start is durable before any network work for that
//!    account begins, and a lease taken before a failed step is released
//!    before the failure is reported.
//! 3. **Request execution**: each leased attempt's single network try runs
//!    inside `std::thread::scope` over at most `max_concurrent_requests`
//!    workers, and the scope joins before returning, so no thread survives
//!    command exit.
//! 4. **Persistence**: each result is committed independently, in its own
//!    transaction followed by its own projection publication, so a store
//!    failure on one account cannot discard another account's evidence.
//!
//! The isolation requirement is the point of the shape: a batch in which one
//! endpoint hangs must still commit the other accounts' observations, because
//! the alternative loses irreplaceable evidence for accounts that were
//! perfectly reachable. That is why persistence is per result rather than per
//! batch, even though a batch transaction would be simpler.
//!
//! Scoped threads are the right tool because the borrowed data outlives
//! nothing: the scope joins before returning. A background thread persisting
//! after exit would write into a database the process no longer coordinates,
//! which is the failure mode the design's not-to-build list names explicitly.
//!
//! The command budget is enforced at the transport port the workers run
//! through. A provider adapter supplies its own budget to the requests it
//! issues; that budget is the adapter's local view, not the command's. The
//! wrapper substitutes the command's remaining budget: it clips the request
//! timeouts, refuses requests the command can no longer afford before the
//! inner transport is touched, and reports a request that outlives the budget
//! as [`FailureClass::TotalBudgetExpired`] rather than as whatever the driver
//! saw last. Every adapter is therefore budget-bounded without having to know
//! the command's configuration.
//!
//! One network try runs per logical attempt here. The retry sequence wraps
//! this execution stage through the [`crate::meter::retry::RetryEnv`] seam,
//! whose production driver lands with `aub sample` (`aub-eun.6`); a retried
//! attempt is still exactly one logical attempt with one terminal result, so
//! this stage's per-account cardinality holds under either driver.
//!
//! A measured reading is persisted only together with the response capsule
//! the adapter captured for it: the observation row references its evidence
//! row, so a success without a captured capsule is a persistence failure
//! naming that, never a fabricated evidence row.
//!
//! May not depend on:
//! - SQLite directly (rule `03`): every read and write goes through the
//!   repository seam in `crate::store::repository`
//! - presentation
//! - calibration
//! - the credential or configuration modules (rule `07`): the caller
//!   resolves credentials, policy values and the concurrency bound, and
//!   hands them over resolved

use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::{Digest, Sha256};

use crate::domain::attempt::DueReason as DecisionDueReason;
use crate::domain::attempt::{AttemptId, AttemptOutcome, AttemptStarted};
use crate::domain::failure::FailureClass;
use crate::domain::ids::{AdapterVersion, ProviderContractId};
use crate::domain::time::{Clock, MonotonicDuration, ProviderObservedAt, UtcTimestamp};
use crate::domain::window::{MeterWindow, QuantizationSemantics, WindowScope};
use crate::error::Error;
use crate::meter::adapter::{
    CredentialHandle, HttpTransport, MeterRequest, ProviderAdapter, ProviderObservation,
};
use crate::meter::anthropic::AnthropicReading;
use crate::meter::due::{
    self, AttemptHistoryEntry, DueBasisRef, DueDecision, DueInputs, DuePolicy,
};
use crate::meter::evidence::CapturedProviderResponse;
use crate::meter::retry::attempt_outcome_of;
use crate::meter::transport::{CommandBudget, HttpRequest, HttpResponse};
use crate::projection::Publication;
use crate::store::account::AccountId;
use crate::store::meter_attempt::{
    DueBasis, DueReason as StoredDueReason, MeterAttemptRowId, NewMeterAttempt,
    NewMeterAttemptResult,
};
use crate::store::meter_evidence::NewMeterResponseEvidence;
use crate::store::repository::{NewMeterInterpretation, Repository, TerminalMeterBundle};
use crate::store::sample_run::{SampleRunId, Trigger};
use crate::store::sampling_lease::{AccountName, LeaseHolder, LeaseOutcome};
use crate::store::sampling_policy_snapshot::ResolvedSamplingPolicy;
use crate::store::spool::{SpoolCycleOutcome, spool_then_commit};
use crate::store::window_anomaly::StoredWindowAnomaly;

/// The recipe token inside the normalized fingerprint, so a later change to
/// how the fingerprint is computed cannot collide with an earlier one under
/// the same semantics version.
const NORMALIZED_FINGERPRINT_RECIPE: &[u8] = b"aub-normalized-windows-v1";

/// The reading surface the orchestrator needs from an adapter's typed success
/// value: the windows it measured and, where the provider supplies one, the
/// instant the provider says it measured. Implemented beside the orchestrator
/// for every adapter whose readings it persists, so the adapter trait itself
/// stays free of persistence-shaped obligations.
pub trait MeteredReading {
    fn windows(&self) -> &[MeterWindow];
    fn provider_observed_at(&self) -> Option<ProviderObservedAt>;

    /// A reading may identify a response-shape contract more precisely than
    /// the adapter's default declaration, which keeps legacy replays labelled
    /// without rewriting their immutable observations.
    fn provider_contract_id(&self) -> Option<&ProviderContractId> {
        None
    }
}

impl MeteredReading for AnthropicReading {
    fn windows(&self) -> &[MeterWindow] {
        &self.windows
    }

    fn provider_observed_at(&self) -> Option<ProviderObservedAt> {
        self.provider_observed_at
    }

    fn provider_contract_id(&self) -> Option<&ProviderContractId> {
        Some(&self.provider_contract_id)
    }
}

/// One account's prepared work in a sampling batch, assembled by the caller:
/// identity, adapter, resolved credential, resolved policy. The configured
/// `name` is also the lease key, so two accounts in one batch must not share
/// it.
pub struct BatchAccount<A> {
    pub name: AccountName,
    pub provider_key: String,
    pub adapter: A,
    pub credential: CredentialHandle,
    pub credential_context_id: Option<String>,
    pub request: MeterRequest,
    pub policy: ResolvedSamplingPolicy,
    /// How close to a known reset the due decision demands fresh evidence.
    /// Configuration resolves it; the policy snapshot records the same value
    /// in its own string form.
    pub reset_edge_lead: MonotonicDuration,
    /// An explicit operator or hook request: due regardless of history.
    pub forced: bool,
    /// Which adapter build produced the reading, persisted on the
    /// interpretation for provenance. An empty version is refused by the
    /// database rather than silently stored.
    pub adapter_version: AdapterVersion,
}

/// Runs one batch of accounts through the four sampling stages.
///
/// The repository, transport, clock and every policy number arrive resolved:
/// this type orchestrates, it does not resolve configuration or credentials.
/// The concurrency bound is the configuration value
/// `sampling.max_concurrent_requests`; the lease TTL is the caller's choice,
/// with [`crate::store::sampling_lease::DEFAULT_LEASE_TTL`] as the documented
/// default.
pub struct SamplingOrchestrator<'a, T, C> {
    pub repository: &'a Repository,
    pub transport: T,
    pub clock: C,
    pub trigger: Trigger,
    pub configuration_fingerprint: String,
    pub holder: LeaseHolder,
    pub lease_ttl: MonotonicDuration,
    /// The command-wide wall-clock budget every request is clipped to.
    pub command_budget: MonotonicDuration,
    /// The most requests one batch may keep in flight.
    pub max_concurrent_requests: usize,
}

/// What one batch did, one entry per input account in input order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchReport {
    pub run_id: SampleRunId,
    pub accounts: Vec<AccountReport>,
    /// Worker threads the execution stage spawned.
    pub workers_spawned: usize,
    /// Worker threads that finished their work loop. Equals
    /// `workers_spawned` because the scope joins before `run` returns; the
    /// field exists so that contract is observable rather than assumed.
    pub workers_completed: usize,
}

/// One account's outcome within a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountReport {
    pub name: AccountName,
    pub disposition: AccountDisposition,
}

/// How far one account got through the batch. Exhaustive rather than an
/// `Option`: every account in the input is accounted for exactly once, and a
/// caller rendering a refusal names what happened instead of reading silence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountDisposition {
    /// Stage 1 concluded no attempt is owed yet.
    NotYet { next_due_at: UtcTimestamp },
    /// Stage 1 could not read the account's evidence snapshot.
    DueLookupFailed { reason: String },
    /// Stage 2 found another holder's lease live; the attempt belongs to
    /// whoever holds it.
    LeaseHeld { holder: String },
    /// Stage 2 could not establish eligibility: the policy snapshot or the
    /// attempt start could not be committed. No request was issued.
    EligibilityFailed { reason: String },
    /// The attempt ran and its terminal fact was committed.
    Sampled(SampledAttempt),
    /// The attempt ran but its terminal fact could not be committed, and
    /// nothing durable preserved the reading either. The attempt row remains
    /// as the evidence it is; the outcome the request produced is carried so
    /// the report does not lose what happened on the network. Distinct from
    /// [`AccountDisposition::Spooled`]: this variant means the observation
    /// itself is gone, which for a measured reading only happens when the
    /// spool write itself fails, since a commit failure after a successful
    /// spool write is `Spooled` instead.
    PersistFailed {
        attempt_id: AttemptId,
        outcome: AttemptOutcome,
        reason: String,
    },
    /// The attempt ran, the reading was durably spooled to disk, but the
    /// terminal commit into SQLite failed. Unlike `PersistFailed`, the
    /// observation is not lost: the pending record on disk is what the next
    /// drain applies (PLAN.md section 13, `aub doctor`'s `pending-evidence`
    /// check). The outcome is always `AttemptOutcome::Success`, because only
    /// a measured reading is spooled.
    Spooled {
        attempt_id: AttemptId,
        outcome: AttemptOutcome,
        reason: String,
    },
}

/// One committed terminal fact for one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledAttempt {
    pub attempt_id: AttemptId,
    pub outcome: AttemptOutcome,
    /// Whether the success path committed evidence, an observation and its
    /// windows alongside the result.
    pub observation_committed: bool,
    /// The publication that followed this commit.
    pub publication: Publication,
    /// Any window anomaly the commit's consecutive-window comparison found
    /// against the account's immediately preceding observation
    /// (`aub-eun.14`). Always empty when `observation_committed` is false:
    /// a result with no observation has no window to compare.
    pub window_anomalies: Vec<StoredWindowAnomaly>,
}

/// Stage 1's per-account outcome.
enum DueStageOutcome {
    NotYet {
        next_due_at: UtcTimestamp,
    },
    LookupFailed {
        reason: String,
    },
    /// The account is due; the reason and basis are re-decided under the
    /// lease in stage 2, from a snapshot the lease serializes.
    Due {
        account_id: AccountId,
    },
}

/// Stage 2's refusal to sample a due account.
enum EligibilityRefusal {
    LeaseHeld(String),
    /// The due decision, re-validated under the lease, no longer finds an
    /// attempt owed: another invocation's fresh attempt is in the history
    /// now. The lease is released and the account is reported not yet due.
    NotYet {
        next_due_at: UtcTimestamp,
    },
    Failed(String),
}

/// What one worker produced for one account: the captured response, the
/// instant it was received, and how long the request took on the monotonic
/// clock.
struct AttemptPayload<R> {
    captured: CapturedProviderResponse<R>,
    received_at: UtcTimestamp,
    elapsed: MonotonicDuration,
}

/// One account's reserved slice of the batch: the input it borrows, its store
/// identity, and the attempt the orchestrator already committed. The due
/// reason and basis live on the attempt row they were persisted with; the
/// struct carries no copy of them.
struct LeasedAttempt<'acc, A> {
    index: usize,
    account: &'acc BatchAccount<A>,
    account_id: AccountId,
    attempt: AttemptStarted,
}

impl<'a, T, C> SamplingOrchestrator<'a, T, C>
where
    T: HttpTransport + Sync,
    C: Clock + Sync,
{
    /// Runs the batch. The only batch-level failure is the run row: if the
    /// invocation cannot record that it ran, nothing else in the batch should
    /// happen, because no attempt could reference a run that does not exist.
    /// Every other failure is per account and reported there.
    pub fn run<A>(&self, accounts: &[BatchAccount<A>]) -> Result<BatchReport, Error>
    where
        A: ProviderAdapter + Sync,
        A::Reading: MeteredReading + Send,
    {
        let started_at = self.clock.now();
        let run_id = self.repository.start_sample_run(
            self.trigger,
            started_at,
            &self.configuration_fingerprint,
        )?;
        let budget = CommandBudget::new(self.command_budget, &self.clock);

        // Stage 1: due determination, one account at a time, each failure
        // recorded against its own account.
        let mut reports: Vec<Option<AccountReport>> = (0..accounts.len()).map(|_| None).collect();
        let mut leased: Vec<LeasedAttempt<'_, A>> = Vec::new();

        for (index, account) in accounts.iter().enumerate() {
            match self.determine_due(account, started_at) {
                DueStageOutcome::NotYet { next_due_at } => {
                    reports[index] = Some(AccountReport {
                        name: account.name.clone(),
                        disposition: AccountDisposition::NotYet { next_due_at },
                    });
                }
                DueStageOutcome::LookupFailed { reason } => {
                    reports[index] = Some(AccountReport {
                        name: account.name.clone(),
                        disposition: AccountDisposition::DueLookupFailed { reason },
                    });
                }
                DueStageOutcome::Due { account_id } => {
                    match self.acquire_eligibility(index, account, account_id, run_id) {
                        Ok(leased_attempt) => leased.push(leased_attempt),
                        Err(EligibilityRefusal::LeaseHeld(holder)) => {
                            reports[index] = Some(AccountReport {
                                name: account.name.clone(),
                                disposition: AccountDisposition::LeaseHeld { holder },
                            });
                        }
                        Err(EligibilityRefusal::NotYet { next_due_at }) => {
                            reports[index] = Some(AccountReport {
                                name: account.name.clone(),
                                disposition: AccountDisposition::NotYet { next_due_at },
                            });
                        }
                        Err(EligibilityRefusal::Failed(reason)) => {
                            reports[index] = Some(AccountReport {
                                name: account.name.clone(),
                                disposition: AccountDisposition::EligibilityFailed { reason },
                            });
                        }
                    }
                }
            }
        }

        // Stage 3: bounded scoped-thread execution. The scope joins every
        // worker before returning, so no thread survives this call.
        let (mut products, workers_spawned, workers_completed) =
            self.execute_requests(&leased, &budget);

        // Stage 4: per-account persistence, each account's terminal fact in
        // its own transaction, and each account's lease released once its own
        // persistence attempt is done. A store failure on one account moves
        // the loop to the next account; it never ends the batch.
        for (slot, item) in leased.iter().enumerate() {
            let disposition = match products[slot].take() {
                Some(payload) => self.persist_one(item, payload),
                None => panic!(
                    "sampling worker produced no result for account {}; the scoped-thread \
                     join guarantees one",
                    item.account.name.as_str()
                ),
            };
            reports[item.index] = Some(AccountReport {
                name: item.account.name.clone(),
                disposition,
            });
            // A release that fails leaves the lease to expire by TTL, which
            // is the lease's own safety net for a holder that died.
            let _ = self
                .repository
                .release_sampling_lease(&item.account.name, &self.holder);
        }

        Ok(BatchReport {
            run_id,
            accounts: reports
                .into_iter()
                .map(|report| report.expect("every input account is accounted for"))
                .collect(),
            workers_spawned,
            workers_completed,
        })
    }

    /// Stage 1 for one account: ensure the identity row, read the evidence
    /// snapshot, and evaluate the due decision against it.
    fn determine_due<A>(&self, account: &BatchAccount<A>, now: UtcTimestamp) -> DueStageOutcome {
        let account_id =
            match self
                .repository
                .ensure_account(&account.provider_key, account.name.as_str(), now)
            {
                Ok(account_id) => account_id,
                Err(error) => {
                    return DueStageOutcome::LookupFailed {
                        reason: error.to_string(),
                    };
                }
            };
        match self.evaluate_due(account, account_id, now) {
            Ok(DueDecision::Due { .. }) => DueStageOutcome::Due { account_id },
            Ok(DueDecision::NotYet { next_due_at, .. }) => DueStageOutcome::NotYet { next_due_at },
            Err(reason) => DueStageOutcome::LookupFailed { reason },
        }
    }

    /// Evaluates the due decision for one account against a fresh evidence
    /// snapshot, so every caller decides from one moment of the database.
    /// The error is the store failure's text, ready for a report.
    fn evaluate_due<A>(
        &self,
        account: &BatchAccount<A>,
        account_id: AccountId,
        now: UtcTimestamp,
    ) -> Result<DueDecision, String> {
        let snapshot = self
            .repository
            .due_evidence_snapshot(account_id)
            .map_err(|error| error.to_string())?;
        // The decision reads only the most recent entry: the latest attempt
        // with the terminal result it ever reached.
        let latest_result = snapshot.latest_result;
        let history: Vec<AttemptHistoryEntry> = snapshot
            .latest_attempt
            .map(|attempt| AttemptHistoryEntry {
                attempt,
                result: latest_result,
            })
            .into_iter()
            .collect();
        Ok(due::evaluate(&DueInputs {
            policy: DuePolicy {
                ordinary_cadence: account.policy.ordinary_cadence,
                reset_edge_lead: account.reset_edge_lead,
            },
            history,
            known_resets: snapshot.known_resets,
            now,
            forced: account.forced,
        }))
    }

    /// Stage 2 for one due account: take the lease, re-validate the due
    /// decision under it, record the policy snapshot, and commit the attempt
    /// start. The re-validation is what makes one batch per account hold
    /// under concurrency: between stage 1's read and this stage's lease the
    /// world can have moved (another invocation may have committed a fresh
    /// attempt and released its own lease), and the lease is the
    /// serialization point, so the decision that gates the attempt is made
    /// while holding it. Any step failing means no request is issued for this
    /// account, and the lease is released before returning.
    fn acquire_eligibility<'acc, A>(
        &self,
        index: usize,
        account: &'acc BatchAccount<A>,
        account_id: AccountId,
        run_id: SampleRunId,
    ) -> Result<LeasedAttempt<'acc, A>, EligibilityRefusal>
    where
        A: ProviderAdapter,
    {
        let refusal = |error: Error| EligibilityRefusal::Failed(error.to_string());
        match self.repository.acquire_sampling_lease(
            &account.name,
            &self.holder,
            self.lease_ttl,
            &self.clock,
        ) {
            Ok(LeaseOutcome::Granted(_)) => {}
            Ok(LeaseOutcome::AlreadyHeld(existing)) => {
                return Err(EligibilityRefusal::LeaseHeld(
                    existing.holder.as_str().to_string(),
                ));
            }
            Err(error) => return Err(refusal(error)),
        }

        let evaluated_at = self.clock.now();
        let (reason, basis) = match self.evaluate_due(account, account_id, evaluated_at) {
            Ok(DueDecision::Due { reason, basis }) => (reason, basis),
            Ok(DueDecision::NotYet { next_due_at, .. }) => {
                let _ = self
                    .repository
                    .release_sampling_lease(&account.name, &self.holder);
                return Err(EligibilityRefusal::NotYet { next_due_at });
            }
            Err(error) => {
                let _ = self
                    .repository
                    .release_sampling_lease(&account.name, &self.holder);
                return Err(EligibilityRefusal::Failed(error));
            }
        };

        let stored_basis = match stored_due_basis(basis) {
            Ok(basis) => basis,
            Err(error) => {
                let _ = self
                    .repository
                    .release_sampling_lease(&account.name, &self.holder);
                return Err(refusal(error));
            }
        };

        let policy_snapshot_id =
            match self
                .repository
                .resolve_policy_snapshot(account_id, evaluated_at, &account.policy)
            {
                Ok(id) => id,
                Err(error) => {
                    let _ = self
                        .repository
                        .release_sampling_lease(&account.name, &self.holder);
                    return Err(refusal(error));
                }
            };

        let declarations = account.adapter.declarations();
        let new_attempt = NewMeterAttempt {
            run_id,
            account_id,
            provider: account.provider_key.clone(),
            request_started_at: self.clock.now(),
            credential_context_id: account.credential_context_id.clone(),
            policy_snapshot_id,
            due_at: evaluated_at,
            due_reason: stored_due_reason(reason),
            due_basis: stored_basis,
            provider_contract_id: declarations.provider_contract_id.as_str().to_string(),
            meter_semantics_id: declarations.meter_semantics_id.as_str().to_string(),
        };
        let attempt = match self.repository.start_meter_attempt(&new_attempt) {
            Ok(started) => started,
            Err(error) => {
                let _ = self
                    .repository
                    .release_sampling_lease(&account.name, &self.holder);
                return Err(refusal(error));
            }
        };
        Ok(LeasedAttempt {
            index,
            account,
            account_id,
            attempt,
        })
    }
    /// Stage 3: execute every leased attempt's network try over at most
    /// `max_concurrent_requests` workers inside one scope. The workers pull
    /// from a shared queue of slots, so the bound holds regardless of how
    /// long each individual request takes, and the scope joins every worker
    /// before returning.
    fn execute_requests<'acc, A>(
        &self,
        leased: &[LeasedAttempt<'acc, A>],
        budget: &CommandBudget,
    ) -> (Vec<Option<AttemptPayload<A::Reading>>>, usize, usize)
    where
        A: ProviderAdapter + Sync,
        A::Reading: MeteredReading + Send,
    {
        let empty: Vec<Option<AttemptPayload<A::Reading>>> = leased.iter().map(|_| None).collect();
        if leased.is_empty() {
            return (empty, 0, 0);
        }
        let worker_count = leased.len().min(self.max_concurrent_requests);
        let next_slot = AtomicUsize::new(0);
        let completed_workers = AtomicUsize::new(0);
        let products = Mutex::new(empty);
        let transport = BudgetedTransport {
            inner: &self.transport,
            budget,
        };

        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                scope.spawn(|| {
                    loop {
                        let slot = next_slot.fetch_add(1, Ordering::Relaxed);
                        if slot >= leased.len() {
                            break;
                        }
                        let payload = self.attempt_request(&leased[slot], &transport);
                        let mut reserved = products.lock().unwrap_or_else(PoisonError::into_inner);
                        reserved[slot] = Some(payload);
                        drop(reserved);
                    }
                    // Counted once per worker, after its work loop, so the
                    // field reads how many workers finished, not how many
                    // slots were processed: one worker typically serves
                    // several slots.
                    completed_workers.fetch_add(1, Ordering::Relaxed);
                });
            }
        });

        let workers_completed = completed_workers.load(Ordering::Relaxed);
        let products = products
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        (products, worker_count, workers_completed)
    }

    /// One attempt's single network try: the adapter call through the
    /// budget-clamped transport port, with the receive instant and the
    /// monotonic request duration captured around it.
    fn attempt_request<A>(
        &self,
        item: &LeasedAttempt<'_, A>,
        transport: &BudgetedTransport<'_, T>,
    ) -> AttemptPayload<A::Reading>
    where
        A: ProviderAdapter,
        A::Reading: MeteredReading,
    {
        let request_started = self.clock.monotonic_now();
        let captured = item.account.adapter.observe_with_evidence(
            &item.account.credential,
            &item.account.request,
            transport,
            &self.clock,
        );
        let received_at = self.clock.now();
        let elapsed = self.clock.monotonic_now().duration_since(request_started);
        AttemptPayload {
            captured,
            received_at,
            elapsed,
        }
    }

    /// Stage 4 for one account: commit the terminal fact. A success commits
    /// the complete bundle, result, evidence, interpretation and windows, in
    /// one transaction; a failure commits its result alone, because a failure
    /// has no provider observation to record and the failed-response capsule
    /// store is a later bead's (`aub-2r3`). The store error becomes this
    /// account's report; it never propagates out of the loop.
    fn persist_one<A>(
        &self,
        item: &LeasedAttempt<'_, A>,
        payload: AttemptPayload<A::Reading>,
    ) -> AccountDisposition
    where
        A: ProviderAdapter,
        A::Reading: MeteredReading,
    {
        let attempt_id = item.attempt.attempt_id();
        let outcome = attempt_outcome_of(&payload.captured.observation);
        let persist_error = |error: Error| AccountDisposition::PersistFailed {
            attempt_id,
            outcome,
            reason: error.to_string(),
        };
        let AttemptPayload {
            captured,
            received_at,
            elapsed,
        } = payload;
        match &captured.observation {
            ProviderObservation::Measured(reading) => {
                let bundle = match self.terminal_bundle(
                    item,
                    captured.evidence.as_ref(),
                    reading,
                    received_at,
                    elapsed,
                ) {
                    Ok(bundle) => bundle,
                    Err(error) => return persist_error(error),
                };
                // PLAN.md section 13, steps 5 to 7: spool the parsed reading
                // durably before the commit is attempted, so a commit that
                // cannot run (busy database, a refused constraint, a crash)
                // leaves the observation on disk instead of destroying it.
                match spool_then_commit(self.repository, &bundle, &self.clock) {
                    Ok(SpoolCycleOutcome::Committed {
                        ids, publication, ..
                    }) => AccountDisposition::Sampled(SampledAttempt {
                        attempt_id,
                        outcome: AttemptOutcome::Success,
                        observation_committed: true,
                        publication,
                        window_anomalies: ids.window_anomalies,
                    }),
                    Ok(SpoolCycleOutcome::LeftPending { error, .. }) => {
                        AccountDisposition::Spooled {
                            attempt_id,
                            outcome: AttemptOutcome::Success,
                            reason: error.to_string(),
                        }
                    }
                    // The spool write itself failed: the reading was never
                    // made durable anywhere, so this is a real loss rather
                    // than a deferred commit.
                    Err(error) => persist_error(error),
                }
            }
            ProviderObservation::AuthRequired(_) | ProviderObservation::Unreachable(_) => {
                let result = NewMeterAttemptResult {
                    attempt_id: match row_id_of(attempt_id) {
                        Ok(row_id) => row_id,
                        Err(error) => return persist_error(error),
                    },
                    completed_at: received_at,
                    elapsed,
                    outcome,
                    sanitized_error_classification: None,
                    retry_index: None,
                    clock_anomaly: false,
                };
                match self.repository.commit_terminal_result(&result) {
                    Ok(publication) => AccountDisposition::Sampled(SampledAttempt {
                        attempt_id,
                        outcome,
                        observation_committed: false,
                        publication,
                        window_anomalies: Vec::new(),
                    }),
                    Err(error) => persist_error(error),
                }
            }
        }
    }

    /// Builds one complete terminal bundle from a measured reading: the
    /// result, the captured evidence, the interpretation under the adapter's
    /// declarations, and the windows the reading carries.
    fn terminal_bundle<A>(
        &self,
        item: &LeasedAttempt<'_, A>,
        evidence: Option<&crate::meter::evidence::JsonEvidenceCapsule>,
        reading: &A::Reading,
        received_at: UtcTimestamp,
        elapsed: MonotonicDuration,
    ) -> Result<TerminalMeterBundle, Error>
    where
        A: ProviderAdapter,
        A::Reading: MeteredReading,
    {
        let attempt_id = item.attempt.attempt_id();
        let row_id = row_id_of(attempt_id)?;
        let evidence = evidence.ok_or_else(|| {
            Error::Store(format!(
                "account {}: the adapter measured a reading without a response capsule, \
                 which cannot be persisted as an observation",
                item.account.name.as_str()
            ))
        })?;
        let declarations = item.account.adapter.declarations();
        let provider_contract_id = reading
            .provider_contract_id()
            .cloned()
            .unwrap_or_else(|| declarations.provider_contract_id.clone());
        let fingerprint = normalized_fingerprint(
            declarations.meter_semantics_id.as_str(),
            provider_contract_id.as_str(),
            reading.windows(),
        );
        let result = NewMeterAttemptResult {
            attempt_id: row_id,
            completed_at: received_at,
            elapsed,
            outcome: AttemptOutcome::Success,
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: false,
        };
        let evidence_row = NewMeterResponseEvidence {
            attempt_id: row_id,
            response_classification: "success".to_string(),
            received_at,
            // The typed reading carries no original timestamp spelling; the
            // retained capsule does, so a corrected parser can recover it.
            provider_observed_at_original: None,
            evidence_capsule: evidence.serialized().to_string(),
            capsule_schema_version: evidence.schema_version().to_string(),
            sanitizer_version: evidence.sanitizer_version().to_string(),
            capture_truncated: evidence.capture_truncated(),
        };
        let interpretation = NewMeterInterpretation {
            account_id: item.account_id,
            provider: item.account.provider_key.clone(),
            provider_observed_at: reading
                .provider_observed_at()
                .map(|observed| observed.as_utc()),
            received_at,
            measurement_basis: declarations.measurement_basis,
            observed_plan: None,
            observed_tier: None,
            adapter_version: item.account.adapter_version.clone(),
            provider_contract_id,
            meter_semantics_id: declarations.meter_semantics_id,
            normalized_fingerprint: fingerprint,
        };
        TerminalMeterBundle::new(
            result,
            evidence_row,
            interpretation,
            reading.windows().to_vec(),
        )
    }
}

/// The transport port the workers run requests through: the caller's
/// transport wrapped with the command's own budget.
///
/// A provider adapter supplies its own budget to the requests it issues; that
/// budget is the adapter's local view, not the command's, and the command's
/// budget is the ceiling this orchestration owns. The wrapper substitutes it:
/// a request the command can no longer afford returns
/// [`FailureClass::TotalBudgetExpired`] before the inner transport is
/// touched, the request timeouts are clipped to what remains of the budget,
/// and a request that outlives the budget on the way out is reported as
/// expired rather than as whatever the driver saw last.
struct BudgetedTransport<'a, T: HttpTransport> {
    inner: &'a T,
    budget: &'a CommandBudget,
}

impl<T: HttpTransport> HttpTransport for BudgetedTransport<'_, T> {
    fn send(
        &self,
        request: &HttpRequest,
        _adapter_budget: &CommandBudget,
        clock: &impl Clock,
    ) -> Result<HttpResponse, FailureClass> {
        let Some(remaining) = self.budget.remaining(clock) else {
            return Err(FailureClass::TotalBudgetExpired);
        };
        let mut clipped = request.clone();
        clipped.timeouts = request.timeouts.clip_to_budget(remaining);
        let response = self.inner.send(&clipped, self.budget, clock);
        if self.budget.is_expired(clock) {
            return Err(FailureClass::TotalBudgetExpired);
        }
        response
    }
}

/// The storage row id of a domain attempt id. The ids originate as row ids,
/// so the conversion only fails on a value that could not have come from the
/// store, which is reported rather than truncated silently.
fn row_id_of(attempt_id: AttemptId) -> Result<MeterAttemptRowId, Error> {
    i64::try_from(attempt_id.value())
        .map(MeterAttemptRowId::new)
        .map_err(|_| {
            Error::Store(format!(
                "attempt id {} exceeds the storage row id range",
                attempt_id.value()
            ))
        })
}

/// The due decision's reason, in the store's spelling of the same four-value
/// vocabulary. One conversion, in one place.
fn stored_due_reason(reason: DecisionDueReason) -> StoredDueReason {
    match reason {
        DecisionDueReason::OrdinaryCadence => StoredDueReason::OrdinaryCadence,
        DecisionDueReason::ResetEdge => StoredDueReason::ResetEdge,
        DecisionDueReason::PostResetConfirmation => StoredDueReason::PostResetConfirmation,
        DecisionDueReason::ForcedOrManual => StoredDueReason::ForcedOrManual,
    }
}

/// The prior fact a due decision was based on, in the store's spelling. The
/// ids originated as row ids, so the conversion is exact for any value the
/// store produced.
fn stored_due_basis(basis: Option<DueBasisRef>) -> Result<Option<DueBasis>, Error> {
    basis
        .map(|reference| {
            let id = match reference {
                DueBasisRef::Attempt(id) | DueBasisRef::Result(id) => id,
            };
            let row_id = row_id_of(id)?;
            Ok(match reference {
                DueBasisRef::Attempt(_) => DueBasis::Attempt { row_id },
                DueBasisRef::Result(_) => DueBasis::Result { attempt_id: row_id },
            })
        })
        .transpose()
}

/// The hash identifying one normalized reading: the semantics version it was
/// read under and every window in canonical field order. Deterministic by
/// construction, so the same evidence reinterpreted under the same semantics
/// produces the same fingerprint and a changed reading a different one.
fn normalized_fingerprint(
    meter_semantics_id: &str,
    provider_contract_id: &str,
    windows: &[MeterWindow],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(NORMALIZED_FINGERPRINT_RECIPE);
    hasher.update(b"\n");
    hasher.update(meter_semantics_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(provider_contract_id.as_bytes());
    for window in windows {
        let line = format!(
            "|{}|{}|{}|{}|{}|{}|{}|{}|{}\n",
            window.semantic_key().as_str(),
            scope_token(window.scope()),
            window.quota_used().as_ppm().get(),
            window.reported_resolution().as_ppm().get(),
            quantization_token(window.quantization()),
            match window.reset_state() {
                crate::domain::window::WindowResetState::Known(ts) => ts.unix_nanos().to_string(),
                crate::domain::window::WindowResetState::NotStarted => "not_started".to_string(),
            },
            window.nominal_duration().as_nanos(),
            window.is_active(),
            window.severity().as_str(),
        );
        hasher.update(line.as_bytes());
    }
    hex(&hasher.finalize())
}

fn scope_token(scope: &WindowScope) -> String {
    match scope {
        WindowScope::AccountWide => "account_wide".to_string(),
        WindowScope::ModelSpecific(model) => format!("model:{}", model.as_str()),
    }
}

fn quantization_token(quantization: QuantizationSemantics) -> &'static str {
    match quantization {
        QuantizationSemantics::Exact => "exact",
        QuantizationSemantics::RoundedToNearest => "rounded_to_nearest",
        QuantizationSemantics::RoundedDown => "rounded_down",
        QuantizationSemantics::RoundedUp => "rounded_up",
        QuantizationSemantics::Unknown => "unknown",
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::time::{FakeClock, MonotonicInstant};
    use crate::domain::window::{NominalWindowDuration, ReportedResolution, WindowSemanticKey};
    use crate::meter::anthropic::AnthropicAdapter;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::ledger_generation;
    use crate::store::meter_attempt;
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;
    use crate::store::spool::{drain_pending, pending_file_path};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Barrier;
    use test_support::StateDir;

    /// A clock shared between the orchestrator and the scripted transport, so
    /// a test can advance time from inside a request without racing the
    /// orchestrator's own reads of the same clock.
    #[derive(Clone)]
    struct SharedClock(Arc<Mutex<FakeClock>>);

    impl SharedClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(FakeClock::new(
                UtcTimestamp::from_unix_nanos(1_700_000_000_000_000_000),
            ))))
        }

        fn advance(&self, duration: MonotonicDuration) {
            self.0.lock().unwrap().advance(duration);
        }
    }

    impl Clock for SharedClock {
        fn now(&self) -> UtcTimestamp {
            self.0.lock().unwrap().now()
        }

        fn monotonic_now(&self) -> MonotonicInstant {
            self.0.lock().unwrap().monotonic_now()
        }
    }

    /// What the scripted transport hands back for one account, keyed by the
    /// account tag the adapter's endpoint URL carries.
    #[derive(Debug, Clone)]
    enum ScriptedOutcome {
        /// A valid Anthropic usage body, so the adapter measures.
        Success,
        /// A 401, so the adapter concludes authentication is required.
        Unauthorized,
        /// A 500, so the adapter records an unreachable server error.
        ServerError,
        /// A body that is not JSON, so the adapter records a malformed body.
        Malformed,
    }

    /// The test transport: per-account scripts keyed off the request URL,
    /// in-flight and completion counters for the concurrency and thread
    /// lifetime assertions, and a hook for advancing the shared clock inside
    /// a request.
    struct ScriptedTransport {
        clock: SharedClock,
        scripts: Mutex<HashMap<String, ScriptedOutcome>>,
        /// Advance the shared clock by this much inside every request.
        advance_per_request: Mutex<Option<MonotonicDuration>>,
        calls_per_account: Mutex<HashMap<String, usize>>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        completed_sends: AtomicUsize,
    }

    impl ScriptedTransport {
        fn new(clock: SharedClock) -> Self {
            Self {
                clock,
                scripts: Mutex::new(HashMap::new()),
                advance_per_request: Mutex::new(None),
                calls_per_account: Mutex::new(HashMap::new()),
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                completed_sends: AtomicUsize::new(0),
            }
        }

        fn script(&self, tag: &str, outcome: ScriptedOutcome) {
            self.scripts
                .lock()
                .unwrap()
                .insert(tag.to_string(), outcome);
        }

        fn calls(&self, tag: &str) -> usize {
            *self
                .calls_per_account
                .lock()
                .unwrap()
                .get(tag)
                .unwrap_or(&0)
        }

        fn max_in_flight(&self) -> usize {
            self.max_in_flight.load(Ordering::Relaxed)
        }
    }

    impl HttpTransport for ScriptedTransport {
        fn send(
            &self,
            request: &HttpRequest,
            _budget: &CommandBudget,
            _clock: &impl Clock,
        ) -> Result<HttpResponse, FailureClass> {
            let tag = request
                .url
                .split("//")
                .nth(1)
                .and_then(|rest| rest.split('.').next())
                .unwrap_or_default()
                .to_string();
            *self
                .calls_per_account
                .lock()
                .unwrap()
                .entry(tag.clone())
                .or_insert(0) += 1;

            let now_in_flight = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
            self.max_in_flight
                .fetch_max(now_in_flight, Ordering::Relaxed);
            if let Some(duration) = *self.advance_per_request.lock().unwrap() {
                self.clock.advance(duration);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            let outcome = self.scripts.lock().unwrap().get(&tag).cloned();
            let result = match outcome {
                Some(ScriptedOutcome::Unauthorized) => Ok(HttpResponse {
                    status: 401,
                    headers: Vec::new(),
                    body: b"expired".to_vec(),
                }),
                Some(ScriptedOutcome::ServerError) => Ok(HttpResponse {
                    status: 500,
                    headers: Vec::new(),
                    body: b"boom".to_vec(),
                }),
                Some(ScriptedOutcome::Malformed) => Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: b"definitely not json".to_vec(),
                }),
                Some(ScriptedOutcome::Success) | None => Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: VALID_SUCCESS_BODY.as_bytes().to_vec(),
                }),
            };
            self.in_flight.fetch_sub(1, Ordering::Relaxed);
            // The completion counter is the last thing send does, so a caller
            // reading it after the orchestrator returns knows every send ran
            // to completion before the return.
            self.completed_sends.fetch_add(1, Ordering::Relaxed);
            result
        }
    }

    const VALID_SUCCESS_BODY: &str = r#"{"five_hour":{"utilization":10.0,"resets_at":"2026-01-01T00:00:00.000Z"},"seven_day":{"utilization":20.0,"resets_at":"2026-01-08T00:00:00.000Z"}}"#;

    fn policy() -> PragmaPolicy {
        PragmaPolicy {
            // The concurrent-invocation test races three writer threads against
            // one database while the rest of this file's tests run in the same
            // process, on a machine that may be running other work too. Actual
            // write transactions here take milliseconds; the timeout only has
            // to survive scheduling contention, not real lock hold time.
            busy_timeout: MonotonicDuration::from_millis(10_000),
        }
    }

    /// A migrated repository over a scratch state directory; the path is
    /// returned so a test thread can build its own repository over the same
    /// database, the way three concurrent invocations of the command would.
    fn fixture_database() -> (StateDir, PathBuf) {
        let scratch = StateDir::new();
        let database_path = scratch.path().join("sampler.db");
        let mut conn = open(&database_path, AccessMode::ReadWrite, &policy()).unwrap();
        run_migrations(
            &mut conn,
            &registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
        )
        .unwrap();
        drop(conn);
        (scratch, database_path)
    }

    fn batch_account(name: &str) -> BatchAccount<AnthropicAdapter> {
        BatchAccount {
            name: AccountName::new(name),
            provider_key: "anthropic".to_string(),
            adapter: AnthropicAdapter::with_endpoint(format!("http://{name}.accounts.test/usage")),
            credential: CredentialHandle::new("test-token"),
            credential_context_id: Some("ctx-test".to_string()),
            request: MeterRequest::default(),
            policy: ResolvedSamplingPolicy {
                ordinary_cadence: MonotonicDuration::from_seconds(300),
                freshness_horizon: MonotonicDuration::from_seconds(900),
                reset_edge_policy: "lead-120s".to_string(),
                retry_backoff_policy: "exponential-2-250ms".to_string(),
                command_budget: MonotonicDuration::from_seconds(30),
                policy_algorithm_version: "v1".to_string(),
            },
            reset_edge_lead: MonotonicDuration::from_seconds(120),
            forced: false,
            adapter_version: AdapterVersion::new("adapter-test-v1"),
        }
    }

    fn sampled(report: &AccountReport) -> &SampledAttempt {
        if let AccountDisposition::Sampled(sampled) = &report.disposition {
            sampled
        } else {
            panic!(
                "expected a sampled disposition, got {:?}",
                report.disposition
            )
        }
    }

    /// The property: over randomized outcome combinations, the number of
    /// persisted attempts always equals the number of due accounts, every
    /// attempt carries exactly one persisted result, and every worker the
    /// batch spawned finished before it returned.
    /// The property's one case: a batch over the given randomized outcome
    /// script, asserted for attempt cardinality and worker completion. Shared
    /// by every generated case below.
    fn randomized_outcomes_case(outcomes: Vec<u8>) {
        let (_scratch_dir, database_path) = fixture_database();
        let transport = ScriptedTransport::new(SharedClock::new());
        let clock = transport.clock.clone();
        let accounts: Vec<BatchAccount<AnthropicAdapter>> = outcomes
            .iter()
            .enumerate()
            .map(|(index, _)| batch_account(&format!("prop{index}")))
            .collect();
        for (index, outcome) in outcomes.iter().enumerate() {
            let scripted = match outcome {
                0 | 4 => ScriptedOutcome::Success,
                1 | 5 => ScriptedOutcome::Unauthorized,
                2 => ScriptedOutcome::ServerError,
                _ => ScriptedOutcome::Malformed,
            };
            transport.script(&format!("prop{index}"), scripted);
        }

        let repository = Repository::new(&database_path, policy());
        let report = SamplingOrchestrator {
            repository: &repository,
            transport: &transport,
            clock: &clock,
            trigger: Trigger::Timer,
            configuration_fingerprint: "fixture".to_string(),
            holder: LeaseHolder::new("test-holder"),
            lease_ttl: MonotonicDuration::from_seconds(30),
            command_budget: MonotonicDuration::from_seconds(30),
            max_concurrent_requests: 3,
        }
        .run(&accounts)
        .expect("the batch must run");

        // Every account was due (no history anywhere), so the persisted
        // attempt count equals the account count, whatever the outcomes.
        let due_count = accounts.len();
        let sampled_count = report
            .accounts
            .iter()
            .filter(|entry| {
                matches!(
                    entry.disposition,
                    AccountDisposition::Sampled(_)
                        | AccountDisposition::PersistFailed { .. }
                        | AccountDisposition::Spooled { .. }
                )
            })
            .count();
        assert_eq!(
            sampled_count, due_count,
            "persisted attempts must equal due accounts"
        );
        for entry in &report.accounts {
            let account = sampled(entry);
            let started = repository
                .attempt_started(account.attempt_id)
                .expect("the attempt start read must succeed");
            assert!(started.is_some(), "every sampled attempt must be persisted");
            let result = repository
                .attempt_result(account.attempt_id)
                .expect("the attempt result read must succeed");
            let stored = result.unwrap_or_else(|| {
                panic!(
                    "attempt {} has no terminal result though the batch reported it sampled",
                    account.attempt_id.value()
                )
            });
            assert_eq!(stored.attempt_id(), account.attempt_id);
            assert_eq!(
                stored.outcome(),
                account.outcome,
                "the persisted outcome must be the outcome the report carries"
            );
            // At most one result per attempt is the table's primary key;
            // a second read returns the same single fact.
            let again = repository
                .attempt_result(account.attempt_id)
                .expect("the read must succeed");
            assert_eq!(again, result, "reading the result twice returns one fact");
        }
        assert_eq!(
            report.workers_completed, report.workers_spawned,
            "every worker must have finished before the batch returned"
        );
        assert_eq!(
            report.workers_spawned,
            due_count.min(3),
            "the worker pool is bounded by the configured concurrency"
        );
    }

    // Over randomized outcome combinations, the number of persisted attempts
    // always equals the number of due accounts, every attempt carries exactly
    // one persisted result, and every worker the batch spawned finished
    // before it returned. The case budget is bounded: the property's space
    // is outcome combinations over at most six accounts, which 24 generated
    // cases cover well and which keeps the suite within the gate's budget.
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]
        #[test]
        fn persisted_attempts_equal_due_accounts_over_randomized_outcomes(
            outcomes in proptest::collection::vec(0u8..6, 1usize..=6),
        ) {
            randomized_outcomes_case(outcomes);
        }
    }

    /// No thread outlives the command: every account's send ran to completion
    /// before `run` returned, and the batch reports every spawned worker as
    /// finished. With a slow send, an implementation that returned while its
    /// threads were still running would read a completion count below the
    /// account count here.
    #[test]
    fn no_thread_outlives_the_command() {
        let (_scratch_dir, database_path) = fixture_database();
        let transport = ScriptedTransport::new(SharedClock::new());
        let clock = transport.clock.clone();
        let accounts: Vec<BatchAccount<AnthropicAdapter>> = (0..4)
            .map(|index| batch_account(&format!("outlives{index}")))
            .collect();

        let report = {
            let repository = Repository::new(&database_path, policy());
            SamplingOrchestrator {
                repository: &repository,
                transport: &transport,
                clock: &clock,
                trigger: Trigger::Timer,
                configuration_fingerprint: "fixture".to_string(),
                holder: LeaseHolder::new("test-holder"),
                lease_ttl: MonotonicDuration::from_seconds(30),
                command_budget: MonotonicDuration::from_seconds(30),
                max_concurrent_requests: 2,
            }
            .run(&accounts)
            .expect("the batch must run")
        };

        assert_eq!(
            transport.completed_sends.load(Ordering::Relaxed),
            accounts.len(),
            "every request completed before the batch returned"
        );
        assert_eq!(report.workers_completed, 2);
        assert_eq!(report.workers_spawned, 2);
        for entry in &report.accounts {
            assert_eq!(sampled(entry).outcome, AttemptOutcome::Success);
        }
    }

    /// The due-determination stage is distinct and can veto: an account whose
    /// history carries a fresh successful attempt is not yet due, so the
    /// batch records that against the account and issues no request and no
    /// attempt for it, while an account with no history in the same batch is
    /// due and sampled. A naive implementation that sampled every account
    /// unconditionally would create a second attempt for the fresh one.
    #[test]
    fn an_account_whose_history_is_fresh_is_not_due_and_creates_nothing() {
        let (_scratch_dir, database_path) = fixture_database();
        let transport = ScriptedTransport::new(SharedClock::new());
        let clock = transport.clock.clone();
        let fresh = batch_account("fresh");
        let due = batch_account("due");
        let repository = Repository::new(&database_path, policy());

        // First batch: both accounts have no history, so both are due and
        // both sample, which is what gives `fresh` its recent evidence.
        let first = SamplingOrchestrator {
            repository: &repository,
            transport: &transport,
            clock: &clock,
            trigger: Trigger::Timer,
            configuration_fingerprint: "fixture".to_string(),
            holder: LeaseHolder::new("test-holder"),
            lease_ttl: MonotonicDuration::from_seconds(30),
            command_budget: MonotonicDuration::from_seconds(30),
            max_concurrent_requests: 2,
        }
        .run(std::slice::from_ref(&fresh))
        .expect("the first batch must run");
        assert!(matches!(
            first.accounts[0].disposition,
            AccountDisposition::Sampled(_)
        ));
        let calls_after_first = transport.calls("fresh");

        // Second batch, no time advanced: `fresh` was sampled moments ago
        // under a 300 second cadence, `due` has no history at all.
        let accounts = vec![fresh, due];
        let second = SamplingOrchestrator {
            repository: &repository,
            transport: &transport,
            clock: &clock,
            trigger: Trigger::Timer,
            configuration_fingerprint: "fixture".to_string(),
            holder: LeaseHolder::new("test-holder"),
            lease_ttl: MonotonicDuration::from_seconds(30),
            command_budget: MonotonicDuration::from_seconds(30),
            max_concurrent_requests: 2,
        }
        .run(&accounts)
        .expect("the second batch must run");

        let skipped = &second.accounts[0];
        match &skipped.disposition {
            AccountDisposition::NotYet { next_due_at } => {
                assert_eq!(
                    next_due_at.unix_nanos(),
                    clock.now().unix_nanos() + 300_000_000_000i64,
                    "the next due instant is the cadence boundary from the fresh attempt"
                );
            }
            other @ (AccountDisposition::DueLookupFailed { .. }
            | AccountDisposition::LeaseHeld { .. }
            | AccountDisposition::EligibilityFailed { .. }
            | AccountDisposition::Sampled(_)
            | AccountDisposition::PersistFailed { .. }
            | AccountDisposition::Spooled { .. }) => {
                panic!("the fresh account must be not yet due, got {other:?}")
            }
        }
        assert_eq!(
            transport.calls("fresh"),
            calls_after_first,
            "no request is issued for an account the due stage vetoed"
        );
        assert!(matches!(
            second.accounts[1].disposition,
            AccountDisposition::Sampled(_)
        ));
        assert_eq!(
            second.workers_spawned, 1,
            "only the due account gets a worker"
        );
    }

    /// Bounded concurrency respected: with a configured bound of two and four
    /// due accounts, no more than two requests are ever in flight at once,
    /// and the bound is actually reached rather than running one at a time.
    #[test]
    fn bounded_concurrency_is_respected_and_reached() {
        let (_scratch_dir, database_path) = fixture_database();
        let transport = ScriptedTransport::new(SharedClock::new());
        let clock = transport.clock.clone();
        let accounts: Vec<BatchAccount<AnthropicAdapter>> = (0..4)
            .map(|index| batch_account(&format!("bound{index}")))
            .collect();

        let report = {
            let repository = Repository::new(&database_path, policy());
            SamplingOrchestrator {
                repository: &repository,
                transport: &transport,
                clock: &clock,
                trigger: Trigger::Timer,
                configuration_fingerprint: "fixture".to_string(),
                holder: LeaseHolder::new("test-holder"),
                lease_ttl: MonotonicDuration::from_seconds(30),
                command_budget: MonotonicDuration::from_seconds(30),
                max_concurrent_requests: 2,
            }
            .run(&accounts)
            .expect("the batch must run")
        };

        assert_eq!(report.workers_spawned, 2);
        assert!(
            transport.max_in_flight() <= 2,
            "the transport saw {} simultaneous requests, bound is 2",
            transport.max_in_flight()
        );
        assert_eq!(
            transport.max_in_flight(),
            2,
            "a bound of two with four accounts must actually run two at once"
        );
    }

    /// A store failure on one account does not discard another account's
    /// evidence: account `broken` carries an empty adapter version, which the
    /// database refuses on the observation row, so its terminal commit fails
    /// after its request succeeded, while `healthy` commits its observation.
    /// `aub-1r3m`: the broken account's reading is durably spooled rather than
    /// lost, and a subsequent drain applies it to the ledger exactly once.
    #[test]
    fn a_store_failure_on_one_account_does_not_discard_another_accounts_evidence() {
        let (scratch_dir, database_path) = fixture_database();
        let transport = ScriptedTransport::new(SharedClock::new());
        let clock = transport.clock.clone();
        let mut broken = batch_account("broken");
        broken.adapter_version = AdapterVersion::new("");
        let healthy = batch_account("healthy");
        let accounts = vec![broken, healthy];

        let report = {
            let repository = Repository::new(&database_path, policy());
            SamplingOrchestrator {
                repository: &repository,
                transport: &transport,
                clock: &clock,
                trigger: Trigger::Timer,
                configuration_fingerprint: "fixture".to_string(),
                holder: LeaseHolder::new("test-holder"),
                lease_ttl: MonotonicDuration::from_seconds(30),
                command_budget: MonotonicDuration::from_seconds(30),
                max_concurrent_requests: 2,
            }
            .run(&accounts)
            .expect("the batch must run")
        };
        let repository = Repository::new(&database_path, policy());

        let broken_attempt_id = if let AccountDisposition::Spooled {
            attempt_id,
            outcome,
            reason,
        } = &report.accounts[0].disposition
        {
            assert_eq!(*outcome, AttemptOutcome::Success);
            assert!(
                reason.contains("adapter_version") || reason.contains("constraint"),
                "the refusal must name the database constraint: {reason}"
            );
            *attempt_id
        } else {
            panic!(
                "the broken account's reading must be spooled rather than lost, got {:?}",
                report.accounts[0].disposition
            );
        };
        let healthy_attempt = sampled(&report.accounts[1]);
        assert_eq!(healthy_attempt.outcome, AttemptOutcome::Success);
        assert!(healthy_attempt.observation_committed);

        // The healthy account's observation is committed and readable, and
        // the broken account's attempt remains as the evidence it is: a
        // started attempt with no terminal result, because the commit that
        // would have written one is exactly what failed.
        assert!(
            repository
                .attempt_started(healthy_attempt.attempt_id)
                .expect("the read must succeed")
                .is_some(),
            "the healthy attempt is persisted"
        );
        assert!(
            repository
                .attempt_result(healthy_attempt.attempt_id)
                .expect("the read must succeed")
                .is_some(),
            "the healthy account's result is committed"
        );
        assert!(
            repository
                .attempt_started(broken_attempt_id)
                .expect("the read must succeed")
                .is_some(),
            "the broken account's attempt start is durable evidence"
        );
        assert!(
            repository
                .attempt_result(broken_attempt_id)
                .expect("the read must succeed")
                .is_none(),
            "the broken account has no terminal result yet; the commit never landed"
        );

        // The reading itself is not gone: a pending record for exactly this
        // attempt sits durably in the spool, which is what `aub doctor`'s
        // `pending-evidence` check surfaces before any drain runs.
        let pending_path = pending_file_path(
            scratch_dir.path(),
            i64::try_from(broken_attempt_id.value()).unwrap(),
        );
        assert!(
            pending_path.exists(),
            "a durable pending record must exist for the spooled attempt at {pending_path:?}"
        );

        // The constraint that made the commit fail is still there, so a drain
        // right now still refuses the terminal fact: this proves the record
        // on disk is real evidence gated on a real condition, not a
        // placeholder that would apply under anything. The full spool then
        // recover then drain cycle, through a failure that clears on its own,
        // is `spool_then_commit_recovers_after_a_transient_commit_failure`
        // below.
        let mut conn = open(&database_path, AccessMode::ReadWrite, &policy()).unwrap();
        let refused = drain_pending(&mut conn, scratch_dir.path())
            .expect_err("draining against the same broken constraint must still refuse");
        assert!(
            refused.to_string().contains("constraint") || refused.to_string().contains("adapter"),
            "the refusal must still name the database constraint: {refused}"
        );
        assert!(
            pending_path.exists(),
            "a refused drain must leave the pending record in place"
        );
    }

    /// End to end: the live sampling path spools a reading whose commit fails
    /// on a transient condition (the writer slot held by another connection),
    /// and once that condition clears, the ordinary startup drain applies the
    /// spooled record and the observation is readable in the ledger. A second
    /// drain is a no-op, proving replay is idempotent rather than merely
    /// silent about a record it no longer finds (`aub-1r3m`).
    ///
    /// Due determination, lease acquisition and request execution run through
    /// `SamplingOrchestrator`'s own stage methods rather than `run`, because
    /// the writer slot must stay free for those (they commit the run row and
    /// the attempt start) and busy only for the one terminal commit under
    /// test; `run` gives no seam to hold the lock over one stage but not the
    /// others. Every call below is the same private method `run` itself
    /// calls, in the same order, over the same production types.
    #[test]
    fn spool_then_commit_recovers_after_a_transient_commit_failure() {
        let (scratch_dir, database_path) = fixture_database();
        let transport = ScriptedTransport::new(SharedClock::new());
        let clock = transport.clock.clone();
        let account = batch_account("recoverable");

        let busy_policy = crate::store::connection::PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(150),
        };
        let repository = Repository::new(&database_path, busy_policy);
        let orchestrator = SamplingOrchestrator {
            repository: &repository,
            transport: &transport,
            clock: &clock,
            trigger: Trigger::Timer,
            configuration_fingerprint: "fixture".to_string(),
            holder: LeaseHolder::new("test-holder"),
            lease_ttl: MonotonicDuration::from_seconds(30),
            command_budget: MonotonicDuration::from_seconds(30),
            max_concurrent_requests: 1,
        };

        let started_at = clock.now();
        let run_id = orchestrator
            .repository
            .start_sample_run(
                orchestrator.trigger,
                started_at,
                &orchestrator.configuration_fingerprint,
            )
            .expect("the run row must commit with no contention");
        let leased = match orchestrator.determine_due(&account, started_at) {
            DueStageOutcome::Due { account_id } => orchestrator
                .acquire_eligibility(0, &account, account_id, run_id)
                .unwrap_or_else(|_| panic!("eligibility must be acquired with no contention")),
            DueStageOutcome::NotYet { .. } | DueStageOutcome::LookupFailed { .. } => {
                panic!("a fresh account with no history must be due")
            }
        };
        let attempt_id = leased.attempt.attempt_id();
        let budget = CommandBudget::new(orchestrator.command_budget, &clock);
        let (mut products, _, _) =
            orchestrator.execute_requests(std::slice::from_ref(&leased), &budget);
        let payload = products[0]
            .take()
            .expect("the one worker must have produced a result");

        // Another writer holds the slot for exactly the terminal-commit call
        // below, so `persist_one`'s own commit is refused past its busy
        // bound, the shape a lock timeout or a busy database produces in
        // production. Everything before this point already committed with
        // the slot free, matching how `run` never holds the writer lock
        // across stages either.
        // The guard comes from the store rather than being built here: the meter
        // layer may not name a SQLite type at all, which the boundary rule
        // `03-meter-no-sqlite` enforces on every file under `src/meter/`.
        let mut holder = open(&database_path, AccessMode::ReadWrite, &busy_policy).unwrap();
        let held = crate::store::connection::hold_writer_slot(&mut holder);
        let disposition = orchestrator.persist_one(&leased, payload);
        let attempt_id_from_disposition =
            if let AccountDisposition::Spooled { attempt_id, .. } = &disposition {
                *attempt_id
            } else {
                panic!("a commit refused by a held writer slot must spool, got {disposition:?}");
            };
        assert_eq!(attempt_id_from_disposition, attempt_id);

        let pending_path = pending_file_path(
            scratch_dir.path(),
            i64::try_from(attempt_id.value()).unwrap(),
        );
        assert!(
            pending_path.exists(),
            "the transient failure must leave a durable pending record"
        );

        // The condition clears: release the writer slot, exactly like a busy
        // database becoming free or a crashed process's lock expiring.
        held.commit().unwrap();
        drop(holder);

        assert!(
            repository
                .attempt_result(attempt_id)
                .expect("the read must succeed")
                .is_none(),
            "nothing committed yet; only the pending record carries the reading"
        );

        // The ordinary startup drain, not a test-only reconstruction: this is
        // the same call every mutating command makes before doing anything
        // else.
        let mut conn = open(&database_path, AccessMode::ReadWrite, &policy()).unwrap();
        let drain_report =
            drain_pending(&mut conn, scratch_dir.path()).expect("the drain must apply cleanly");
        assert_eq!(drain_report.applied, 1, "exactly one record is applied");
        drop(conn);

        assert!(
            !pending_path.exists(),
            "an applied record must be removed from the spool"
        );
        assert!(
            repository
                .attempt_result(attempt_id)
                .expect("the read must succeed")
                .is_some(),
            "the drain must have committed the observation into the ledger"
        );

        // Idempotent replay: draining again with no pending record left finds
        // nothing to apply and nothing already-applied, per the
        // attempt-id-keyed replay contract PLAN.md section 13 describes.
        let mut conn = open(&database_path, AccessMode::ReadWrite, &policy()).unwrap();
        let second_drain =
            drain_pending(&mut conn, scratch_dir.path()).expect("a no-op drain must not fail");
        assert_eq!(
            second_drain.applied, 0,
            "nothing new is applied on a second drain"
        );
        assert_eq!(
            second_drain.already_applied, 0,
            "there is no pending record left to find already-applied"
        );
    }

    /// Budget expiry persists exactly one logical attempt and at most one
    /// result for every due account, including total-budget expiry: the
    /// first account measures inside the budget, the second's request
    /// outlives it and is reported as expired, and the third finds the budget
    /// already spent before any request is issued for it.
    #[test]
    fn budget_expiry_persists_one_attempt_and_one_result_per_due_account() {
        let (_scratch_dir, database_path) = fixture_database();
        let transport = ScriptedTransport::new(SharedClock::new());
        let clock = transport.clock.clone();
        *transport.advance_per_request.lock().unwrap() = Some(MonotonicDuration::from_seconds(1));
        let accounts: Vec<BatchAccount<AnthropicAdapter>> = (0..3)
            .map(|index| batch_account(&format!("budget{index}")))
            .collect();

        let report = {
            let repository = Repository::new(&database_path, policy());
            SamplingOrchestrator {
                repository: &repository,
                transport: &transport,
                clock: &clock,
                trigger: Trigger::Timer,
                configuration_fingerprint: "fixture".to_string(),
                holder: LeaseHolder::new("test-holder"),
                lease_ttl: MonotonicDuration::from_seconds(30),
                command_budget: MonotonicDuration::from_seconds(2),
                max_concurrent_requests: 1,
            }
            .run(&accounts)
            .expect("the batch must run")
        };

        let first = sampled(&report.accounts[0]);
        assert_eq!(first.outcome, AttemptOutcome::Success);
        assert!(first.observation_committed);

        let second = sampled(&report.accounts[1]);
        assert_eq!(
            second.outcome,
            AttemptOutcome::Unreachable(FailureClass::TotalBudgetExpired),
            "a request that outlives the budget is reported as expired, not as its driver outcome"
        );
        let third = sampled(&report.accounts[2]);
        assert_eq!(
            third.outcome,
            AttemptOutcome::Unreachable(FailureClass::TotalBudgetExpired),
            "an account the budget expires before is reported as expired"
        );
        assert_eq!(
            transport.calls("budget2"),
            0,
            "no request is issued for an account the budget expired before"
        );
        assert_eq!(transport.calls("budget1"), 1);
        assert_eq!(transport.calls("budget0"), 1);
    }

    /// The fingerprint is deterministic for the same windows and changes when
    /// any window value, the semantics version, or the window set changes.
    #[test]
    fn the_normalized_fingerprint_is_deterministic_and_distinguishes_windows() {
        fn window(key: &str, used_ppm: i32, resets_nanos: i64) -> MeterWindow {
            MeterWindow::new(
                WindowSemanticKey::new(key),
                WindowScope::AccountWide,
                QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
                ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
                QuantizationSemantics::RoundedToNearest,
                UtcTimestamp::from_unix_nanos(resets_nanos),
                NominalWindowDuration::from_nanos(18_000_000_000_000),
            )
        }
        let windows = vec![
            window("five_hour", 250_000, 9_000),
            window("seven_day", 400_000, 9_000),
        ];
        let first = normalized_fingerprint("semantics-v1", "contract-v1", &windows);
        assert_eq!(
            first,
            normalized_fingerprint("semantics-v1", "contract-v1", &windows),
            "the same reading must fingerprint identically"
        );
        assert_ne!(
            first,
            normalized_fingerprint("semantics-v2", "contract-v1", &windows),
            "the semantics version is part of the fingerprint"
        );
        let mut changed = windows.clone();
        changed[0] = window("five_hour", 250_001, 9_000);
        assert_ne!(
            first,
            normalized_fingerprint("semantics-v1", "contract-v1", &changed),
            "a changed window value must change the fingerprint"
        );
        assert_ne!(
            first,
            normalized_fingerprint("semantics-v1", "contract-v1", &windows[..1]),
            "a changed window set must change the fingerprint"
        );
    }

    /// Concurrent invocations of one batch over one account: exactly one
    /// invocation samples and creates the one logical attempt, and every
    /// other invocation reports why it did not sample, either because another
    /// holder's lease was live when it reached the gate, or because the
    /// evidence snapshot it read already carried the winner's fresh attempt.
    /// Which of the two a loser sees depends on where its read lands relative
    /// to the winner's commit, so the assertion is on the cardinality the
    /// bead demands, not on one disposition. The published projection
    /// generation equals the committed ledger generation.
    #[test]
    fn three_concurrent_batches_for_one_account_create_one_attempt() {
        let (_scratch_dir, database_path) = fixture_database();
        let transport = Arc::new(ScriptedTransport::new(SharedClock::new()));
        let clock = transport.clock.clone();
        let barrier = Arc::new(Barrier::new(3));

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let transport = Arc::clone(&transport);
                let clock = clock.clone();
                let database_path = database_path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let repository = Repository::new(database_path, policy());
                    let account = batch_account("leased");
                    let transport: &ScriptedTransport = &transport;
                    SamplingOrchestrator {
                        repository: &repository,
                        transport,
                        clock: &clock,
                        trigger: Trigger::Timer,
                        configuration_fingerprint: "fixture".to_string(),
                        holder: LeaseHolder::new("test-holder"),
                        lease_ttl: MonotonicDuration::from_seconds(30),
                        command_budget: MonotonicDuration::from_seconds(30),
                        max_concurrent_requests: 1,
                    }
                    .run(std::slice::from_ref(&account))
                    .expect("each batch must run")
                })
            })
            .collect();
        let reports: Vec<BatchReport> = handles
            .into_iter()
            .map(|handle| handle.join().expect("the batch thread must not panic"))
            .collect();

        let mut sampled_count = 0usize;
        for report in &reports {
            for entry in &report.accounts {
                match &entry.disposition {
                    // A winner's outcome, whether or not its commit landed:
                    // both mean this invocation held the lease and ran the
                    // request, which is the one-winner property under test.
                    AccountDisposition::Sampled(_) | AccountDisposition::Spooled { .. } => {
                        sampled_count += 1
                    }
                    AccountDisposition::NotYet { .. } | AccountDisposition::LeaseHeld { .. } => {}
                    other @ (AccountDisposition::DueLookupFailed { .. }
                    | AccountDisposition::EligibilityFailed { .. }
                    | AccountDisposition::PersistFailed { .. }) => panic!(
                        "a loser must report why it did not sample, never sample twice: {other:?}"
                    ),
                }
            }
        }
        assert_eq!(sampled_count, 1, "exactly one invocation samples");

        // The store holds exactly one logical attempt for the account: one
        // started attempt with exactly one terminal result, referencing the
        // one sample run the winning invocation opened. The losers' refusal
        // dispositions created nothing.
        let conn = open(
            &database_path,
            AccessMode::ReadOnly,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(2_000),
            },
        )
        .expect("the database must open for the cardinality read");
        let (starts, results) =
            meter_attempt::count_attempts(&conn).expect("the attempt count must read");
        assert_eq!(starts, 1, "three invocations create exactly one attempt");
        assert_eq!(results, 1, "the one attempt carries exactly one result");
        let attempt_row_id = row_id_of(AttemptId::new(1)).expect("row id 1 is a storage row id");
        let stored = meter_attempt::attempt_by_row_id(&conn, attempt_row_id)
            .expect("the attempt read must succeed")
            .expect("the one attempt must exist");
        assert_eq!(
            stored.run_id,
            reports
                .iter()
                .find(|report| report
                    .accounts
                    .iter()
                    .any(|entry| matches!(entry.disposition, AccountDisposition::Sampled(_))))
                .expect("one report sampled")
                .run_id,
            "the attempt references the sample run of the invocation that created it"
        );
        drop(conn);

        // The published projection generation equals the committed ledger
        // generation: the winner's publication names what the file records,
        // and the database agrees with it after every invocation finished.
        let published = reports
            .iter()
            .flat_map(|report| report.accounts.iter())
            .find_map(|entry| match &entry.disposition {
                AccountDisposition::Sampled(sampled) => sampled.publication.published_generation(),
                AccountDisposition::NotYet { .. }
                | AccountDisposition::DueLookupFailed { .. }
                | AccountDisposition::LeaseHeld { .. }
                | AccountDisposition::EligibilityFailed { .. }
                | AccountDisposition::PersistFailed { .. }
                | AccountDisposition::Spooled { .. } => None,
            })
            .expect("the winning invocation published");
        let conn = open(
            &database_path,
            AccessMode::ReadOnly,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(2_000),
            },
        )
        .expect("the database must open for the generation read");
        assert_eq!(
            ledger_generation::current(&conn).expect("the generation must read"),
            published,
            "the published projection generation equals the committed ledger generation"
        );
    }
}

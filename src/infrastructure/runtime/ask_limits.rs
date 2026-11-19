//! Producer-owned limits for headless `ask`. Other turn routes do not opt in.

mod money;

use std::collections::HashMap;
use std::future::{Future, pending, poll_fn};
use std::num::NonZeroU64;
use std::pin::pin;
use std::time::Duration;

use chrono::{DateTime, Local, MappedLocalTime, NaiveDate, Offset, TimeZone, Utc};
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::domain::models::{AppConfig, UsageLedgerEntry, UsageProvenance, pricing::PricingConfig};
use crate::domain::ports::{LedgerReadMode, UsageLedgerPort};
use crate::domain::services::pricing_resolver;

use self::money::{Decimal, Usd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskLimitReason {
    MaxDuration,
    AccountingError,
    DailyBudget,
    MaxCost,
    MaxTurns,
}

impl AskLimitReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MaxDuration => "max_duration",
            Self::AccountingError => "accounting_error",
            Self::DailyBudget => "daily_budget",
            Self::MaxCost => "max_cost",
            Self::MaxTurns => "max_turns",
        }
    }
}

#[derive(Debug)]
pub(crate) struct AskStop {
    pub(crate) reason: AskLimitReason,
    pub(crate) detail: Option<String>,
}

impl AskStop {
    fn new(reason: AskLimitReason) -> Self {
        Self {
            reason,
            detail: None,
        }
    }

    fn accounting(detail: &'static str) -> Self {
        Self {
            reason: AskLimitReason::AccountingError,
            detail: Some(detail.into()),
        }
    }
}

/// Opt-in state for `run_turn`; only the headless CLI constructs it.
pub struct AskExecution {
    deadline: Option<Instant>,
    configuration_error: Option<&'static str>,
    max_turns: Option<NonZeroU64>,
    admitted: u64,
    max_cost_usd: Option<Usd>,
    invocation_usd: Usd,
    daily_limit_usd: Option<Usd>,
    last_snapshot: Option<DailySnapshot>,
    pricing: HashMap<String, PricingConfig>,
    stop_tx: Option<oneshot::Sender<AskStop>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BudgetDay {
    start_ms: i64,
    end_ms: i64,
}

struct DailySnapshot {
    day: BudgetDay,
    spent_usd: Usd,
}

impl AskExecution {
    pub(crate) fn new(
        max_turns: Option<NonZeroU64>,
        max_duration: Option<Duration>,
        max_cost_usd: Option<f64>,
        config: &AppConfig,
        stop_tx: oneshot::Sender<AskStop>,
    ) -> Option<Self> {
        let daily_limit_usd = config.budget.daily_limit_usd;
        if max_turns.is_none()
            && max_duration.is_none()
            && max_cost_usd.is_none()
            && daily_limit_usd.is_none()
        {
            return None;
        }
        let deadline = max_duration.and_then(|duration| Instant::now().checked_add(duration));
        let exact_max_cost = max_cost_usd
            .filter(|&limit| limit > 0.0)
            .and_then(Usd::from_f64);
        let exact_daily_limit = daily_limit_usd.and_then(Usd::from_f64);
        let configuration_error = if max_duration.is_some() && deadline.is_none() {
            Some("maximum duration exceeds the monotonic clock range")
        } else if max_cost_usd.is_some() && exact_max_cost.is_none() {
            Some("maximum cost must be finite and positive")
        } else if daily_limit_usd.is_some() && exact_daily_limit.is_none() {
            Some("daily budget must be finite and nonnegative")
        } else {
            None
        };
        Some(Self {
            deadline,
            configuration_error,
            max_turns,
            admitted: 0,
            max_cost_usd: exact_max_cost,
            invocation_usd: Usd::ZERO,
            daily_limit_usd: exact_daily_limit,
            last_snapshot: None,
            pricing: if max_cost_usd.is_some() || daily_limit_usd.is_some() {
                config.pricing.clone()
            } else {
                HashMap::new()
            },
            stop_tx: Some(stop_tx),
        })
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(super) fn money_configured(&self) -> bool {
        self.daily_limit_usd.is_some() || self.max_cost_usd.is_some()
    }

    // A denied call never reaches this point. The driver caps the total number
    // of iterations independently, so a u64 admitted count cannot overflow.
    pub(super) fn mark_admitted(&mut self) {
        self.admitted += 1;
    }

    pub(super) fn stopped_before_work(&mut self) -> bool {
        let stop = self.clock_or_configuration_stop();
        self.publish(stop)
    }

    fn clock_or_configuration_stop(&self) -> Option<AskStop> {
        if deadline_elapsed(self.deadline) {
            Some(AskStop::new(AskLimitReason::MaxDuration))
        } else {
            self.configuration_error.map(AskStop::accounting)
        }
    }

    fn publish(&mut self, stop: Option<AskStop>) -> bool {
        if let Some(stop) = stop {
            if let Some(tx) = self.stop_tx.take() {
                let _ = tx.send(stop);
            }
            true
        } else {
            false
        }
    }

    fn decision(
        &self,
        accounting: Result<(), &'static str>,
        continuation: bool,
    ) -> Option<AskStop> {
        if let Some(stop) = self.clock_or_configuration_stop() {
            return Some(stop);
        }
        if let Err(detail) = accounting {
            return Some(AskStop::accounting(detail));
        }
        if let (Some(limit), Some(snapshot)) = (&self.daily_limit_usd, &self.last_snapshot) {
            if &snapshot.spent_usd >= limit {
                return Some(AskStop {
                    reason: AskLimitReason::DailyBudget,
                    detail: Some(format!(
                        "daily spend ${} reached the ${limit} budget",
                        snapshot.spent_usd
                    )),
                });
            }
        }
        if let Some(limit) = &self.max_cost_usd {
            if &self.invocation_usd >= limit {
                return Some(AskStop {
                    reason: AskLimitReason::MaxCost,
                    detail: Some(format!(
                        "invocation spend ${} reached the ${limit} limit",
                        self.invocation_usd
                    )),
                });
            }
        }
        if continuation
            && self
                .max_turns
                .is_some_and(|limit| self.admitted >= limit.get())
        {
            return Some(AskStop::new(AskLimitReason::MaxTurns));
        }
        None
    }

    /// Before the first call, load the strict daily snapshot. Later boundaries
    /// already read it after settlement; refresh only if the local day changed
    /// while tools were running. Cancellation during a read denies admission.
    pub(super) async fn may_admit(
        &mut self,
        ledger: &dyn UsageLedgerPort,
        cancel: &CancellationToken,
    ) -> bool {
        if self.stopped_before_work() || cancel.is_cancelled() {
            return false;
        }
        let accounting = if self.daily_limit_usd.is_some() {
            match current_budget_day() {
                Ok(day)
                    if self
                        .last_snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.day == day) =>
                {
                    Ok(())
                }
                Ok(day) => {
                    match interruptible(
                        self.deadline,
                        Some(cancel),
                        self.read_snapshot(ledger, day, current_budget_day),
                    )
                    .await
                    {
                        Ok(result) => self.store_snapshot(result),
                        Err(_) => {
                            self.stopped_before_work();
                            return false;
                        }
                    }
                }
                Err(detail) => Err(detail),
            }
        } else {
            Ok(())
        };
        !self.check_boundary(accounting, true) && !cancel.is_cancelled()
    }

    fn check_boundary(&mut self, accounting: Result<(), &'static str>, continuation: bool) -> bool {
        let stop = self.decision(accounting, continuation);
        self.publish(stop)
    }

    /// Validate the call itself even if its receipt won't be in the daily
    /// snapshot (e.g. local midnight elapsed during the append). The daily
    /// sum comes only from the ledger, never from adding this price to it.
    pub(super) fn price_settled_call(
        &mut self,
        entry: &UsageLedgerEntry,
    ) -> Result<(), &'static str> {
        if self.max_cost_usd.is_some() {
            Self::add_strict_cost(&self.pricing, entry, &mut self.invocation_usd)?;
        } else if self.daily_limit_usd.is_some() {
            let mut cost = Usd::ZERO;
            Self::add_strict_cost(&self.pricing, entry, &mut cost)?;
        }
        Ok(())
    }

    /// Called after the append has resolved. A deadline can cancel the daily
    /// read, but can't discard a write whose result is still outstanding.
    pub(super) async fn stopped_after_settlement(
        &mut self,
        ledger: &dyn UsageLedgerPort,
        accounting: Result<(), &'static str>,
        continuation: bool,
    ) -> bool {
        let accounting = if self.daily_limit_usd.is_some()
            && accounting.is_ok()
            && !deadline_elapsed(self.deadline)
        {
            match current_budget_day() {
                Ok(day) => match interruptible(
                    self.deadline,
                    None,
                    self.read_snapshot(ledger, day, current_budget_day),
                )
                .await
                {
                    Ok(result) => self.store_snapshot(result),
                    Err(_) => Ok(()), // The elapsed deadline has terminal priority.
                },
                Err(detail) => Err(detail),
            }
        } else {
            accounting
        };
        self.check_boundary(accounting, continuation)
    }

    fn store_snapshot(
        &mut self,
        result: Result<DailySnapshot, &'static str>,
    ) -> Result<(), &'static str> {
        self.last_snapshot = Some(result?);
        Ok(())
    }

    async fn read_snapshot(
        &self,
        ledger: &dyn UsageLedgerPort,
        mut day: BudgetDay,
        mut current_day: impl FnMut() -> Result<BudgetDay, &'static str>,
    ) -> Result<DailySnapshot, &'static str> {
        loop {
            let spent_usd = ledger
                .read_since(
                    day.start_ms,
                    LedgerReadMode::Strict {
                        end_unix_ms: day.end_ms,
                    },
                )
                .await
                .map_err(|e| {
                    tracing::warn!("Strict ask usage ledger read failed: {e}");
                    "could not read a strict daily usage snapshot"
                })
                .and_then(|entries| {
                    let mut sum = Usd::ZERO;
                    for entry in &entries {
                        Self::add_strict_cost(&self.pricing, entry, &mut sum)?;
                    }
                    Ok(sum)
                });
            // A read that spans midnight isn't a snapshot of the new local day.
            let current = current_day()?;
            if current != day {
                day = current;
                continue;
            }
            return spent_usd.map(|spent_usd| DailySnapshot { day, spent_usd });
        }
    }

    fn add_strict_cost(
        pricing: &HashMap<String, PricingConfig>,
        entry: &UsageLedgerEntry,
        sum: &mut Usd,
    ) -> Result<(), &'static str> {
        if entry.provenance != Some(UsageProvenance::Observed) {
            return Err("usage counters are missing or unverified");
        }
        let usage = &entry.usage;
        let reasoning = usage.reasoning_tokens.unwrap_or(0);
        let output = usage
            .tokens_out
            .checked_sub(reasoning)
            .ok_or("recorded reasoning usage exceeds total output usage")?;
        let price = pricing_resolver::lookup_pricing(pricing, &entry.model)
            .ok_or("no price is available for recorded usage")?;
        let decimal =
            |rate| Decimal::from_f64(rate).ok_or("usage prices must be finite and nonnegative");
        let input_rate = decimal(price.input_per_million)?;
        let output_rate = decimal(price.output_per_million)?;
        let cache_write_rate = match price.cache_creation_per_million {
            Some(rate) => decimal(rate)?,
            None => input_rate.scaled(125, 2),
        };
        let cache_read_rate = match price.cache_read_per_million {
            Some(rate) => decimal(rate)?,
            None => input_rate.scaled(1, 1),
        };
        let reasoning_rate = match price.reasoning_per_million {
            Some(rate) => decimal(rate)?,
            None => output_rate,
        };
        sum.add_tokens(input_rate, usage.tokens_in)?;
        sum.add_tokens(output_rate, output)?;
        sum.add_tokens(reasoning_rate, reasoning)?;
        sum.add_tokens(cache_write_rate, usage.cache_creation_tokens.unwrap_or(0))?;
        sum.add_tokens(cache_read_rate, usage.cache_read_tokens.unwrap_or(0))?;
        sum.check_range()
    }
}

fn current_budget_day() -> Result<BudgetDay, &'static str> {
    budget_day(&Local, Utc::now().timestamp_millis())
        .ok_or("could not establish the local budget day window")
}

fn budget_day(zone: &impl TimeZone, now_ms: i64) -> Option<BudgetDay> {
    let date = local_date_at(zone, now_ms)?;
    let start_ms = beginning_of_local_date(zone, date)?;
    let end_ms = beginning_of_local_date(zone, date.succ_opt()?)?;
    ((start_ms..end_ms).contains(&now_ms)
        && local_date_at(zone, start_ms)? == date
        && local_date_at(zone, end_ms - 1)? == date)
        .then_some(BudgetDay { start_ms, end_ms })
}

fn local_date_at(zone: &impl TimeZone, timestamp_ms: i64) -> Option<NaiveDate> {
    let utc = DateTime::from_timestamp_millis(timestamp_ms)?.naive_utc();
    Some(
        utc.checked_add_offset(zone.offset_from_utc_datetime(&utc).fix())?
            .date(),
    )
}

/// Find the first instant of the date, or of a later date if this date was skipped.
fn beginning_of_local_date(zone: &impl TimeZone, date: NaiveDate) -> Option<i64> {
    let midnight = date.and_hms_opt(0, 0, 0)?;
    let candidate = match zone.from_local_datetime(&midnight) {
        MappedLocalTime::Single(time) => Some(time.timestamp_millis()),
        MappedLocalTime::Ambiguous(a, b) => {
            let (a, b) = (a.timestamp_millis(), b.timestamp_millis());
            let (earlier, later) = (a.min(b), a.max(b));
            if local_date_at(zone, earlier)? < date {
                // Some zone implementations include the old offset at the
                // transition itself, when it is no longer in force.
                Some(later)
            } else {
                // A rollback within the new date keeps its first midnight.
                // A return to the preceding date is not a contiguous window.
                if local_date_at(zone, later - 1)? != date {
                    return None;
                }
                Some(earlier)
            }
        }
        MappedLocalTime::None => None,
    };
    let is_boundary = |instant| {
        Some(local_date_at(zone, instant - 1)? < date && local_date_at(zone, instant)? >= date)
    };
    if let Some(instant) = candidate
        && is_boundary(instant)?
    {
        return Some(instant);
    }

    // A forward offset change can skip midnight or even the entire date.
    // Offsets are strictly within ±24 hours, so UTC brackets the change.
    const DAY_MS: i64 = 86_400_000;
    let utc_midnight = midnight.and_utc().timestamp_millis();
    let (mut before, mut after) = (utc_midnight - DAY_MS, utc_midnight + DAY_MS);
    while after - before > 1 {
        let probe = before + (after - before) / 2;
        if local_date_at(zone, probe)? < date {
            before = probe;
        } else {
            after = probe;
        }
    }
    is_boundary(after)?.then_some(after)
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Interruption {
    Deadline,
    Cancelled,
}

pub(super) fn deadline_elapsed(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

/// Tokio's timer wheel has a finite maximum tick and rounds the requested
/// instant. Keep each timer within a short horizon even for a huge, but
/// representable, deadline; the absolute monotonic deadline remains authoritative.
async fn wait_for_deadline(deadline: Option<Instant>) {
    let Some(deadline) = deadline else {
        return pending().await;
    };
    const TIMER_HORIZON: Duration = Duration::from_secs(86_400);
    const TIMER_ROUNDING: Duration = Duration::from_nanos(999_999);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let mut wake_at = now + deadline.duration_since(now).min(TIMER_HORIZON);
        if wake_at.checked_add(TIMER_ROUNDING).is_none() {
            wake_at -= TIMER_ROUNDING;
        }
        if wake_at > now {
            tokio::time::sleep_until(wake_at).await;
            if Instant::now() >= wake_at {
                continue;
            }
        }
        // Don't spin if the timer wheel saturated or the clock is inside
        // the last sub-millisecond interval the wheel cannot represent.
        tokio::task::yield_now().await;
    }
}

/// The deadline and cancellation are polled before the work, including its
/// first poll. Dropping the work drops its scoped wait and held resources.
pub(super) async fn interruptible<T>(
    deadline: Option<Instant>,
    cancel: Option<&CancellationToken>,
    work: impl Future<Output = T>,
) -> Result<T, Interruption> {
    let mut timer = pin!(wait_for_deadline(deadline));
    let mut cancelled = pin!(async {
        match cancel {
            Some(cancel) => cancel.cancelled().await,
            None => pending().await,
        }
    });
    let mut work = pin!(work);
    poll_fn(|cx| {
        // The timer uses millisecond ticks.
        // Check the exact Instant even when something else woke this task.
        if deadline_elapsed(deadline) || timer.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(Err(Interruption::Deadline));
        }
        if cancelled.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(Err(Interruption::Cancelled));
        }
        work.as_mut().poll(cx).map(Ok)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDateTime};

    use crate::domain::errors::StorageError;
    use crate::domain::models::UsageInfo;
    use crate::domain::services::cost_calculator::test_support::{ledger_row, pricing};

    #[test]
    fn common_boundary_prioritizes_limits_and_allows_normal_last_call() {
        let (tx, _) = oneshot::channel();
        let mut config = AppConfig::default();
        config.budget.daily_limit_usd = Some(5.0);
        let mut execution =
            AskExecution::new(NonZeroU64::new(1), None, Some(5.0), &config, tx).unwrap();
        execution.mark_admitted();
        assert!(execution.decision(Ok(()), false).is_none());
        assert_eq!(
            execution.decision(Ok(()), true).unwrap().reason,
            AskLimitReason::MaxTurns
        );

        execution.invocation_usd = Usd::from_f64(5.0).unwrap();
        assert_eq!(
            execution.decision(Ok(()), true).unwrap().reason,
            AskLimitReason::MaxCost
        );
        execution.last_snapshot = Some(DailySnapshot {
            day: BudgetDay {
                start_ms: 0,
                end_ms: 1,
            },
            spent_usd: Usd::from_f64(5.0).unwrap(),
        });
        assert_eq!(
            execution.decision(Ok(()), true).unwrap().reason,
            AskLimitReason::DailyBudget
        );
        assert_eq!(
            execution
                .decision(Err("append failed"), true)
                .unwrap()
                .reason,
            AskLimitReason::AccountingError
        );
        execution.deadline = Some(Instant::now());
        assert_eq!(
            execution
                .decision(Err("append failed"), true)
                .unwrap()
                .reason,
            AskLimitReason::MaxDuration
        );
    }

    #[test]
    fn strict_cost_distinguishes_verifiable_zero_unknown_and_invalid_price() {
        let (tx, _) = oneshot::channel();
        let config = AppConfig {
            pricing: pricing("priced", 2.0, 3.0, 1.0, None),
            ..AppConfig::default()
        };
        let mut execution = AskExecution::new(None, None, Some(1.0), &config, tx).unwrap();
        let mut entry = ledger_row(
            &UsageInfo::from_inclusive_counts(0, 0, None, None, None),
            "priced",
        );
        assert!(execution.price_settled_call(&entry).is_err());
        entry.provenance = Some(UsageProvenance::Observed);
        assert_eq!(execution.price_settled_call(&entry), Ok(()));
        entry.usage.tokens_in = 250_000;
        entry.usage.cache_read_tokens = Some(500_000);
        assert_eq!(execution.price_settled_call(&entry), Ok(()));
        assert_eq!(execution.invocation_usd, Usd::from_f64(1.0).unwrap());

        execution
            .pricing
            .get_mut("priced")
            .unwrap()
            .output_per_million = -1.0;
        assert!(execution.price_settled_call(&entry).is_err());
    }

    fn observed_row(timestamp_ms: i64, input: u32, output: u32) -> UsageLedgerEntry {
        let mut entry = ledger_row(
            &UsageInfo::from_inclusive_counts(input, output, None, None, None),
            "priced",
        );
        entry.timestamp_ms = timestamp_ms;
        entry.provenance = Some(UsageProvenance::Observed);
        entry
    }

    struct SnapshotLedger(Vec<UsageLedgerEntry>);

    #[async_trait::async_trait]
    impl UsageLedgerPort for SnapshotLedger {
        async fn append(&self, _: UsageLedgerEntry) -> Result<(), StorageError> {
            unreachable!("snapshot tests only read the ledger")
        }

        async fn read_session(&self, _: &str) -> Result<Vec<UsageLedgerEntry>, StorageError> {
            unreachable!("daily snapshots read across sessions")
        }

        async fn read_since(
            &self,
            since_unix_ms: i64,
            mode: LedgerReadMode,
        ) -> Result<Vec<UsageLedgerEntry>, StorageError> {
            let LedgerReadMode::Strict { end_unix_ms } = mode else {
                panic!("money-limited ask must read a strict snapshot");
            };
            let window = since_unix_ms..end_unix_ms;
            Ok(self
                .0
                .iter()
                .filter(|entry| window.contains(&entry.timestamp_ms))
                .cloned()
                .collect())
        }
    }

    #[tokio::test]
    async fn strict_money_stops_at_decimal_equality_but_not_adjacent_below() {
        let (input, output) = (
            observed_row(10, 1_000_000, 0),
            observed_row(11, 0, 1_000_000),
        );
        let day = BudgetDay {
            start_ms: 10,
            end_ms: 12,
        };
        for (price, reached) in [
            (0.7_f64.next_down(), false),
            (0.7, true),
            (0.7_f64.next_up(), true),
        ] {
            let (tx, _) = oneshot::channel();
            let mut config = AppConfig {
                pricing: pricing("priced", 0.1, price, 0.0, None),
                ..AppConfig::default()
            };
            config.budget.daily_limit_usd = Some(0.8);
            let mut execution = AskExecution::new(None, None, Some(0.8), &config, tx).unwrap();
            execution.price_settled_call(&input).unwrap();
            assert!(execution.decision(Ok(()), true).is_none());
            execution.price_settled_call(&output).unwrap();
            assert_eq!(
                execution.decision(Ok(()), true).map(|stop| stop.reason),
                reached.then_some(AskLimitReason::MaxCost)
            );
            let ledger = SnapshotLedger(vec![output.clone(), input.clone()]);
            let snapshot = execution.read_snapshot(&ledger, day, || Ok(day)).await;
            execution.store_snapshot(snapshot).unwrap();
            assert_eq!(
                execution.decision(Ok(()), true).map(|stop| stop.reason),
                reached.then_some(AskLimitReason::DailyBudget)
            );
        }
    }

    #[test]
    fn strict_money_prices_disjoint_tokens_with_exact_defaults() {
        let (tx, _) = oneshot::channel();
        let mut config = AppConfig {
            pricing: pricing("priced", 0.7, 0.3, 0.0, None),
            ..AppConfig::default()
        };
        let price = config.pricing.get_mut("priced").unwrap();
        price.cache_read_per_million = None;
        price.reasoning_per_million = Some(0.4);
        let mut execution = AskExecution::new(None, None, Some(0.7875), &config, tx).unwrap();
        let mut entry = observed_row(0, 500_000, 500_000);
        entry.usage.cache_creation_tokens = Some(250_000);
        entry.usage.cache_read_tokens = Some(625_000);
        entry.usage.reasoning_tokens = Some(250_000);
        // 0.35 base input + 0.075 output + 0.10 reasoning + 0.21875 write + 0.04375 read.
        execution.price_settled_call(&entry).unwrap();
        assert_eq!(execution.invocation_usd.to_string(), "0.7875");
        assert_eq!(
            execution.decision(Ok(()), true).unwrap().reason,
            AskLimitReason::MaxCost
        );
        entry.usage.reasoning_tokens = Some(500_001);
        assert!(execution.price_settled_call(&entry).is_err());
    }

    #[test]
    fn strict_money_preserves_subnormal_amounts_and_rejects_overflow() {
        let minimum = f64::from_bits(1);
        let (tx, _) = oneshot::channel();
        let config = AppConfig {
            pricing: pricing("priced", minimum, f64::MAX, 0.0, None),
            ..AppConfig::default()
        };
        let mut execution = AskExecution::new(None, None, Some(minimum), &config, tx).unwrap();
        execution
            .price_settled_call(&observed_row(0, 999_999, 0))
            .unwrap();
        assert!(execution.invocation_usd > Usd::ZERO);
        assert!(execution.decision(Ok(()), true).is_none());
        execution
            .price_settled_call(&observed_row(0, 1, 0))
            .unwrap();
        assert_eq!(
            execution.decision(Ok(()), true).unwrap().reason,
            AskLimitReason::MaxCost
        );

        let (tx, _) = oneshot::channel();
        let mut execution = AskExecution::new(None, None, Some(f64::MAX), &config, tx).unwrap();
        execution
            .price_settled_call(&observed_row(0, 0, 1_000_000))
            .unwrap();
        assert_eq!(
            execution.decision(Ok(()), true).unwrap().reason,
            AskLimitReason::MaxCost
        );
        assert_eq!(
            execution.price_settled_call(&observed_row(0, 1, 0)),
            Err("usage cost overflow")
        );
    }

    fn utc_ms(day: u32, hour: u32) -> i64 {
        Utc.with_ymd_and_hms(2026, 1, day, hour, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    #[tokio::test]
    async fn strict_snapshot_excludes_next_day_and_reloads_on_rollover() {
        let zone = FixedOffset::east_opt(3_600).unwrap();
        let today = budget_day(&zone, utc_ms(2, 12)).unwrap();
        let tomorrow = budget_day(&zone, today.end_ms).unwrap();
        assert_eq!(
            today,
            BudgetDay {
                start_ms: utc_ms(1, 23),
                end_ms: utc_ms(2, 23),
            }
        );
        assert_eq!(tomorrow.start_ms, today.end_ms);
        let ledger = SnapshotLedger(vec![
            observed_row(today.start_ms - 1, 0, 1_000_000),
            observed_row(today.start_ms, 1_000_000, 0),
            observed_row(tomorrow.start_ms, 0, 1_000_000),
        ]);
        let (tx, _) = oneshot::channel();
        let mut config = AppConfig {
            pricing: pricing("priced", 0.1, 0.7, 0.0, None),
            ..AppConfig::default()
        };
        config.budget.daily_limit_usd = Some(0.7);
        let mut execution = AskExecution::new(None, None, None, &config, tx).unwrap();
        let snapshot = execution
            .read_snapshot(&ledger, today, || Ok(today))
            .await
            .unwrap();
        assert_eq!(snapshot.spent_usd, Usd::from_f64(0.1).unwrap());
        execution.last_snapshot = Some(snapshot);
        assert!(execution.decision(Ok(()), true).is_none());
        let mut reads = 0;
        let snapshot = execution
            .read_snapshot(&ledger, today, || {
                reads += 1;
                Ok(tomorrow)
            })
            .await
            .unwrap();
        assert_eq!(reads, 2);
        assert_eq!(snapshot.day, tomorrow);
        assert_eq!(snapshot.spent_usd, Usd::from_f64(0.7).unwrap());
        execution.last_snapshot = Some(snapshot);
        assert_eq!(
            execution.decision(Ok(()), true).unwrap().reason,
            AskLimitReason::DailyBudget
        );
    }

    #[derive(Clone, Debug)]
    struct Shift {
        utc: NaiveDateTime,
        before: FixedOffset,
        after: FixedOffset,
    }

    #[derive(Clone, Debug)]
    struct ShiftOffset(Shift, FixedOffset);

    impl Offset for ShiftOffset {
        fn fix(&self) -> FixedOffset {
            self.1
        }
    }

    impl TimeZone for Shift {
        type Offset = ShiftOffset;

        fn from_offset(offset: &Self::Offset) -> Self {
            offset.0.clone()
        }

        fn offset_from_local_date(&self, date: &NaiveDate) -> MappedLocalTime<Self::Offset> {
            self.offset_from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<Self::Offset> {
            let before = local.checked_sub_offset(self.before).unwrap() < self.utc;
            let after = local.checked_sub_offset(self.after).unwrap() >= self.utc;
            let prior = ShiftOffset(self.clone(), self.before);
            let next = ShiftOffset(self.clone(), self.after);
            match (before, after) {
                (true, true) => MappedLocalTime::Ambiguous(next, prior),
                (true, false) => MappedLocalTime::Single(prior),
                (false, true) => MappedLocalTime::Single(next),
                (false, false) => MappedLocalTime::None,
            }
        }

        fn offset_from_utc_date(&self, date: &NaiveDate) -> Self::Offset {
            self.offset_from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Self::Offset {
            ShiftOffset(
                self.clone(),
                if *utc < self.utc {
                    self.before
                } else {
                    self.after
                },
            )
        }
    }

    #[test]
    fn budget_day_handles_skipped_and_repeated_midnight_without_host_tz() {
        let zone = |hour, before, after| Shift {
            utc: DateTime::from_timestamp_millis(utc_ms(2, hour))
                .unwrap()
                .naive_utc(),
            before: FixedOffset::east_opt(before * 3_600).unwrap(),
            after: FixedOffset::east_opt(after * 3_600).unwrap(),
        };
        let gap = zone(0, 0, 1);
        let previous = budget_day(&gap, utc_ms(1, 12)).unwrap();
        let day = budget_day(&gap, utc_ms(2, 12)).unwrap();
        assert_eq!(day.start_ms, utc_ms(2, 0));
        assert_eq!(day.end_ms, utc_ms(2, 23));
        assert_eq!(previous.end_ms, day.start_ms);

        let repeat = zone(0, 1, 0);
        let day = budget_day(&repeat, utc_ms(2, 12)).unwrap();
        assert_eq!(day.start_ms, utc_ms(1, 23));
        assert_eq!(day.end_ms, utc_ms(3, 0));
        assert!(budget_day(&zone(0, 1, -1), utc_ms(2, 12)).is_none());

        let skip_date = zone(10, -10, 14);
        let previous = budget_day(&skip_date, utc_ms(1, 12)).unwrap();
        let next = budget_day(&skip_date, utc_ms(2, 12)).unwrap();
        assert_eq!(previous.end_ms, utc_ms(2, 10));
        assert_eq!(next.start_ms, previous.end_ms);
        assert_eq!(next.end_ms, utc_ms(3, 10));
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_wait_accepts_an_instant_near_clock_overflow() {
        let start = Instant::now();
        let nanos = |n: u128| Duration::new((n / 1_000_000_000) as u64, (n % 1_000_000_000) as u32);
        let (mut lower, mut upper) = (0, Duration::MAX.as_nanos());
        while upper - lower > 1 {
            let middle = (upper + lower) / 2;
            if start.checked_add(nanos(middle)).is_some() {
                lower = middle;
            } else {
                upper = middle;
            }
        }
        let distant = start.checked_add(nanos(lower)).unwrap();
        assert!(distant.checked_add(Duration::from_nanos(999_999)).is_none());
        assert_eq!(
            interruptible(Some(distant), None, async { 42 }).await.ok(),
            Some(42)
        );
        assert_eq!(Instant::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_wait_interrupts_unready_work() {
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(matches!(
            interruptible(Some(deadline), None, pending::<()>()).await,
            Err(Interruption::Deadline)
        ));
        assert!(Instant::now() >= deadline);
    }

    #[test]
    fn unrepresentable_duration_is_an_error_not_an_inactive_limit() {
        let (tx, mut rx) = oneshot::channel();
        let config = AppConfig::default();
        let mut execution = AskExecution::new(None, Some(Duration::MAX), None, &config, tx)
            .expect("configured limit");
        assert!(execution.stopped_before_work());
        assert_eq!(
            rx.try_recv().unwrap().reason,
            AskLimitReason::AccountingError
        );
    }
}

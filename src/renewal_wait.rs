//! Keep a sent profile-install request alive while an already-connected phone
//! is unresponsive. A timeout is not evidence that the device is locked.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

pub const CANCELLED: &str = "Renewal wait canceled by user.";

#[derive(Default)]
pub struct Control {
    waiting: AtomicBool,
    canceled: AtomicBool,
    wake: Notify,
}

impl Control {
    pub fn reset(&self) {
        self.waiting.store(false, Ordering::SeqCst);
        self.canceled.store(false, Ordering::SeqCst);
    }

    pub fn is_waiting(&self) -> bool {
        self.waiting.load(Ordering::SeqCst)
    }

    pub fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    async fn canceled(&self) {
        loop {
            let notified = self.wake.notified();
            if self.is_canceled() {
                return;
            }
            notified.await;
        }
    }
}

struct Waiting<'a>(&'a Control);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.waiting.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Probe {
    Installed,
    Connected,
    Disconnected(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Completion {
    Acknowledged,
    Verified,
}

#[derive(Clone, Copy)]
pub struct Policy {
    pub grace: Duration,
    pub poll: Duration,
    pub max_wait: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(60),
            poll: Duration::from_secs(15),
            max_wait: Duration::from_secs(12 * 60 * 60),
        }
    }
}

/// Poll the *same* install future throughout: dropping it at the grace period
/// would abandon its response and leave the protocol stream unsafe to reuse.
/// Verification must use a separate connection and the exact new profile UUID.
pub async fn await_install<I, P, F>(
    install: I,
    mut probe: P,
    control: &Control,
    policy: Policy,
    mut on_wait: impl FnMut(),
) -> Result<Completion, String>
where
    I: Future<Output = Result<(), String>>,
    P: FnMut() -> F,
    F: Future<Output = Probe>,
{
    tokio::pin!(install);
    tokio::select! {
        biased;
        _ = control.canceled() => return Err(CANCELLED.into()),
        result = &mut install => return result.map(|_| Completion::Acknowledged),
        _ = tokio::time::sleep(policy.grace) => {}
    }

    control.waiting.store(true, Ordering::SeqCst);
    let _waiting = Waiting(control);
    on_wait();
    let deadline = tokio::time::sleep(policy.max_wait);
    tokio::pin!(deadline);

    loop {
        // Probe immediately at the grace period, then at a modest interval.
        // Keep listening to the pending install even while a probe is slow.
        let check = probe();
        tokio::pin!(check);
        let status = tokio::select! {
            biased;
            _ = control.canceled() => return Err(CANCELLED.into()),
            result = &mut install => return result.map(|_| Completion::Acknowledged),
            _ = &mut deadline => return Err("iPhone response wait expired after 12 hours.".into()),
            status = &mut check => status,
        };
        match status {
            Probe::Installed => return Ok(Completion::Verified),
            Probe::Disconnected(error) => {
                return Err(format!("iPhone disconnected while waiting: {error}"))
            }
            Probe::Connected => {}
        }
        tokio::select! {
            biased;
            _ = control.canceled() => return Err(CANCELLED.into()),
            result = &mut install => return result.map(|_| Completion::Acknowledged),
            _ = &mut deadline => return Err("iPhone response wait expired after 12 hours.".into()),
            _ = tokio::time::sleep(policy.poll) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::future::{pending, ready};

    fn policy() -> Policy {
        Policy {
            grace: Duration::from_secs(60),
            poll: Duration::from_secs(15),
            max_wait: Duration::from_secs(3600),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn immediate_success_never_enters_waiting() {
        let control = Control::default();
        let result = await_install(
            ready(Ok(())),
            || async { panic!("must not probe") },
            &control,
            policy(),
            || panic!("must not wait"),
        )
        .await;
        assert_eq!(result, Ok(Completion::Acknowledged));
        assert!(!control.is_waiting());
    }

    #[tokio::test(start_paused = true)]
    async fn original_request_resumes_after_grace_without_resubmitting() {
        let control = Control::default();
        let polls = Cell::new(0);
        let install = async {
            tokio::time::sleep(Duration::from_secs(95)).await;
            Ok(())
        };
        let result = await_install(
            install,
            || {
                assert!(control.is_waiting());
                polls.set(polls.get() + 1);
                ready(Probe::Connected)
            },
            &control,
            policy(),
            || assert!(control.is_waiting()),
        )
        .await;
        assert_eq!(result, Ok(Completion::Acknowledged));
        assert!(polls.get() >= 2);
        assert!(!control.is_waiting());
    }

    #[tokio::test(start_paused = true)]
    async fn installed_profile_verification_recovers_missing_ack() {
        let control = Control::default();
        let result = await_install(
            pending(),
            || ready(Probe::Installed),
            &control,
            policy(),
            || {},
        )
        .await;
        assert_eq!(result, Ok(Completion::Verified));
        assert!(!control.is_waiting());
    }

    #[tokio::test(start_paused = true)]
    async fn losing_connection_stops_the_wait() {
        let control = Control::default();
        let result = await_install(
            pending(),
            || ready(Probe::Disconnected("offline".into())),
            &control,
            policy(),
            || {},
        )
        .await;
        assert!(result.unwrap_err().contains("disconnected"));
        assert!(!control.is_waiting());
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_a_stalled_probe() {
        let control = Control::default();
        let wait = await_install(pending(), || pending::<Probe>(), &control, policy(), || {});
        let cancel = async {
            tokio::time::sleep(Duration::from_secs(61)).await;
            assert!(control.is_waiting());
            control.cancel();
        };
        let (result, ()) = tokio::join!(wait, cancel);
        assert_eq!(result, Err(CANCELLED.into()));
        assert!(!control.is_waiting());
    }

    #[tokio::test(start_paused = true)]
    async fn acknowledgment_is_processed_during_a_stalled_probe() {
        let control = Control::default();
        let install = async {
            tokio::time::sleep(Duration::from_secs(61)).await;
            Ok(())
        };
        let result = await_install(install, || pending::<Probe>(), &control, policy(), || {}).await;
        assert_eq!(result, Ok(Completion::Acknowledged));
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_install_error_does_not_enter_waiting() {
        let control = Control::default();
        let result = await_install(
            ready(Err("rejected".into())),
            || ready(Probe::Connected),
            &control,
            policy(),
            || panic!("must not wait"),
        )
        .await;
        assert_eq!(result, Err("rejected".into()));
    }

    #[tokio::test(start_paused = true)]
    async fn connected_but_unresponsive_wait_has_a_deadline() {
        let control = Control::default();
        let result = await_install(
            pending(),
            || ready(Probe::Connected),
            &control,
            policy(),
            || {},
        )
        .await;
        assert!(result.unwrap_err().contains("wait expired"));
        assert!(!control.is_waiting());
    }

    #[tokio::test(start_paused = true)]
    async fn canceled_control_can_be_reused_for_a_later_job() {
        let control = Control::default();
        control.cancel();
        control.reset();
        let install = async {
            tokio::time::sleep(Duration::from_secs(61)).await;
            Ok(())
        };
        assert_eq!(
            await_install(
                install,
                || ready(Probe::Connected),
                &control,
                policy(),
                || {}
            )
            .await,
            Ok(Completion::Acknowledged)
        );
    }
}

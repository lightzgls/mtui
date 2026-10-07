//! Durable history delivery, separate from playback and album/artist loading.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use super::journal::ReportQueue;
use super::stats::Reporter;
use super::worker::Request;
use crate::config::Cookies;

pub(super) fn run(rx: Receiver<Request>) {
    let Ok(mut reporter) = Reporter::new() else {
        return;
    };
    retry(&mut reporter);
    loop {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Request::RetryReports) | Err(RecvTimeoutError::Timeout) => retry(&mut reporter),
            // sign_out clears the durable file synchronously. A delayed worker
            // message must not delete plays collected after the next sign-in.
            Ok(Request::ClearReports) => reporter.clear_session(),
            Ok(Request::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(_) => debug_assert!(false, "non-history request routed to history worker"),
        }
    }
}

fn retry(reporter: &mut Reporter) {
    let mut reports = ReportQueue::load();
    if reports.front().is_none() {
        return;
    }
    // Bounded passes also give sign-out and new checkpoints a chance to run.
    for _ in 0..reports.len().min(8) {
        let Some(cookies) = Cookies::available().ok().flatten() else {
            return;
        };
        let Some(report) = reports.front().cloned() else {
            break;
        };
        match reporter.report(&cookies, &report) {
            Ok(()) => {
                if !reports.acknowledge_front() {
                    return;
                }
                crate::diagnostics::info(
                    "history",
                    "listening report confirmed in account history",
                );
            }
            Err(error) => {
                crate::diagnostics::error(
                    "history",
                    &format!("playback report retained: {error:#}"),
                );
                reports.defer_front();
            }
        }
    }
}

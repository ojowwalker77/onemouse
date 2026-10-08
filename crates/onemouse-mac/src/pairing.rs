//! Hands pairing requests from connection threads to the UI thread and the
//! user's answer back.

use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use onemouse_transport::PairingRequest;

/// One question at a time: a second request while one is pending is
/// declined, so an attacker can't stack dialogs next to the real one.
#[derive(Debug, Default)]
pub struct Prompts {
    pending: Mutex<Option<(PairingRequest, Sender<bool>)>>,
}

impl Prompts {
    /// Called on a connection thread: waits up to `timeout` for the user.
    pub fn ask(&self, request: &PairingRequest, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = self.lock();
            if pending.is_some() {
                return false;
            }
            *pending = Some((request.clone(), tx));
        }
        let answer = rx.recv_timeout(timeout).unwrap_or(false);
        // Unanswered: withdraw it so the UI doesn't show a stale dialog.
        self.lock().take_if(|(r, _)| r == request);
        answer
    }

    /// Called on the UI thread: the question to show, and where to answer.
    pub fn next(&self) -> Option<(PairingRequest, Sender<bool>)> {
        self.lock().take()
    }

    fn lock(&self) -> MutexGuard<'_, Option<(PairingRequest, Sender<bool>)>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn request(code: &str) -> PairingRequest {
        PairingRequest {
            peer_name: "pc".into(),
            peer_fingerprint: "ab:cd".into(),
            code: code.into(),
            deadline: std::time::Instant::now() + Duration::from_secs(60),
        }
    }

    #[test]
    fn answers_travel_back_to_the_asking_thread() {
        let prompts = Arc::new(Prompts::default());
        let asker = {
            let prompts = Arc::clone(&prompts);
            thread::spawn(move || prompts.ask(&request("123 456"), Duration::from_secs(5)))
        };
        let (req, answer) = loop {
            if let Some(next) = prompts.next() {
                break next;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(req.code, "123 456");
        answer.send(true).unwrap();
        assert!(asker.join().unwrap());
    }

    #[test]
    fn unanswered_requests_decline_and_disappear() {
        let prompts = Prompts::default();
        assert!(!prompts.ask(&request("1"), Duration::from_millis(20)));
        assert!(prompts.next().is_none());
    }

    #[test]
    fn a_second_request_is_declined_while_one_is_pending() {
        let prompts = Arc::new(Prompts::default());
        let first = {
            let prompts = Arc::clone(&prompts);
            thread::spawn(move || prompts.ask(&request("1"), Duration::from_millis(300)))
        };
        thread::sleep(Duration::from_millis(50));
        assert!(!prompts.ask(&request("2"), Duration::from_secs(5)));
        assert!(!first.join().unwrap());
    }
}

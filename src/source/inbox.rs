//! Coalesced read work: at most one pending request per operation, with cancellation.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, atomic::{AtomicU64, Ordering}};
use anyhow::{Result, anyhow};
use super::{http::Cancellation, worker::Request};

#[derive(Clone, Copy)]
pub enum Kind { Browse, Cover, Panels, Artwork }

pub struct Work {
    pub request: Request,
    pub cancel: Option<Cancellation>,
}

pub struct Inbox {
    kind: Kind,
    generation: Arc<AtomicU64>,
    pending: Mutex<VecDeque<Work>>,
    visible: Mutex<HashSet<String>>,
    wake: Condvar,
}

impl Inbox {
    pub fn new(kind: Kind) -> Arc<Self> {
        Arc::new(Self { kind, generation: Arc::new(AtomicU64::new(0)),
            pending: Mutex::new(VecDeque::new()), visible: Mutex::new(HashSet::new()), wake: Condvar::new() })
    }

    pub fn send(&self, request: Request) -> Result<()> {
        let mut pending = self.pending.lock().map_err(|_|anyhow!("read worker queue is unavailable"))?;
        let shutdown = matches!(request, Request::Shutdown);
        let newer = !matches!(self.kind, Kind::Panels | Kind::Artwork) || matches!(request, Request::Watch { .. }) || shutdown;
        if newer { self.generation.fetch_add(1, Ordering::SeqCst); }
        if shutdown { pending.clear(); }
        if let Request::Watch { video_id } = &request {
            pending.retain(|work| match &work.request {
                Request::Lyrics { video_id: old, .. } | Request::Related { video_id: old, .. }
                    | Request::Comments { video_id: old } => old == video_id,
                _ => true,
            });
        }
        if let Request::Art { key, .. } = &request {
            if !self.visible.lock().map_err(|_|anyhow!("artwork queue is unavailable"))?.contains(key) { return Ok(()); }
            pending.retain(|work| !matches!(&work.request, Request::Art { key: old, .. } if old == key));
        } else {
            let key = operation(&request);
            pending.retain(|work| operation(&work.request) != key);
        }
        let cancel = if matches!(request, Request::Art { .. } | Request::MoreQueue { .. } | Request::SeedQueue { .. } | Request::Shutdown) {
            None
        } else { Some(Cancellation::capture(self.generation.clone())) };
        let work = Work { request, cancel };
        if matches!(work.request, Request::Watch { .. }) { pending.push_front(work); }
        else { pending.push_back(work); }
        self.wake.notify_one();
        Ok(())
    }

    pub fn recv(&self) -> Option<Work> {
        let mut pending = self.pending.lock().ok()?;
        loop {
            if let Some(work) = self.pop(&mut pending) {
                if work.cancel.as_ref().is_some_and(|cancel|!cancel.active()) { continue; }
                return Some(work);
            }
            pending = self.wake.wait(pending).ok()?;
        }
    }

    pub fn try_recv(&self) -> Option<Work> {
        let mut pending = self.pending.lock().ok()?;
        self.pop(&mut pending)
    }

    fn pop(&self, pending: &mut VecDeque<Work>) -> Option<Work> {
        if matches!(self.kind, Kind::Artwork) { pending.pop_back() } else { pending.pop_front() }
    }

    pub fn visible_art(&self, keys: impl Iterator<Item=String>) {
        let Ok(mut pending) = self.pending.lock() else { return; };
        let Ok(mut visible) = self.visible.lock() else { return; };
        *visible = keys.take(crate::art::CAPACITY).collect();
        pending.retain(|work| !matches!(&work.request, Request::Art { key, .. } if !visible.contains(key)));
    }
}

fn operation(request: &Request) -> u8 {
    match request {
        Request::Watch { .. } => 1,
        Request::Lyrics { .. } => 2,
        Request::Related { .. } => 3,
        Request::Comments { .. } => 4,
        Request::MoreQueue { .. } | Request::SeedQueue { .. } => 5,
        Request::Shutdown => 255,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn newer_browsing_supersedes_pending_and_running_reads() {
        let inbox = Inbox::new(Kind::Browse);
        inbox.send(Request::Cover{id:"old".into()}).unwrap();
        let running = inbox.recv().unwrap();
        for n in 0..100 { inbox.send(Request::Cover{id:n.to_string()}).unwrap(); }
        assert!(!running.cancel.unwrap().active());
        assert_eq!(inbox.pending.lock().unwrap().len(),1);
        assert!(matches!(inbox.recv().unwrap().request,Request::Cover{id} if id=="99"));
    }
    #[test]
    fn track_changes_cancel_panels_without_losing_radio_continuations() {
        let inbox = Inbox::new(Kind::Panels);
        inbox.send(Request::Lyrics{video_id:"a".into(),browse_id:None,query:None}).unwrap();
        let running = inbox.recv().unwrap();
        inbox.send(Request::MoreQueue{epoch:1,token:"token".into()}).unwrap();
        inbox.send(Request::Comments{video_id:"a".into()}).unwrap();
        inbox.send(Request::Watch{video_id:"b".into()}).unwrap();
        assert!(!running.cancel.unwrap().active());
        assert!(matches!(inbox.recv().unwrap().request,Request::Watch{video_id} if video_id=="b"));
        assert!(matches!(inbox.recv().unwrap().request,Request::MoreQueue{epoch:1,..}));
    }

    #[test]
    fn artwork_backlogs_follow_the_visible_page_and_prioritize_new_tiles() {
        let inbox = Inbox::new(Kind::Artwork);
        for page in 0..100 {
            let keys: Vec<_> = (0..20).map(|n| format!("{page}-{n}")).collect();
            inbox.visible_art(keys.iter().cloned());
            for key in keys { inbox.send(Request::Art { key, url:"fixture".into() }).unwrap(); }
            assert_eq!(inbox.pending.lock().unwrap().len(), 20);
        }
        assert!(matches!(inbox.recv().unwrap().request, Request::Art{key,..} if key=="99-19"));
        inbox.send(Request::Shutdown).unwrap();
        assert!(matches!(inbox.recv().unwrap().request,Request::Shutdown));
    }
}

//! Visible-row icon extraction. The latest viewport replaces undispatched
//! work; one Shell call can finish in the background after it is superseded.

mod process;
mod shell;
mod wire;

use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{DestroyIcon, HICON, PostMessageW, WM_APP},
};

use crate::catalog::Source;

pub(super) const READY: u32 = WM_APP + 33;
const CACHE_LIMIT: usize = 48;
const HELPER_IDLE: Duration = Duration::from_millis(350);

pub(crate) fn run_worker() -> Result<(), String> {
    process::serve().map_err(|error| format!("Не удалось извлечь значки: {error}"))
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    source: Source,
    px: i32,
}

// HICONs are opaque process-local handles, valid on both the worker and the
// owner thread. Keep ownership singular across that transfer.
struct Icon(usize);

impl Icon {
    fn from_raw(handle: HICON) -> Option<Self> {
        (!handle.is_null()).then_some(Self(handle as usize))
    }

    fn raw(&self) -> HICON {
        self.0 as HICON
    }
}

impl Drop for Icon {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { DestroyIcon(self.raw()) };
        }
    }
}

type Entry = (Key, Option<Icon>);

#[derive(Default)]
struct Cache(VecDeque<Entry>);

impl Cache {
    fn touch(&mut self, key: &Key) -> bool {
        if let Some(index) = self.0.iter().position(|(cached, _)| cached == key) {
            let entry = self.0.remove(index).unwrap();
            self.0.push_back(entry);
            return true;
        }
        false
    }

    fn insert(&mut self, key: Key, icon: Option<Icon>) {
        if let Some(index) = self.0.iter().position(|(cached, _)| cached == &key) {
            self.0.remove(index);
        }
        self.0.push_back((key, icon));
        if self.0.len() > CACHE_LIMIT {
            self.0.pop_front();
        }
    }

    fn get(&self, source: &Source, px: i32) -> Option<&Icon> {
        self.0
            .iter()
            .find(|(key, _)| &key.source == source && key.px == px)
            .and_then(|(_, icon)| icon.as_ref())
    }
}

#[derive(Default)]
struct Work {
    generation: u64,
    series: u64,
    helper_handle: Option<usize>,
    pending: VecDeque<Key>,
    returned: Vec<Entry>,
    notified: bool,
    worker_running: bool,
    stop_when_idle: bool,
    closed: bool,
}

impl Work {
    fn replace(&mut self, pending: VecDeque<Key>) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = pending;
        self.returned.clear();
    }

    fn finish(&mut self, generation: u64, entry: Entry) -> bool {
        if self.closed || self.generation != generation {
            return false;
        }
        self.returned.push(entry);
        true
    }

    fn cancel_series(&mut self) {
        self.series = self.series.wrapping_add(1);
        self.replace(VecDeque::new());
        if let Some(handle) = self.helper_handle {
            process::cancel(handle);
        }
    }
}

#[derive(Default)]
struct Shared {
    work: Mutex<Work>,
    wake: Condvar,
}

pub(super) struct Loader {
    owner: usize,
    shared: Option<Arc<Shared>>,
    visible: Vec<Key>,
    cache: Cache,
}

impl Loader {
    pub(super) fn new(owner: HWND) -> Self {
        Self {
            owner: owner as usize,
            shared: None,
            visible: Vec::new(),
            cache: Cache::default(),
        }
    }

    pub(super) fn request<'a>(
        &mut self,
        sources: impl Iterator<Item = &'a Source> + Clone,
        px: i32,
    ) {
        // Repositioning the controls within the same viewport needs no new strings.
        if self.visible.iter().all(|key| key.px == px)
            && self
                .visible
                .iter()
                .map(|key| &key.source)
                .eq(sources.clone())
        {
            return;
        }
        let mut visible = Vec::new();
        if (1..=512).contains(&px) {
            for source in sources.take(CACHE_LIMIT) {
                let key = Key {
                    source: source.clone(),
                    px,
                };
                if !visible.contains(&key) {
                    visible.push(key);
                }
            }
        }
        if self.visible == visible {
            return;
        }
        let pending: VecDeque<_> = visible
            .iter()
            .filter(|key| !self.cache.touch(key))
            .cloned()
            .collect();
        self.visible = visible;

        if self.shared.is_none() && !pending.is_empty() {
            self.shared = Some(Arc::new(Shared::default()));
        }
        if let Some(shared) = &self.shared {
            let mut work = shared.work.lock().unwrap();
            work.replace(pending);
            if !work.pending.is_empty() {
                work.stop_when_idle = false;
                if !work.worker_running {
                    let worker = Arc::clone(shared);
                    let owner = self.owner;
                    work.worker_running = std::thread::Builder::new()
                        .name("rustart-icons".into())
                        .spawn(move || run(owner, worker))
                        .is_ok();
                }
            }
            shared.wake.notify_one();
        }
    }

    pub(super) fn collect(&mut self) -> bool {
        let Some(shared) = &self.shared else {
            return false;
        };
        let returned = {
            let mut work = shared.work.lock().unwrap();
            work.notified = false;
            std::mem::take(&mut work.returned)
        };
        let changed = !returned.is_empty();
        for (key, icon) in returned {
            self.cache.insert(key, icon);
        }
        changed
    }

    pub(super) fn get(&self, source: &Source, px: i32) -> HICON {
        self.cache
            .get(source, px)
            .map_or(std::ptr::null_mut(), Icon::raw)
    }

    pub(super) fn clear(&mut self) {
        self.request(std::iter::empty(), 0);
        self.cache.0.clear();
    }

    pub(super) fn release(&mut self) {
        if let Some(shared) = &self.shared {
            let mut work = shared.work.lock().unwrap();
            work.cancel_series();
            work.stop_when_idle = true;
            shared.wake.notify_one();
        }
        self.visible = Vec::new();
        self.cache = Cache::default();
    }

    fn close_worker(&mut self) {
        if let Some(shared) = self.shared.take() {
            let mut work = shared.work.lock().unwrap();
            work.closed = true;
            work.cancel_series();
            shared.wake.notify_one();
        }
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.close_worker();
    }
}

fn run(owner: usize, shared: Arc<Shared>) {
    let mut helper: Option<(u64, process::Helper)> = None;
    loop {
        let next = next_request(&shared, helper.as_ref().map(|(series, _)| *series));
        match next {
            Next::Retire => retire_helper(&shared, &mut helper),
            Next::Stop => {
                retire_helper(&shared, &mut helper);
                return;
            }
            Next::Icon {
                generation,
                series,
                key,
            } => {
                if helper.is_none()
                    && let Ok(started) = process::Helper::start()
                {
                    let mut work = shared.work.lock().unwrap();
                    if work.closed || work.series != series {
                        continue;
                    }
                    work.helper_handle = Some(started.handle());
                    helper = Some((series, started));
                }
                let result = helper.as_mut().map(|(_, helper)| helper.get(&key));
                let icon = match result {
                    Some(Ok(icon)) => icon,
                    Some(Err(_)) => {
                        retire_helper(&shared, &mut helper);
                        None
                    }
                    None => None,
                };
                let mut work = shared.work.lock().unwrap();
                if work.finish(generation, (key, icon)) && !work.notified {
                    work.notified = unsafe { PostMessageW(owner as HWND, READY, 0, 0) } != 0;
                }
            }
        }
    }
}

enum Next {
    Stop,
    Retire,
    Icon {
        generation: u64,
        series: u64,
        key: Key,
    },
}

fn next_request(shared: &Shared, active: Option<u64>) -> Next {
    let work = shared.work.lock().unwrap();
    let mut work = if let Some(series) = active {
        let (work, _) = shared
            .wake
            .wait_timeout_while(work, HELPER_IDLE, |work| {
                !work.closed && work.pending.is_empty() && work.series == series
            })
            .unwrap();
        work
    } else {
        shared
            .wake
            .wait_while(work, |work| {
                !work.closed && !work.stop_when_idle && work.pending.is_empty()
            })
            .unwrap()
    };
    if work.closed {
        Next::Stop
    } else if active.is_some_and(|series| series != work.series) {
        Next::Retire
    } else if let Some(key) = work.pending.pop_front() {
        Next::Icon {
            generation: work.generation,
            series: work.series,
            key,
        }
    } else if active.is_some() {
        Next::Retire
    } else {
        // There is no child left to reap. Publishing the stopped state under
        // the queue lock lets request() safely start the next coordinator.
        work.worker_running = false;
        Next::Stop
    }
}

fn retire_helper(shared: &Shared, helper: &mut Option<(u64, process::Helper)>) {
    if let Some(owned) = helper.take() {
        {
            let mut work = shared.work.lock().unwrap();
            work.helper_handle = None;
        }
        // The worker reaps before spawning again; callers never wait for I/O.
        drop(owned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(index: usize) -> Key {
        Key {
            source: Source::Packaged(index.to_string()),
            px: 34,
        }
    }

    #[test]
    fn cache_limits_entries_and_keeps_recent_misses() {
        let mut cache = Cache::default();
        for index in 0..CACHE_LIMIT {
            cache.insert(key(index), None);
        }
        assert!(cache.touch(&key(0)));
        cache.insert(key(CACHE_LIMIT), None);
        assert_eq!(cache.0.len(), CACHE_LIMIT);
        assert!(!cache.touch(&key(1)));
        assert!(cache.touch(&key(0)));
    }

    #[test]
    fn new_viewport_discards_pending_and_late_results() {
        let mut work = Work::default();
        work.replace([key(0), key(1)].into());
        let old = work.generation;
        assert!(work.finish(old, (key(0), None)));
        work.replace([key(2)].into());
        assert!(work.returned.is_empty());
        assert!(!work.finish(old, (key(1), None)));
        assert!(work.pending.pop_front() == Some(key(2)));
        assert!(work.pending.is_empty());
        assert!(work.finish(work.generation, (key(2), None)));
    }

    #[test]
    fn idle_release_cancels_the_series_and_discards_old_delivery() {
        let mut loader = Loader::new(std::ptr::null_mut());
        let retired = Arc::new(Shared::default());
        loader.shared = Some(Arc::clone(&retired));
        loader.visible.push(key(0));
        loader.cache.insert(key(0), None);
        let generation = {
            let mut work = retired.work.lock().unwrap();
            work.worker_running = true;
            work.replace([key(1)].into());
            let generation = work.generation;
            assert!(work.finish(generation, (key(0), None)));
            generation
        };

        loader.release();

        assert!(loader.shared.is_some());
        assert!(loader.visible.is_empty());
        assert!(loader.cache.0.is_empty());
        assert!(!loader.collect());
        let mut work = retired.work.lock().unwrap();
        assert!(!work.closed);
        assert!(work.pending.is_empty());
        assert!(work.returned.is_empty());
        assert!(!work.finish(generation, (key(1), None)));
        assert!(work.stop_when_idle);
        drop(work);
        assert!(matches!(next_request(&retired, None), Next::Stop));
        assert!(!retired.work.lock().unwrap().worker_running);
    }

    #[test]
    fn reopening_before_retirement_drains_old_series_before_new_work() {
        let shared = Shared::default();
        let old_series = {
            let mut work = shared.work.lock().unwrap();
            work.worker_running = true;
            let old_series = work.series;
            work.cancel_series();
            work.stop_when_idle = true;
            old_series
        };
        {
            let mut work = shared.work.lock().unwrap();
            // request() has resumed the queue before the old helper exited.
            work.replace([key(2)].into());
            work.stop_when_idle = false;
        }
        assert!(matches!(
            next_request(&shared, Some(old_series)),
            Next::Retire
        ));
        let Next::Icon {
            series, key: next, ..
        } = next_request(&shared, None)
        else {
            panic!("the resumed coordinator did not service the new viewport");
        };
        assert_ne!(series, old_series);
        assert!(next == key(2));
        assert!(shared.work.lock().unwrap().worker_running);
    }
}

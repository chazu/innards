//! Disabled profiling has no sample allocations and reads no clock.
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};
#[derive(Default, Serialize)]
pub struct Distribution {
    count: u64,
    total_us: u64,
    max_us: u64,
    buckets: [u64; 8],
}
impl Distribution {
    fn add(&mut self, us: u64) {
        self.count += 1;
        self.total_us += us;
        self.max_us = self.max_us.max(us);
        let bucket = [100, 500, 1000, 4000, 16667, 50000, 250000]
            .iter()
            .position(|v| us <= *v)
            .unwrap_or(7);
        self.buckets[bucket] += 1;
    }
}
#[derive(Serialize)]
pub struct Receipt {
    pub request_id: u64,
    pub ack_us: Option<u64>,
    pub rendered_update_us: Option<u64>,
    #[serde(skip)]
    start: Instant,
    #[serde(skip)]
    updated: bool,
}
#[derive(Default, Serialize)]
pub struct Profile {
    pub schema_version: u32,
    pub view: String,
    pub counters: BTreeMap<&'static str, u64>,
    pub receipts: VecDeque<Receipt>,
    pub sources: BTreeMap<String, u64>,
    pub timings: BTreeMap<&'static str, Distribution>,
    #[serde(skip)]
    pub enabled: bool,
}
impl Profile {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            schema_version: 1,
            ..Self::default()
        }
    }
    pub fn request(&mut self, id: u64) {
        if self.enabled {
            if self.receipts.len() == 32 {
                self.receipts.pop_front();
            }
            self.receipts.push_back(Receipt {
                request_id: id,
                ack_us: None,
                rendered_update_us: None,
                start: Instant::now(),
                updated: false,
            });
        }
    }
    pub fn ack(&mut self, id: u64) {
        if let Some(r) = self.receipts.iter_mut().find(|r| r.request_id == id) {
            r.ack_us = Some(r.start.elapsed().as_micros() as u64);
        }
    }
    pub fn updated(&mut self, id: u64) {
        if let Some(r) = self.receipts.iter_mut().find(|r| r.request_id == id) {
            r.updated = true;
        }
    }
    pub fn rendered(&mut self) {
        for r in &mut self.receipts {
            if r.updated && r.rendered_update_us.is_none() {
                r.rendered_update_us = Some(r.start.elapsed().as_micros() as u64);
            }
        }
    }
    pub fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }
    pub fn end(&mut self, name: &'static str, started: Option<Instant>) {
        if let Some(start) = started {
            self.timings
                .entry(name)
                .or_default()
                .add(start.elapsed().as_micros() as u64);
        }
    }
    pub fn count(&mut self, name: &'static str, n: usize) {
        if self.enabled {
            *self.counters.entry(name).or_default() += n as u64;
        }
    }
    pub fn gauge(&mut self, name: &'static str, n: usize) {
        if self.enabled {
            self.counters.insert(name, n as u64);
        }
    }
    pub fn peak(&mut self, name: &'static str, n: usize) {
        if self.enabled {
            let v = self.counters.entry(name).or_default();
            *v = (*v).max(n as u64);
        }
    }
}

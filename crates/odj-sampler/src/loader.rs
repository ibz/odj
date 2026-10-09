//! Background loading of pads: decode the file, check it is still the same audio,
//! cut the region. One worker handles the jobs in order, so at most one decoded
//! file is in memory at a time; consecutive jobs on the same file decode it once.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use odj_core::track::{Decoded, decode};

use crate::engine::{Sample, cut};
use crate::kit::PadSource;

pub struct Job {
    pub pad: usize,
    /// Tells a result apart from one for an assignment since replaced.
    pub generation: u64,
    pub source: PadSource,
}

#[derive(Debug, PartialEq)]
pub enum Failure {
    /// No file at the stored path.
    Missing,
    /// The file is there but holds different audio now.
    Changed,
    Error(String),
}

pub struct Done {
    pub pad: usize,
    pub generation: u64,
    pub result: Result<Sample, Failure>,
}

pub struct Loader {
    jobs: Sender<Job>,
    done: Receiver<Done>,
}

impl Loader {
    pub fn new(out_rate: u32) -> Self {
        let (jobs, job_rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        thread::spawn(move || work(job_rx, done_tx, out_rate));
        Self { jobs, done }
    }

    pub fn load(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    pub fn poll(&self) -> Option<Done> {
        self.done.try_recv().ok()
    }
}

fn work(jobs: Receiver<Job>, done: Sender<Done>, out_rate: u32) {
    let mut cache: Option<(PathBuf, Decoded)> = None;
    let mut next = None;
    loop {
        let job = match next.take() {
            Some(job) => job,
            None => {
                // Idle: don't sit on a whole decoded track.
                cache = None;
                match jobs.recv() {
                    Ok(job) => job,
                    Err(_) => return,
                }
            }
        };
        let result = run(&job.source, &mut cache, out_rate);
        if done.send(Done { pad: job.pad, generation: job.generation, result }).is_err() {
            return;
        }
        next = jobs.try_recv().ok();
    }
}

fn run(source: &PadSource, cache: &mut Option<(PathBuf, Decoded)>, out_rate: u32) -> Result<Sample, Failure> {
    if !source.path.is_file() {
        return Err(Failure::Missing);
    }
    if cache.as_ref().is_none_or(|(p, _)| *p != source.path) {
        *cache = None;
        let decoded = decode(&source.path).map_err(|e| Failure::Error(e.to_string()))?;
        *cache = Some((source.path.clone(), decoded));
    }
    let (_, d) = cache.as_ref().expect("decoded above");
    if d.id != source.track_id || d.sample_rate != source.sample_rate {
        return Err(Failure::Changed);
    }
    Ok(cut(&d.samples, d.sample_rate, source.start, source.end, out_rate))
}

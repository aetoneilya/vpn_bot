//! Round-trip latency between the exit node (where the bot runs) and the relay.
//!
//! Every minute the bot opens a TCP connection to the relay and records the handshake
//! time — one network round trip over the relay ↔ exit path clients use. Failed
//! attempts are stored as gaps so the chart shows outages.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;
use tokio::net::TcpStream;

use crate::state::AppState;

const SAMPLE_EVERY: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const ATTEMPTS: usize = 3;
const RETENTION_SECS: i64 = 8 * 24 * 3600;

#[derive(Debug, Clone, Copy)]
pub enum Range {
    Hour,
    Day,
    Week,
}

impl Range {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("1h") => Self::Hour,
            Some("7d") => Self::Week,
            _ => Self::Day,
        }
    }

    fn span_secs(self) -> i64 {
        match self {
            Self::Hour => 3600,
            Self::Day => 24 * 3600,
            Self::Week => 7 * 24 * 3600,
        }
    }

    fn bucket_secs(self) -> i64 {
        match self {
            Self::Hour => 60,
            Self::Day => 5 * 60,
            Self::Week => 3600,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Point {
    /// Bucket start, unix seconds.
    pub t: i64,
    /// Average RTT in the bucket; `None` when every sample in it failed.
    pub ms: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Series {
    pub bucket_secs: i64,
    pub points: Vec<Point>,
    pub current_ms: Option<f64>,
    pub min_ms: Option<f64>,
    pub avg_ms: Option<f64>,
    pub max_ms: Option<f64>,
    /// Share of failed samples in the range, percent.
    pub loss_pct: f64,
}

pub async fn sampler(state: Arc<AppState>, relay: SocketAddr) {
    log::info!("latency sampler started relay={relay}");
    let mut interval = tokio::time::interval(SAMPLE_EVERY);
    let mut last_prune = 0;
    loop {
        interval.tick().await;
        let now = chrono::Utc::now().timestamp();
        let rtt = measure(relay).await;
        if let Err(err) = state.store.record_latency(now, rtt) {
            log::warn!("failed to record latency: {err:#}");
        }
        if now - last_prune > 3600 {
            last_prune = now;
            if let Err(err) = state.store.prune_latency(now - RETENTION_SECS) {
                log::warn!("failed to prune latency: {err:#}");
            }
        }
    }
}

/// Best of a few TCP handshakes, in milliseconds; `None` if all of them failed.
async fn measure(relay: SocketAddr) -> Option<f64> {
    let mut best: Option<f64> = None;
    for _ in 0..ATTEMPTS {
        let started = Instant::now();
        if let Ok(Ok(_stream)) =
            tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(relay)).await
        {
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            best = Some(best.map_or(ms, |b: f64| b.min(ms)));
        }
    }
    best
}

pub fn series(state: &AppState, range: Range) -> Result<Series> {
    let now = chrono::Utc::now().timestamp();
    let samples = state.store.latency_since(now - range.span_secs())?;
    Ok(aggregate(&samples, range.bucket_secs()))
}

fn aggregate(samples: &[(i64, Option<f64>)], bucket_secs: i64) -> Series {
    let mut points: Vec<Point> = Vec::new();
    let mut bucket: Option<(i64, f64, usize, usize)> = None; // (start, sum, ok, total)
    let flush = |points: &mut Vec<Point>, (start, sum, ok, _total): (i64, f64, usize, usize)| {
        points.push(Point {
            t: start,
            ms: (ok > 0).then(|| round1(sum / ok as f64)),
        });
    };

    for &(ts, rtt) in samples {
        let start = ts - ts.rem_euclid(bucket_secs);
        match bucket {
            Some(b) if b.0 == start => {}
            Some(b) => {
                flush(&mut points, b);
                bucket = Some((start, 0.0, 0, 0));
            }
            None => bucket = Some((start, 0.0, 0, 0)),
        }
        let b = bucket.as_mut().expect("bucket initialized above");
        b.3 += 1;
        if let Some(ms) = rtt {
            b.1 += ms;
            b.2 += 1;
        }
    }
    if let Some(b) = bucket {
        flush(&mut points, b);
    }

    let ok: Vec<f64> = samples.iter().filter_map(|(_, rtt)| *rtt).collect();
    let failed = samples.len() - ok.len();
    Series {
        bucket_secs,
        points,
        current_ms: samples.last().and_then(|(_, rtt)| rtt.map(round1)),
        min_ms: ok.iter().copied().reduce(f64::min).map(round1),
        avg_ms: (!ok.is_empty()).then(|| round1(ok.iter().sum::<f64>() / ok.len() as f64)),
        max_ms: ok.iter().copied().reduce(f64::max).map(round1),
        loss_pct: if samples.is_empty() {
            0.0
        } else {
            round1(failed as f64 * 100.0 / samples.len() as f64)
        },
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_average_and_mark_outages() {
        let samples = [
            (0, Some(10.0)),
            (30, Some(20.0)),
            (60, None),
            (90, None),
            (120, Some(40.0)),
        ];
        let s = aggregate(&samples, 60);
        let values: Vec<_> = s.points.iter().map(|p| (p.t, p.ms)).collect();
        assert_eq!(values, [(0, Some(15.0)), (60, None), (120, Some(40.0))]);
        assert_eq!(s.current_ms, Some(40.0));
        assert_eq!((s.min_ms, s.max_ms), (Some(10.0), Some(40.0)));
        assert_eq!(s.loss_pct, 40.0);
    }

    #[test]
    fn empty_range() {
        let s = aggregate(&[], 60);
        assert!(s.points.is_empty());
        assert_eq!((s.current_ms, s.avg_ms, s.loss_pct), (None, None, 0.0));
    }
}

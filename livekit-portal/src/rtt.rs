use std::sync::Arc;
use std::time::Duration;

use livekit::prelude::*;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::metrics::MetricsRegistry;
use crate::video::now_us;

pub(crate) const RTT_TOPIC: &str = "portal_rtt";
const PING_KIND: u8 = 0;
const PONG_KIND: u8 = 1;
/// Bound on the in-flight RTT packet queue. Pings are 1Hz by default and
/// pongs are echoed 1:1, so a healthy peer never queues more than a handful.
/// The cap is a backstop; on overflow we drop without warn since RTT is
/// best-effort and a noisy log adds nothing.
const RTT_QUEUE_CAP: usize = 64;

/// Handles both outbound pings (on a timer) and outbound pongs (echoes of
/// received pings), serialized through a single bounded channel so the
/// receive handler can enqueue a pong without awaiting.
pub(crate) struct RttService {
    tx: mpsc::Sender<Vec<u8>>,
    ping_task: Option<JoinHandle<()>>,
    send_task: Option<JoinHandle<()>>,
    metrics: Arc<MetricsRegistry>,
}

impl RttService {
    pub fn spawn(
        local_participant: LocalParticipant,
        ping_interval_ms: u64,
        metrics: Arc<MetricsRegistry>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(RTT_QUEUE_CAP);

        let lp_send = local_participant;
        let send_task = tokio::spawn(async move {
            while let Some(payload) = rx.recv().await {
                let packet = DataPacket {
                    payload,
                    topic: Some(RTT_TOPIC.to_string()),
                    reliable: false, // retransmits would inflate RTT
                    destination_identities: Vec::new(),
                };
                if let Err(e) = lp_send.publish_data(packet).await {
                    log::warn!("[publish-failed] rtt publish failed: {e}");
                }
            }
        });

        let ping_task = if ping_interval_ms > 0 {
            let tx_ping = tx.clone();
            let metrics_ping = metrics.clone();
            Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(ping_interval_ms));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    let ts = now_us();
                    let mut payload = Vec::with_capacity(9);
                    payload.push(PING_KIND);
                    payload.extend_from_slice(&ts.to_le_bytes());
                    match tx_ping.try_send(payload) {
                        Ok(()) => metrics_ping.record_ping_sent(),
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            // Send loop is backed up; skip this ping.
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
            }))
        } else {
            None
        };

        Self { tx, ping_task, send_task: Some(send_task), metrics }
    }

    /// Handle an inbound packet on `RTT_TOPIC`. Kind byte differentiates ping
    /// (echo back as pong) from pong (record RTT against send timestamp).
    pub fn handle_packet(&self, payload: &[u8]) {
        if payload.len() != 9 {
            return;
        }
        let kind = payload[0];
        let send_ts = u64::from_le_bytes(payload[1..9].try_into().unwrap());
        match kind {
            PING_KIND => {
                let mut pong = Vec::with_capacity(9);
                pong.push(PONG_KIND);
                pong.extend_from_slice(&send_ts.to_le_bytes());
                // Drop on full / closed: pong loss only inflates RTT for one
                // sample and the peer will retry on its next ping.
                let _ = self.tx.try_send(pong);
            }
            PONG_KIND => {
                let rtt = now_us().saturating_sub(send_ts);
                self.metrics.record_rtt(rtt);
            }
            _ => {}
        }
    }
}

impl Drop for RttService {
    fn drop(&mut self) {
        if let Some(t) = self.ping_task.take() {
            t.abort();
        }
        if let Some(t) = self.send_task.take() {
            t.abort();
        }
    }
}

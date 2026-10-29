// Copyright 2026 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Who is in the session: role discovery from each peer's presence, the
//! protocol-version check, and the active-operator pointer.

use std::collections::HashSet;

use crate::transport::{PROTOCOL_VERSION, PeerId, Presence};
use crate::types::Role;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterEvent {
    OperatorJoined(PeerId),
    OperatorLeft(PeerId),
    ActiveOperatorChanged(Option<PeerId>),
    RobotLeft,
}

/// Rosters and the active-operator pointer.
#[derive(Debug, Default)]
pub struct Roster {
    /// On the robot: the source of truth, announced in its own presence.
    /// Elsewhere: a mirror of the robot's.
    pub active_operator: Option<PeerId>,
    /// Connected operators (excluding self).
    pub operators: HashSet<PeerId>,
    /// Connected observers (excluding self). Kept apart from `operators`
    /// so that list only holds peers that can drive.
    pub observers: HashSet<PeerId>,
    pub robot: Option<PeerId>,
    /// Peers already warned about for a protocol version mismatch, so each
    /// one is logged once rather than on every presence update.
    version_mismatch_warned: HashSet<PeerId>,
}

pub struct PresenceUpdate<'a> {
    pub self_role: Role,
    pub peer: &'a str,
    pub presence: &'a Presence,
}

impl Roster {
    /// Whether `id` is a recognised Portal peer of any role.
    pub fn is_known_peer(&self, id: &str) -> bool {
        self.robot.as_deref() == Some(id)
            || self.operators.contains(id)
            || self.observers.contains(id)
    }

    /// Whether `id` may steer the active operator.
    pub fn may_steer(&self, id: &str) -> bool {
        self.operators.contains(id) || self.observers.contains(id)
    }

    pub fn clear(&mut self) {
        self.active_operator = None;
        self.clear_for_reconnect();
    }

    /// Partial clear when the link is restored. Drops the rosters so the
    /// presence that follows rebuilds them, but keeps `active_operator`
    /// pinned:
    ///
    /// * **Robot side.** The pointer is the source of truth, and no
    ///   transport reports a peer's own presence back to it, so clearing it
    ///   would halt control across a transient outage.
    /// * **Elsewhere.** The mirror is reseeded by the robot's next
    ///   presence, and `update` only reports a change on a different value,
    ///   so keeping it avoids a spurious callback.
    pub fn clear_for_reconnect(&mut self) {
        self.operators.clear();
        self.observers.clear();
        self.robot = None;
    }

    /// Classify a peer by its presence and reconcile the rosters.
    /// Idempotent: re-observing the same presence reports nothing new.
    pub fn update(&mut self, update: PresenceUpdate<'_>) -> Option<RosterEvent> {
        let PresenceUpdate { self_role, peer, presence } = update;
        match presence.compatible_role() {
            Some(Role::Robot) => {
                if self.robot.as_deref() != Some(peer) {
                    self.robot = Some(peer.to_string());
                }
                // Operators and observers mirror the robot's pointer.
                if self_role == Role::Robot || self.active_operator == presence.active_operator {
                    return None;
                }
                self.active_operator = presence.active_operator.clone();
                Some(RosterEvent::ActiveOperatorChanged(presence.active_operator.clone()))
            }
            Some(Role::Operator) => self
                .operators
                .insert(peer.to_string())
                .then(|| RosterEvent::OperatorJoined(peer.to_string())),
            Some(Role::Observer) => {
                self.observers.insert(peer.to_string());
                None
            }
            None => {
                // Either the role isn't visible yet (a follow-up presence
                // brings it) or the peer speaks another protocol version and
                // stays out of the rosters.
                if presence.role.is_some()
                    && !presence.is_compatible()
                    && self.version_mismatch_warned.insert(peer.to_string())
                {
                    log::warn!(
                        "[version-mismatch] ignoring '{peer}': it speaks Portal protocol {:?}, this \
                         peer speaks {PROTOCOL_VERSION:?}; upgrade both sides to the same release",
                        presence.protocol_version.as_deref().unwrap_or("<none>"),
                    );
                }
                None
            }
        }
    }

    /// A peer left. The pointer stays pinned by design: the same identity
    /// rejoining resumes control, a different operator claims explicitly
    /// via `set_active_operator`.
    pub fn remove(&mut self, peer: &str) -> Vec<RosterEvent> {
        let mut events = Vec::new();
        if self.operators.remove(peer) {
            events.push(RosterEvent::OperatorLeft(peer.to_string()));
        }
        self.observers.remove(peer);
        if self.robot.as_deref() == Some(peer) {
            self.robot = None;
            events.push(RosterEvent::RobotLeft);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence(role: Role, version: Option<&str>) -> Presence {
        Presence {
            role: Some(role),
            protocol_version: version.map(String::from),
            active_operator: None,
        }
    }

    fn update<'a>(self_role: Role, peer: &'a str, presence: &'a Presence) -> PresenceUpdate<'a> {
        PresenceUpdate { self_role, peer, presence }
    }

    #[test]
    fn matching_version_is_classified() {
        let robot = presence(Role::Robot, Some(PROTOCOL_VERSION));
        assert_eq!(robot.compatible_role(), Some(Role::Robot));
        let operator = presence(Role::Operator, Some(PROTOCOL_VERSION));
        assert_eq!(operator.compatible_role(), Some(Role::Operator));
    }

    #[test]
    fn missing_or_other_version_is_ignored() {
        assert_eq!(presence(Role::Operator, None).compatible_role(), None);
        assert_eq!(presence(Role::Operator, Some("2")).compatible_role(), None);
        assert_eq!(presence(Role::Robot, Some("")).compatible_role(), None);
        assert_eq!(Presence::default().compatible_role(), None);
    }

    #[test]
    fn local_presence_classifies_as_its_role() {
        for role in [Role::Robot, Role::Operator, Role::Observer] {
            assert_eq!(Presence::local(role).compatible_role(), Some(role));
        }
    }

    #[test]
    fn mismatched_peer_stays_out_of_rosters() {
        let mut roster = Roster::default();
        roster.update(update(Role::Robot, "old-operator", &presence(Role::Operator, None)));
        roster.update(update(Role::Robot, "old-operator", &presence(Role::Operator, Some("2"))));
        assert!(roster.operators.is_empty());
        assert!(roster.version_mismatch_warned.contains("old-operator"));

        roster.update(update(Role::Operator, "old-robot", &presence(Role::Robot, Some("2"))));
        assert!(roster.robot.is_none());
    }

    #[test]
    fn observers_have_their_own_roster() {
        let mut roster = Roster::default();
        let observer = Presence::local(Role::Observer);
        let operator = Presence::local(Role::Operator);
        let joined: Vec<_> = [
            roster.update(update(Role::Robot, "recorder", &observer)),
            roster.update(update(Role::Robot, "teleop", &operator)),
        ]
        .into_iter()
        .flatten()
        .collect();

        assert!(roster.observers.contains("recorder"));
        assert!(!roster.operators.contains("recorder"));
        assert_eq!(
            joined,
            vec![RosterEvent::OperatorJoined("teleop".into())],
            "observers are not operators"
        );
    }

    #[test]
    fn observers_mirror_the_active_operator() {
        let mut roster = Roster::default();
        let robot =
            Presence { active_operator: Some("teleop".into()), ..Presence::local(Role::Robot) };
        let event = roster.update(update(Role::Observer, "robot", &robot));
        assert_eq!(roster.active_operator.as_deref(), Some("teleop"));
        assert_eq!(event, Some(RosterEvent::ActiveOperatorChanged(Some("teleop".into()))));
        assert_eq!(roster.update(update(Role::Observer, "robot", &robot)), None);
    }

    #[test]
    fn rejoining_operator_is_reported_once() {
        let mut roster = Roster::default();
        let op = Presence::local(Role::Operator);
        assert!(roster.update(update(Role::Robot, "teleop", &op)).is_some());
        assert!(roster.update(update(Role::Robot, "teleop", &op)).is_none());
        assert_eq!(roster.remove("teleop"), vec![RosterEvent::OperatorLeft("teleop".into())]);
        assert!(roster.remove("teleop").is_empty());
    }
}

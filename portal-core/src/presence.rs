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

//! Who is in the room: role discovery from participant attributes, the
//! protocol-version check, and the active-operator pointer.

use std::collections::{HashMap, HashSet};

use crate::types::Role;

/// Participant attribute keys used by Portal for role discovery and the
/// multi-controller pointer. Namespaced to avoid colliding with any
/// application-level attributes the user may also be setting.
pub const ROLE_ATTR_KEY: &str = "lk.portal.role";
pub const ACTIVE_OPERATOR_ATTR_KEY: &str = "lk.portal.active_operator";
/// Wire protocol version every peer publishes. Peers only recognise each
/// other when it matches, so a mixed-version room fails loudly instead of
/// exchanging packets neither side can read.
pub const PROTOCOL_VERSION_ATTR_KEY: &str = "lk.portal.version";
pub const PROTOCOL_VERSION: &str = "3";
const ROLE_VALUE_ROBOT: &str = "robot";
const ROLE_VALUE_OPERATOR: &str = "operator";
const ROLE_VALUE_OBSERVER: &str = "observer";

/// Attributes a peer sets on itself at connect so others can discover it.
pub fn role_attributes(role: Role) -> HashMap<String, String> {
    let value = match role {
        Role::Robot => ROLE_VALUE_ROBOT,
        Role::Operator => ROLE_VALUE_OPERATOR,
        Role::Observer => ROLE_VALUE_OBSERVER,
    };
    HashMap::from([
        (ROLE_ATTR_KEY.to_string(), value.to_string()),
        (PROTOCOL_VERSION_ATTR_KEY.to_string(), PROTOCOL_VERSION.to_string()),
    ])
}

/// Classify a participant by their `lk.portal.role` attribute. Returns
/// `None` if the attribute is absent, has an unknown value, or the
/// participant speaks a different protocol version.
pub fn classify_role(attrs: &HashMap<String, String>) -> Option<Role> {
    let role = match attrs.get(ROLE_ATTR_KEY).map(String::as_str) {
        Some(ROLE_VALUE_ROBOT) => Role::Robot,
        Some(ROLE_VALUE_OPERATOR) => Role::Operator,
        Some(ROLE_VALUE_OBSERVER) => Role::Observer,
        _ => return None,
    };
    is_compatible(attrs).then_some(role)
}

fn is_compatible(attrs: &HashMap<String, String>) -> bool {
    attrs.get(PROTOCOL_VERSION_ATTR_KEY).map(String::as_str) == Some(PROTOCOL_VERSION)
}

/// The active operator named by an attribute map, `None` when unset or
/// cleared (the empty string).
pub fn active_operator_attr(attrs: &HashMap<String, String>) -> Option<String> {
    attrs.get(ACTIVE_OPERATOR_ATTR_KEY).filter(|v| !v.is_empty()).cloned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceEvent {
    OperatorJoined(String),
    OperatorLeft(String),
    ActiveOperatorChanged(Option<String>),
    RobotLeft,
}

/// Rosters and the active-operator pointer.
#[derive(Debug, Default)]
pub struct Presence {
    /// On Robot side: source of truth, mirrored as own `lk.portal.active_operator`
    /// attribute. On Operator side: a mirror of the robot's attribute.
    pub active_operator: Option<String>,
    /// Currently-connected operators (excluding self).
    pub operators: HashSet<String>,
    /// Currently-connected observers (excluding self). Kept apart from
    /// `operators` so that list only holds peers that can drive.
    pub observers: HashSet<String>,
    /// The robot's identity, discovered from its `lk.portal.role`.
    pub robot_identity: Option<String>,
    /// Identities already warned about for a protocol version mismatch, so
    /// each one is logged once rather than on every attribute change.
    version_mismatch_warned: HashSet<String>,
}

impl Presence {
    /// Whether `id` is a recognised Portal peer of any role.
    pub fn is_known_peer(&self, id: &str) -> bool {
        self.robot_identity.as_deref() == Some(id)
            || self.operators.contains(id)
            || self.observers.contains(id)
    }

    /// Whether `id` may steer the active operator.
    pub fn may_steer(&self, id: &str) -> bool {
        self.operators.contains(id) || self.observers.contains(id)
    }

    pub fn clear(&mut self) {
        self.active_operator = None;
        self.operators.clear();
        self.observers.clear();
        self.robot_identity = None;
    }

    /// Partial clear on reconnect. Drops the per-room rosters so the
    /// post-reconnect participant events rebuild them, but keeps
    /// `active_operator` pinned:
    ///
    /// * **Robot side.** The pointer is the source of truth. The robot
    ///   ignores attribute events on its own identity and LiveKit never
    ///   reports self as connected, so clearing it would halt control
    ///   across a transient reconnect.
    /// * **Operator side.** The mirror is reseeded by the robot's next
    ///   participant event, and `classify_and_update` only reports a change
    ///   on a different value, so keeping it avoids a spurious callback.
    pub fn clear_for_reconnect(&mut self) {
        self.operators.clear();
        self.observers.clear();
        self.robot_identity = None;
    }

    /// Classify a remote participant and reconcile the rosters. Idempotent:
    /// re-observing the same participant reports nothing new.
    pub fn classify_and_update(&mut self, update: ParticipantUpdate<'_>) -> Option<PresenceEvent> {
        let ParticipantUpdate { self_role, identity, attributes } = update;
        match classify_role(attributes) {
            Some(Role::Robot) => {
                if self.robot_identity.as_deref() != Some(identity) {
                    self.robot_identity = Some(identity.to_string());
                }
                // Operators and observers mirror the robot's `active_operator`.
                if self_role == Role::Robot {
                    return None;
                }
                let new_value = active_operator_attr(attributes);
                if self.active_operator == new_value {
                    return None;
                }
                self.active_operator = new_value.clone();
                Some(PresenceEvent::ActiveOperatorChanged(new_value))
            }
            Some(Role::Operator) => self
                .operators
                .insert(identity.to_string())
                .then(|| PresenceEvent::OperatorJoined(identity.to_string())),
            Some(Role::Observer) => {
                self.observers.insert(identity.to_string());
                None
            }
            None => {
                // Either the role attribute isn't visible yet (a follow-up
                // attributes event will bring it) or the peer speaks another
                // protocol version and stays out of the rosters.
                if attributes.contains_key(ROLE_ATTR_KEY)
                    && !is_compatible(attributes)
                    && self.version_mismatch_warned.insert(identity.to_string())
                {
                    log::warn!(
                        "[version-mismatch] ignoring '{identity}': it speaks Portal protocol {:?}, this \
                         peer speaks {PROTOCOL_VERSION:?}; upgrade both sides to the same release",
                        attributes
                            .get(PROTOCOL_VERSION_ATTR_KEY)
                            .map(String::as_str)
                            .unwrap_or("<none>"),
                    );
                }
                None
            }
        }
    }

    /// A participant left. The `active_operator` pointer stays pinned by
    /// design: a same-identity reconnect resumes control, a different
    /// operator claims explicitly via `set_active_operator`.
    pub fn disconnect(&mut self, identity: &str) -> Vec<PresenceEvent> {
        let mut events = Vec::new();
        if self.operators.remove(identity) {
            events.push(PresenceEvent::OperatorLeft(identity.to_string()));
        }
        self.observers.remove(identity);
        if self.robot_identity.as_deref() == Some(identity) {
            self.robot_identity = None;
            events.push(PresenceEvent::RobotLeft);
        }
        events
    }
}

pub struct ParticipantUpdate<'a> {
    pub self_role: Role,
    pub identity: &'a str,
    pub attributes: &'a HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(role: &str, version: Option<&str>) -> HashMap<String, String> {
        let mut attrs = HashMap::from([(ROLE_ATTR_KEY.to_string(), role.to_string())]);
        if let Some(v) = version {
            attrs.insert(PROTOCOL_VERSION_ATTR_KEY.to_string(), v.to_string());
        }
        attrs
    }

    fn update<'a>(
        self_role: Role,
        identity: &'a str,
        attributes: &'a HashMap<String, String>,
    ) -> ParticipantUpdate<'a> {
        ParticipantUpdate { self_role, identity, attributes }
    }

    #[test]
    fn matching_version_is_classified() {
        assert_eq!(classify_role(&attrs("robot", Some(PROTOCOL_VERSION))), Some(Role::Robot));
        assert_eq!(classify_role(&attrs("operator", Some(PROTOCOL_VERSION))), Some(Role::Operator));
    }

    #[test]
    fn missing_or_other_version_is_ignored() {
        assert_eq!(classify_role(&attrs("operator", None)), None);
        assert_eq!(classify_role(&attrs("operator", Some("2"))), None);
        assert_eq!(classify_role(&attrs("robot", Some(""))), None);
        assert_eq!(classify_role(&HashMap::new()), None);
    }

    #[test]
    fn role_attributes_classify_as_their_role() {
        for role in [Role::Robot, Role::Operator, Role::Observer] {
            assert_eq!(classify_role(&role_attributes(role)), Some(role));
        }
    }

    #[test]
    fn mismatched_peer_stays_out_of_rosters() {
        let mut presence = Presence::default();
        presence.classify_and_update(update(Role::Robot, "old-operator", &attrs("operator", None)));
        presence.classify_and_update(update(
            Role::Robot,
            "old-operator",
            &attrs("operator", Some("2")),
        ));
        assert!(presence.operators.is_empty());
        assert!(presence.version_mismatch_warned.contains("old-operator"));

        presence.classify_and_update(update(
            Role::Operator,
            "old-robot",
            &attrs("robot", Some("2")),
        ));
        assert!(presence.robot_identity.is_none());
    }

    #[test]
    fn observers_have_their_own_roster() {
        assert_eq!(classify_role(&attrs("observer", Some(PROTOCOL_VERSION))), Some(Role::Observer));

        let mut presence = Presence::default();
        let observer = attrs("observer", Some(PROTOCOL_VERSION));
        let operator = attrs("operator", Some(PROTOCOL_VERSION));
        let joined: Vec<_> = [
            presence.classify_and_update(update(Role::Robot, "recorder", &observer)),
            presence.classify_and_update(update(Role::Robot, "teleop", &operator)),
        ]
        .into_iter()
        .flatten()
        .collect();

        assert!(presence.observers.contains("recorder"));
        assert!(!presence.operators.contains("recorder"));
        assert_eq!(
            joined,
            vec![PresenceEvent::OperatorJoined("teleop".into())],
            "observers are not operators"
        );
    }

    #[test]
    fn observers_mirror_the_active_operator() {
        let mut presence = Presence::default();
        let mut robot_attrs = attrs("robot", Some(PROTOCOL_VERSION));
        robot_attrs.insert(ACTIVE_OPERATOR_ATTR_KEY.to_string(), "teleop".to_string());
        let event = presence.classify_and_update(update(Role::Observer, "robot", &robot_attrs));
        assert_eq!(presence.active_operator.as_deref(), Some("teleop"));
        assert_eq!(event, Some(PresenceEvent::ActiveOperatorChanged(Some("teleop".into()))));
        assert_eq!(
            presence.classify_and_update(update(Role::Observer, "robot", &robot_attrs)),
            None
        );
    }

    #[test]
    fn rejoining_operator_is_reported_once() {
        let mut presence = Presence::default();
        let op = attrs("operator", Some(PROTOCOL_VERSION));
        assert!(presence.classify_and_update(update(Role::Robot, "teleop", &op)).is_some());
        assert!(presence.classify_and_update(update(Role::Robot, "teleop", &op)).is_none());
        assert_eq!(
            presence.disconnect("teleop"),
            vec![PresenceEvent::OperatorLeft("teleop".into())]
        );
        assert!(presence.disconnect("teleop").is_empty());
    }
}

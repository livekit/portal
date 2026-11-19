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

import type { Codec, DType, Role } from './types';

export interface FieldSpec {
  name: string;
  dtype: DType;
}

export type StallBehavior = 'drop' | 'stale' | 'omit';

export interface VideoSpec {
  name: string;
  codec: Codec;
  /** MJPEG only, 1..=100. */
  quality?: number;
  max_bitrate_kbps?: number;
  simulcast?: boolean;
  screencast?: boolean;
  width?: number;
  height?: number;
  stall_behavior?: StallBehavior;
  max_lag_ms?: number;
}

/**
 * The session's wire contract: the same fields, spelled the same way, as
 * the `portal.yaml` every role loads (see docs/reference). Every peer must
 * declare the same tracks and schemas.
 */
export interface PortalSpec {
  version?: 1;
  fps?: number;
  slack?: number;
  tolerance?: number;
  state_reliable?: boolean;
  action_reliable?: boolean;
  action_max_age_ms?: number;
  stall_behavior?: StallBehavior;
  max_lag_ms?: number;
  time_sync_source?: 'portal' | 'system';
  observation_sync?: boolean;
  action_subscription?: 'none' | 'active' | 'all';
  videos?: VideoSpec[];
  state?: FieldSpec[];
  action?: FieldSpec[];
  telemetry?: FieldSpec[];
}

/**
 * A role's config: the session (LiveKit room) plus the wire contract.
 * Validated by the core when the portal connects.
 */
export class PortalConfig {
  /** The contract as YAML. A spec object is serialized to JSON, which is YAML. */
  readonly yaml: string;
  e2eeKey?: Uint8Array;
  /** Test hook: shifts this peer's raw clock. */
  clockSkewUs = 0;

  protected constructor(
    readonly session: string,
    readonly role: Role,
    contract: Contract,
  ) {
    this.yaml =
      typeof contract === 'string' ? contract : JSON.stringify({ version: 1, ...contract });
  }

  /** Encrypt every stream end to end with a key shared by all peers. */
  setE2eeKey(key: Uint8Array | string): void {
    this.e2eeKey = typeof key === 'string' ? new TextEncoder().encode(key) : key;
  }
}

/** Spec object or YAML text. */
export type Contract = PortalSpec | string;

export class RobotConfig extends PortalConfig {
  constructor(session: string, contract: Contract = {}) {
    super(session, 'robot', contract);
  }

  static fromYaml(yaml: string, session: string): RobotConfig {
    return new RobotConfig(session, yaml);
  }
}

export class OperatorConfig extends PortalConfig {
  constructor(session: string, contract: Contract = {}) {
    super(session, 'operator', contract);
  }

  static fromYaml(yaml: string, session: string): OperatorConfig {
    return new OperatorConfig(session, yaml);
  }
}

/**
 * Defaults to `action_subscription: active` and observation sync off: an
 * observer records raw streams and the actions the robot executes.
 */
export class ObserverConfig extends PortalConfig {
  constructor(session: string, contract: Contract = {}) {
    super(session, 'observer', contract);
  }

  static fromYaml(yaml: string, session: string): ObserverConfig {
    return new ObserverConfig(session, yaml);
  }
}

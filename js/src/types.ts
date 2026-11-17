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

/** A field value: `boolean` for `bool` fields, `number` for every other dtype. */
export type FieldValue = number | boolean;

/** Field name to value, per the declared schema. */
export type Values = Record<string, FieldValue>;

export type Role = 'robot' | 'operator' | 'observer';

export type DType = 'f64' | 'f32' | 'i32' | 'i16' | 'i8' | 'u32' | 'u16' | 'u8' | 'bool';

export type Codec = 'h264' | 'vp8' | 'vp9' | 'av1' | 'h265' | 'raw' | 'png' | 'mjpeg';

/**
 * Where an observation's frame came from: a live match, a reused earlier
 * frame, or a placeholder for a silent track. Always `live` outside of
 * observations.
 */
export type FrameSource = 'live' | 'stale' | 'omitted';

/**
 * One received video frame, stamped with the robot's capture time.
 *
 * WebRTC tracks deliver a browser `VideoFrame` in `frame`. Portal owns it:
 * it stays open while the callback runs and for the next few frames on the
 * track, then Portal closes it. Call `frame.clone()` to keep it longer, and
 * close the clone yourself.
 *
 * RAW, PNG and MJPEG tracks, and placeholders, deliver RGB24 pixels in `data`.
 */
export interface VideoFrameData {
  width: number;
  height: number;
  timestampUs: number;
  source: FrameSource;
  frame?: VideoFrame;
  data?: Uint8Array;
}

export interface Action {
  values: Values;
  /** The same payload as plain numbers. */
  rawValues: Record<string, number>;
  timestampUs: number;
  /** Timestamp of the observation the sender produced this action from. */
  inReplyToTsUs: number | null;
  sender: string;
  /** Whether the sender was the active operator when the robot received it. */
  active: boolean;
}

export interface State {
  values: Values;
  rawValues: Record<string, number>;
  timestampUs: number;
}

export interface Telemetry {
  values: Values;
  rawValues: Record<string, number>;
  timestampUs: number;
  sender: string;
}

/** A state matched with the frame of every video track captured with it. */
export interface Observation {
  state: Values;
  rawState: Record<string, number>;
  frames: Record<string, VideoFrameData>;
  timestampUs: number;
}

/** An annotation. Portal doesn't interpret `type` or `payload`. */
export interface Keypoint {
  type: string;
  payload: Record<string, unknown>;
  timestampUs: number;
  sender: string;
}

export interface RpcInvocationData {
  requestId: string;
  callerIdentity: string;
  payload: string;
  responseTimeout: number;
}

export type RpcHandler = (data: RpcInvocationData) => Promise<string>;

export interface PortalMetrics {
  sync: {
    observationsEmitted: number;
    staleObservationsEmitted: number;
    statesDropped: number;
    framesOmitted: Record<string, number>;
    matchDeltaUsP50: number | null;
    matchDeltaUsP95: number | null;
    lastBlockerTrack: string | null;
  };
  transport: {
    framesSent: Record<string, number>;
    framesReceived: Record<string, number>;
    framesDroppedPublisherFull: Record<string, number>;
    bytesSent: Record<string, number>;
    bytesReceived: Record<string, number>;
    statesSent: number;
    statesReceived: number;
    actionsSent: number;
    actionsReceived: number;
    actionsDroppedStale: number;
    telemetrySent: number;
    telemetryReceived: number;
    frameJitterUs: Record<string, number>;
    stateJitterUs: number;
    actionJitterUs: number;
  };
  buffers: {
    videoFill: Record<string, number>;
    stateFill: number;
    evictions: Record<string, number>;
  };
  rtt: {
    rttUsLast: number | null;
    rttUsMean: number | null;
    rttUsP95: number | null;
    pingsSent: number;
    pongsReceived: number;
  };
  timeSync: {
    source: 'portal' | 'system';
    synced: boolean;
    offsetUs: number;
    uncertaintyUs: number | null;
    measuredOffsetUs: number | null;
    resyncs: number;
    samplesRejected: number;
  };
  policy: {
    e2eUsP50: number | null;
    e2eUsP95: number | null;
    correlatedReceived: number;
  };
}

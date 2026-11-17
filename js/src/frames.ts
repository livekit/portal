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

import type { RemoteVideoTrack } from 'livekit-client';

/**
 * Browser `VideoFrame`s the core refers to by handle. Each track keeps its
 * newest `keep` frames open, which covers every frame the sync buffer can
 * still match, and closes older ones.
 */
export class FrameStore {
  private nextId = 1;
  private readonly frames = new Map<number, VideoFrame>();
  private readonly perTrack = new Map<string, number[]>();

  constructor(private readonly keep: number) {}

  add(track: string, frame: VideoFrame): number {
    const id = this.nextId++;
    this.frames.set(id, frame);
    const ids = this.perTrack.get(track) ?? [];
    ids.push(id);
    while (ids.length > this.keep) {
      const old = ids.shift()!;
      this.frames.get(old)?.close();
      this.frames.delete(old);
    }
    this.perTrack.set(track, ids);
    return id;
  }

  get(id: number): VideoFrame | undefined {
    return this.frames.get(id);
  }

  clear(): void {
    this.frames.forEach((f) => f.close());
    this.frames.clear();
    this.perTrack.clear();
  }
}

export type OnFrame = (frame: VideoFrame, timestampUs: number) => void;

declare class MediaStreamTrackProcessor {
  constructor(init: { track: MediaStreamTrack });
  readable: ReadableStream<VideoFrame>;
}

/**
 * Pulls decoded frames off a subscribed WebRTC track, each stamped with the
 * robot's capture time from its packet trailer. Frames without one are
 * dropped: they cannot be placed on the robot's timeline.
 *
 * Reads frames as they leave the decoder where the browser allows it
 * (`MediaStreamTrackProcessor`), else as a hidden `<video>` presents them,
 * which adds the render path's delay (~20 ms).
 */
export class VideoReceiver {
  private stopped = false;
  private warned = false;
  private element?: HTMLVideoElement;

  constructor(
    private readonly track: RemoteVideoTrack,
    private readonly onFrame: OnFrame,
  ) {
    // Hand frames over as soon as they decode instead of smoothing playout.
    const receiver = track.receiver as (RTCRtpReceiver & { jitterBufferTarget?: number }) | undefined;
    if (receiver && 'jitterBufferTarget' in receiver) receiver.jitterBufferTarget = 0;
    if ('MediaStreamTrackProcessor' in globalThis) void this.pull();
    else this.present();
  }

  stop(): void {
    this.stopped = true;
    if (this.element) {
      this.track.detach(this.element);
      this.element.remove();
    }
  }

  private async pull(): Promise<void> {
    const { readable } = new MediaStreamTrackProcessor({ track: this.track.mediaStreamTrack });
    const reader = readable.getReader();
    while (!this.stopped) {
      const { value: frame, done } = await reader.read();
      if (done) return;
      const { rtpTimestamp } = (frame as VideoFrame & { metadata(): { rtpTimestamp?: number } }).metadata();
      const timestampUs = this.timestampUs(rtpTimestamp);
      if (timestampUs === undefined) frame.close();
      else this.onFrame(frame, timestampUs);
    }
    await reader.cancel();
  }

  private present(): void {
    const el = (this.element = document.createElement('video'));
    el.muted = true;
    el.playsInline = true;
    // Some browsers stop decoding video that isn't in the document.
    el.style.cssText = 'position:fixed;width:1px;height:1px;opacity:0;pointer-events:none';
    document.body.append(el);
    this.track.attach(el);
    const tick = (_now: number, meta: VideoFrameCallbackMetadata): void => {
      if (this.stopped) return;
      const timestampUs = this.timestampUs(meta.rtpTimestamp);
      if (timestampUs !== undefined) {
        this.onFrame(new VideoFrame(el, { timestamp: timestampUs }), timestampUs);
      }
      el.requestVideoFrameCallback(tick);
    };
    el.requestVideoFrameCallback(tick);
  }

  private timestampUs(rtpTimestamp: number | undefined): number | undefined {
    const meta =
      rtpTimestamp === undefined ? undefined : this.track.lookupFrameMetadata({ rtpTimestamp });
    if (meta && meta.userTimestamp > 0n) return Number(meta.userTimestamp);
    if (!this.warned) {
      this.warned = true;
      console.warn('[livekit-portal] video frame without a Portal timestamp; dropping');
    }
    return undefined;
  }
}

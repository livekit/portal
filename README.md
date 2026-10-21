<picture>
  <source media="(prefers-color-scheme: dark)" srcset="/.github/banner_dark.png">
  <source media="(prefers-color-scheme: light)" srcset="/.github/banner_light.png">
  <img style="width:100%;" alt="The LiveKit icon, the name of the repository and some sample code in the background." src="https://raw.githubusercontent.com/livekit/portal/main/.github/banner_light.png">
</picture>

<h1 align="center">LiveKit Portal</h1>

<p align="center">
  <a href="https://github.com/livekit/portal/actions/workflows/tests.yml"><img src="https://github.com/livekit/portal/actions/workflows/tests.yml/badge.svg?branch=main" alt="tests"></a>
  <a href="https://pypi.org/project/livekit-portal/"><img src="https://img.shields.io/pypi/v/livekit-portal" alt="PyPI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="License"></a>
  <a href="https://www.python.org/downloads/"><img src="https://img.shields.io/badge/python-3.10%2B-blue" alt="Python 3.10+"></a>
</p>

<!--BEGIN_DESCRIPTION-->
<p align="center"><b>Teleoperate robots, run remote policies, and record demonstrations with LiveKit Portal.</b></p>
<!--END_DESCRIPTION-->

<p align="center">
  <a href="docs/01-quickstart.md">Quickstart</a> ·
  <a href="docs/">Documentation</a> ·
  <a href="#examples">Examples</a>
</p>

LiveKit Portal connects robots to remote operators over the internet. It carries
camera frames and robot state to the operator, and control actions back to the
robot. It works with any robotics stack, using LiveKit Cloud or a self-hosted
LiveKit server.

## What Portal adds

- **Control arbitration.** Keep policies and human operators in the same session.
  The robot accepts actions from one active operator; transfer control with one
  call or let a supervisor coordinate handoffs. [Control handoff](docs/04-arbitration.md).
- **Synchronized observations.** Receive camera frames and joint state together,
  matched by capture timestamp, so a remote policy can compute its next action.
  [Observations](docs/02-concepts.md#the-observation-model).
- **Shared robot time.** Participants synchronize to the robot's clock, giving
  actions, recordings, and annotations a common timeline across machines.
  [Time sync](docs/05-portal-api.md#time-sync).
- **Data recording sinks.** Add an observer to record alongside any operator.
  Write Rerun archives with `RrdSink`, or implement a sink for your own storage
  format. [Data recording](docs/03-data-recording.md).
- **Policy evaluation.** Record inactive policies' actions for comparison, mark
  episodes with annotations, and measure observation-to-action latency.
  [Action subscriptions](docs/05-portal-api.md#operator-side-action-subscription)
  and [metrics](docs/09-metrics.md).
- **Video for models and people.** Use WebRTC video for live viewing, PNG or RAW
  for exact RGB pixels, or MJPEG for independent frame compression.
  [Frame video](docs/07-frame-video.md).

## Why LiveKit Portal

Remote robotics needs video and control data to cross networks with changing
bandwidth, packet loss, and NAT. LiveKit provides WebRTC transport and media
routing without a VPN or manual port forwarding.

Its room model lets a robot, human operators, policies, and recorders share one
session. Portal adds the robotics-specific coordination: matching observations,
synchronizing clocks, selecting who controls the robot, and recording data.
Your application supplies the sensors, policy, and hardware control.

Use the same Portal code with LiveKit Cloud or a self-hosted server. For clients
built with other LiveKit SDKs, such as a browser teleoperation UI, follow the
[wire protocol](docs/reference/wire-protocol.md).

## What you can build

**Teleoperate a robot.** Connect cameras and hardware through `Robot`, then send
commands from a remote `Operator`. Start with the
[quickstart](docs/01-quickstart.md) or the [SO-101 example](examples/python/so101).

**Run a policy in the cloud.** The policy joins as an operator, receives
synchronized observations, and sends actions back. See the
[inference example](examples/python/inference).

**Collect human corrections.** Keep a human connected alongside the policy,
transfer control when needed, and let an observer record both throughout the
session. Follow [control handoff](docs/04-arbitration.md) and
[data recording](docs/03-data-recording.md).

## Get started

```bash
pip install livekit-portal
```

Follow the [quickstart](docs/01-quickstart.md) to connect your first robot and
operator. It includes platform requirements, credentials, and runnable scripts.
For an existing LeRobot application, use the [LeRobot plugins](docs/reference/lerobot.md).

## Examples

| Example | What it demonstrates |
|---|---|
| [Basic](examples/python/basic) | Synthetic video, actions, recording, and control handoff. No hardware required. |
| [Inference](examples/python/inference) | A policy that plans actions and sends one per control tick. |
| [Cloud inference](examples/python/modal-mock-inference) | A policy running on Modal, with latency measurements. |
| [SO-101](examples/python/so101) | A remote leader arm controlling a physical follower. |

For configuration, tuning, and troubleshooting, see the [documentation](docs/).
For methods and callbacks, see the [API reference](docs/05-portal-api.md).

## License

Apache-2.0. See [LICENSE](LICENSE) for details.

<br/><table>

<thead><tr><th colspan="2">LiveKit Ecosystem</th></tr></thead>
<tbody>
<tr><td>Agents SDKs</td><td><a href="https://github.com/livekit/agents">Python</a> · <a href="https://github.com/livekit/agents-js">Node.js</a></td></tr><tr></tr>
<tr><td>LiveKit SDKs</td><td><a href="https://github.com/livekit/client-sdk-js">Browser</a> · <a href="https://github.com/livekit/client-sdk-swift">Swift</a> · <a href="https://github.com/livekit/client-sdk-android">Android</a> · <a href="https://github.com/livekit/client-sdk-flutter">Flutter</a> · <a href="https://github.com/livekit/client-sdk-react-native">React Native</a> · <a href="https://github.com/livekit/rust-sdks">Rust</a> · <a href="https://github.com/livekit/node-sdks">Node.js</a> · <a href="https://github.com/livekit/python-sdks">Python</a> · <a href="https://github.com/livekit/client-sdk-unity">Unity</a> · <a href="https://github.com/livekit/client-sdk-unity-web">Unity (WebGL)</a> · <a href="https://github.com/livekit/client-sdk-esp32">ESP32</a> · <a href="https://github.com/livekit/client-sdk-cpp">C++</a></td></tr><tr></tr>
<tr><td>Starter Apps</td><td><a href="https://github.com/livekit-examples/agent-starter-python">Python Agent</a> · <a href="https://github.com/livekit-examples/agent-starter-node">TypeScript Agent</a> · <a href="https://github.com/livekit-examples/agent-starter-react">React App</a> · <a href="https://github.com/livekit-examples/agent-starter-swift">SwiftUI App</a> · <a href="https://github.com/livekit-examples/agent-starter-android">Android App</a> · <a href="https://github.com/livekit-examples/agent-starter-flutter">Flutter App</a> · <a href="https://github.com/livekit-examples/agent-starter-react-native">React Native App</a> · <a href="https://github.com/livekit-examples/agent-starter-embed">Web Embed</a></td></tr><tr></tr>
<tr><td>UI Components</td><td><a href="https://github.com/livekit/components-js">React</a> · <a href="https://github.com/livekit/components-android">Android Compose</a> · <a href="https://github.com/livekit/components-swift">SwiftUI</a> · <a href="https://github.com/livekit/components-flutter">Flutter</a></td></tr><tr></tr>
<tr><td>Server APIs</td><td><a href="https://github.com/livekit/node-sdks">Node.js</a> · <a href="https://github.com/livekit/server-sdk-go">Golang</a> · <a href="https://github.com/livekit/server-sdk-ruby">Ruby</a> · <a href="https://github.com/livekit/server-sdk-kotlin">Java/Kotlin</a> · <a href="https://github.com/livekit/python-sdks">Python</a> · <a href="https://github.com/livekit/rust-sdks">Rust</a> · <a href="https://github.com/agence104/livekit-server-sdk-php">PHP (community)</a> · <a href="https://github.com/pabloFuente/livekit-server-sdk-dotnet">.NET (community)</a></td></tr><tr></tr>
<tr><td>Resources</td><td><a href="https://docs.livekit.io">Docs</a> · <a href="https://docs.livekit.io/mcp">Docs MCP Server</a> · <a href="https://github.com/livekit/livekit-cli">CLI</a> · <a href="https://cloud.livekit.io">LiveKit Cloud</a></td></tr><tr></tr>
<tr><td>LiveKit Server OSS</td><td><a href="https://github.com/livekit/livekit">LiveKit server</a> · <a href="https://github.com/livekit/egress">Egress</a> · <a href="https://github.com/livekit/ingress">Ingress</a> · <a href="https://github.com/livekit/sip">SIP</a></td></tr><tr></tr>
<tr><td>Community</td><td><a href="https://community.livekit.io">Developer Community</a> · <a href="https://livekit.io/join-slack">Slack</a> · <a href="https://x.com/livekit">X</a> · <a href="https://www.youtube.com/@livekit_io">YouTube</a></td></tr>
</tbody>
</table>

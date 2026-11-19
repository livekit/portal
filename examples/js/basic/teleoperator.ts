// The browser teleoperator for examples/python/basic: takes control of the
// robot, shows its camera and state, and sends actions back.
// Open with ?url=<livekit url>&token=<token> (see README.md).
import { Operator, OperatorConfig } from '@livekit/portal';
import contract from '../../python/basic/portal.yaml?raw';

const params = new URLSearchParams(location.search);
const status = document.getElementById('status')!;

const op = new Operator(OperatorConfig.fromYaml(contract, params.get('room') ?? 'portal-basic'));

// Your teleop device or policy goes here. This one mirrors the state back.
op.onObservation((obs) => {
  op.sendAction(obs.state, { inReplyToTsUs: obs.timestampUs });
  const cam = obs.frames.cam1!;
  status.textContent = `${cam.width}x${cam.height} frame, state ${JSON.stringify(obs.state)}\n${JSON.stringify(op.metrics()?.policy)}`;
});

await op.connect(params.get('url')!, params.get('token')!);
op.videoTrack('cam1')?.attach(document.getElementById('cam1') as HTMLVideoElement);

// The robot ignores everyone until someone takes control.
await op.setActiveOperator(op.localIdentity!);
await op.performRpc('say', 'hello from the browser');

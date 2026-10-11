# Buzz trial ingress

This override belongs to the dedicated k3s cluster on `buzz-trial`. Its main
Ingress serves `buzz-trial.axes.com`. Recheck that scope before applying it.
Do not install it on shared clusters without assessing every HTTPS consumer.

The default Traefik request-body read timeout is 60 seconds. A production
70-second upload failed during transmission at 63.226 seconds, even though the
relay accepts a proof that was valid when the request arrived. This override
allows 120 seconds for HTTPS request headers and body, retaining a finite limit.
It does not extend proof validity at admission, disable authentication, or change
the relay's upload size limits.

Validate the exact installed Traefik version with an isolated slow-body transfer.
After protected-main acceptance, back up the current HelmChart, any
HelmChartConfig, and Traefik Deployment. Use the k3s-supported HelmChartConfig
path; do not edit the generated Traefik Deployment or packaged `traefik.yaml`.
Install this file as
`/var/lib/rancher/k3s/server/manifests/axes-buzz-traefik-config.yaml`.
Wait for the Helm controller and Traefik rollout, check the effective argument,
then verify authenticated file/video uploads with a 60-second proof and a
70-second body, hash readback, unauthenticated rejection, and fleet reconnects.
Traefik activation may briefly reconnect existing WebSockets.

For rollback, restore the prior HelmChartConfig (or remove this override if none
existed), wait for reconciliation, and recheck relay access. This setting does
not change the relay image or database.

References: [k3s Helm customization](https://docs.k3s.io/helm/) and
[Traefik entrypoint timeouts](https://doc.traefik.io/traefik/v3.7/reference/install-configuration/entrypoints/).


## Device pairing

The dedicated stateless chart in [pairing](pairing/) routes Exact /pair to
upstream's pairing service. It uses accepted client source
890c7febb05de56799b41e7acab46898ace097e1, independently of the main relay image.
That source allows the fifth desktop code attempt within the protocol's event
budget. Keep one replica because the ephemeral peers must share memory.

Lint and render the chart, verify the pinned image, then install it as the
buzz-pairing release in namespace buzz. It reuses buzz-trial-tls; it needs
no identity credentials, service account token, database, or persistent volume.
Verify public TLS WebSocket pairing with generated disposable identities,
including first and fifth correct codes. Check / still authenticates through
the main relay and /pair-extra does not reach this service.

Apply [pairing-url.yaml](pairing-url.yaml) to the main buzz Helm release only
with its retained current values and unchanged image. This advertises the
endpoint in NIP-11 and rolls the main relay; retain the prior values/manifests
and verify authenticated history and reconnects. No schema change is involved.
Rollback removes only this pairing release and its URL overlay. Preserve newer
messages and all existing relay/client state.

Server pairing verification does not establish a physical iPhone test. The
observed mobile 0.21 push descriptor is incompatible with this deployment;
background notifications need separate compatible gateway provisioning.

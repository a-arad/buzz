# Buzz trial ingress

This override belongs to the dedicated k3s cluster on `buzz-trial`. Its sole
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

# barme Helm chart

Runs [barme](https://github.com/elroykanye/barme), a content-addressed object
store that speaks S3, on Kubernetes: a single-replica Deployment backed by a
persistent volume, with credentials and the at-rest master key held in a Secret.

## Install

```
helm install barme ./charts/barme
```

With your own credentials and a bigger volume:

```
helm install barme ./charts/barme \
  --set auth.accessKey=my-access \
  --set auth.secretKey=my-secret \
  --set persistence.size=100Gi
```

Other in-cluster workloads then reach the S3 door at
`http://barme:9000` with any standard S3 SDK (region can be anything;
path-style addressing).

## What it deploys

- A **Deployment** (one replica, `Recreate` strategy) running `barmed`, with the
  data directory on a mounted volume at `/data`.
- A **PersistentVolumeClaim** for that data (`persistence.size`).
- A **Secret** holding `BARME_ACCESS_KEY`, `BARME_SECRET_KEY`, and
  `BARME_MASTER_KEY`, wired into the container by reference.
- A **Service** exposing all four doors: S3 (9000), API (7373), CDN (7375),
  console (7374).
- An optional **Ingress** for the S3 door.
- Liveness and readiness probes against `/health` on the API door.

## Single node by design

barme is a single-node store, so the chart pins `replicas: 1` and uses the
`Recreate` strategy (the ReadWriteOnce volume can't be held by two pods at once).
Do not raise the replica count. Durability comes from the volume plus backups,
not from replication.

## Credentials and the master key

barme encrypts access-key secrets at rest with a 32-byte master key and mints an
owner credential on first boot. The chart manages all three values in a Secret:
anything you leave blank under `auth` is generated once on first install and
**preserved across upgrades** (the chart reads the existing Secret back), so an
upgrade never rotates the master key out from under your data.

Read the generated credential back with:

```
kubectl get secret barme -o jsonpath='{.data.BARME_ACCESS_KEY}' | base64 -d; echo
```

Back up the Secret. If you lose `BARME_MASTER_KEY`, stored key secrets can't be
decrypted. To manage the Secret yourself, set `auth.existingSecret` to its name
(it must carry those three keys).

## Ingress

Off by default. When enabled, the chart routes `ingress.s3Host` straight to the
S3 port **with no path rewrite** — an S3 SigV4 signature covers the request path,
so any prefix strip would invalidate every signed request.

```
helm install barme ./charts/barme \
  --set ingress.enabled=true \
  --set ingress.className=nginx \
  --set ingress.s3Host=s3.example.com
```

## Values

| Key | Default | Description |
|-----|---------|-------------|
| `image.repository` | `elroykanye/barme` | Image repository |
| `image.tag` | `""` (chart appVersion) | Image tag |
| `image.pullPolicy` | `IfNotPresent` | Pull policy |
| `auth.accessKey` | `""` (generated) | Owner access key |
| `auth.secretKey` | `""` (generated) | Owner secret key |
| `auth.masterKey` | `""` (generated) | 64 hex chars; at-rest encryption key |
| `auth.existingSecret` | `""` | Use a Secret you manage instead |
| `persistence.enabled` | `true` | Persist the data dir |
| `persistence.size` | `20Gi` | Volume size |
| `persistence.storageClass` | `""` | StorageClass (`""` = default) |
| `persistence.accessMode` | `ReadWriteOnce` | PVC access mode |
| `service.type` | `ClusterIP` | Service type |
| `service.ports.s3` / `.api` / `.cdn` / `.console` | `9000` / `7373` / `7375` / `7374` | Service ports |
| `ingress.enabled` | `false` | Expose the S3 door via Ingress |
| `ingress.className` | `""` | IngressClass |
| `ingress.s3Host` | `""` | Host routed to the S3 door |
| `ingress.tls` | `[]` | Ingress TLS blocks |
| `resources` | requests 50m/128Mi, limit 512Mi | Container resources |
| `extraEnv` | `{}` | Extra env, e.g. `BARME_EMBED_URL` |

## Uninstall

```
helm uninstall barme
```

The PersistentVolumeClaim is left behind on purpose so data survives a
reinstall. Delete it explicitly to reclaim the storage:

```
kubectl delete pvc barme-data
```

# opencargo as a CI sidecar

Run opencargo next to your CI runner as a local pull-through cache for npm.
The runner points `npm_config_registry` at `http://localhost:6789/npm-proxy/`,
opencargo fetches from npmjs.org on the first request and serves the cache
afterwards. Only npm is proxied today; see the main README's known limitations.

## Files

- `sidecar-deployment.yaml`: Kubernetes pod with a `ci-runner` container and an
  `opencargo-cache` sidecar.
- `configmap.yaml`: sidecar configuration (one `proxy` repository towards
  npmjs.org, anonymous reads, SQLite under `/data/db/`).
- `github-actions-example.yaml`: opencargo as a service container.
- `gitlab-ci-example.yaml`: opencargo as a GitLab CI service.

## Kubernetes

```bash
kubectl apply -f configmap.yaml
kubectl apply -f sidecar-deployment.yaml
```

The cache lives in an `emptyDir` and disappears with the pod; use a persistent
volume if you want it to survive between jobs.

## GitHub Actions and GitLab CI

Both examples start opencargo as a service on port 6789 and write an `.npmrc`
that points at it. Nothing else changes in the pipeline.

## Resources

The sidecar requests 25m CPU and 16Mi of memory with limits of 200m and 64Mi;
adjust the `emptyDir` size to your dependency volume.

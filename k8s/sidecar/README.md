# opencargo -- Mode sidecar CI

Utiliser opencargo comme sidecar dans vos pods CI pour cacher les telechargements npm/cargo sur le noeud local.

## Principe

Le sidecar opencargo fonctionne comme un proxy-cache local dans le pod CI :

1. Le runner CI configure son client npm/cargo pour pointer vers `http://localhost:6789`
2. opencargo intercepte les requetes et les redirige vers le registry upstream (npmjs.org, crates.io, etc.)
3. Les packages telecharges sont caches localement sur le noeud
4. Les builds suivants beneficient du cache local

## Gains de performance

| Scenario | Temps `pnpm install` |
|----------|---------------------|
| Premier build (cache vide) | ~45s (identique a sans sidecar) |
| Builds suivants (cache chaud) | ~5-10s |
| Avec lockfile identique | ~2-3s |

## Fichiers

- `sidecar-deployment.yaml` -- Deploiement k8s avec sidecar opencargo
- `configmap.yaml` -- Configuration du sidecar (proxy vers npmjs.org)
- `github-actions-example.yaml` -- Exemple GitHub Actions avec service container
- `gitlab-ci-example.yaml` -- Exemple GitLab CI avec service

## Utilisation k8s

```bash
kubectl apply -f configmap.yaml
kubectl apply -f sidecar-deployment.yaml
```

Le pod CI aura deux containers :
- `ci-runner` : votre runner CI (node, rust, etc.)
- `opencargo-cache` : le sidecar opencargo qui sert de proxy-cache

La variable `npm_config_registry` est automatiquement configuree pour pointer vers le sidecar.

## Utilisation GitHub Actions

Voir `github-actions-example.yaml` pour un exemple complet.

Le principe est d'utiliser un service container Docker qui demarre opencargo sur le port 6789, puis de configurer `.npmrc` pour pointer vers ce service.

## Utilisation GitLab CI

Voir `gitlab-ci-example.yaml` pour un exemple complet.

GitLab CI supporte nativement les services Docker. opencargo est declare comme service et accessible via son hostname.

## Configuration du sidecar

Le sidecar utilise un `ConfigMap` minimaliste :
- Ecoute sur `0.0.0.0:6789`
- Un seul repository de type `proxy` pointant vers npmjs.org
- Lecture anonyme activee (pas d'auth dans le pod CI)
- Base de donnees SQLite locale dans `/data/db/`

Vous pouvez ajouter d'autres repositories proxy (crates.io, etc.) en editant le ConfigMap.

## Ressources

Le sidecar est tres leger :
- **CPU** : 25m request / 200m limit
- **Memoire** : 16Mi request / 64Mi limit
- **Disque** : 2Gi emptyDir (supprime a la fin du pod)
